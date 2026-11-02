//! Spawning the system OpenSSH binary and turning it into an SFTP session.
//!
//! Plain connections use the sftp subsystem (`ssh -s host sftp`). Elevated
//! connections (sudo / su -) force a remote pty, drive the password prompt,
//! switch the pty to raw mode and exec sftp-server over it — the same trick
//! WinSCP uses, so root file management works without touching sshd config.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::Context;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use super::askpass::AskpassServer;
use crate::types::{
    AuthMethod, ConnectSpec, Elevation, Event, PromptKind, PromptRequest, SessionId,
};

/// Well-known sftp-server locations across distros, tried in order.
const SFTP_SERVER_CANDIDATES: &str = "/usr/lib/openssh/sftp-server \
     /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server \
     /usr/libexec/sftp-server /usr/lib/sftp-server";

const READY: &[u8] = b"LSCP_READY\n";
const NOSERVER: &[u8] = b"LSCP_NOSERVER";
const PASS_PROMPT: &[u8] = b"LSCP_PASS:";
const SU_PROMPT: &[u8] = b"Password:";

/// Budget for the SFTP handshake. It cannot complete until ssh has
/// authenticated, and an interactive login — a password typed by hand, a
/// Duo push, a one-time code — takes as long as the user needs, so this is
/// only a backstop: ssh exiting (bad credentials, the server's
/// LoginGraceTime, connection refused) or the user cancelling a prompt ends
/// a doomed attempt right away.
const HANDSHAKE_TIMEOUT_SECS: u64 = 300;
/// Per-request timeout once the session is up.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// How long to wait for ssh's last words on stderr after it exits.
const STDERR_SETTLE: Duration = Duration::from_millis(500);

pub struct Connection {
    pub sftp: Arc<SftpSession>,
    pub initial_dir: String,
    /// Cancelling kills the ssh process and tears the session down.
    pub cancel: CancellationToken,
}

pub async fn connect(
    id: SessionId,
    spec: ConnectSpec,
    events: async_channel::Sender<Event>,
) -> anyhow::Result<Connection> {
    let askpass = AskpassServer::spawn(events.clone(), spec.secret.clone())?;
    let aborted = askpass.aborted();

    let mut cmd = Command::new("ssh");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in askpass.ssh_env()? {
        cmd.env(key, value);
    }
    cmd.args(base_ssh_args(&spec));

    match spec.elevation {
        Elevation::None => {
            cmd.arg("-s").arg(&spec.host).arg("sftp");
        }
        Elevation::Sudo | Elevation::Su => {
            cmd.arg("-tt")
                .arg(&spec.host)
                .arg("--")
                .arg(bootstrap_script(
                    spec.elevation,
                    spec.sftp_server_path.as_deref(),
                ));
        }
    }

    let mut child = cmd
        .spawn()
        .context("spawning ssh (is OpenSSH installed?)")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = StderrTail::spawn(child.stderr.take().expect("piped stderr"));

    // For elevated sessions, drive the password prompt until the remote
    // bootstrap reports the pty is raw and sftp-server is about to start.
    let mut stdin = stdin;
    let mut leftover = Vec::new();
    if spec.elevation != Elevation::None {
        let driven = drive_elevation(
            &spec,
            &mut stdout,
            &mut stdin,
            &mut child,
            &events,
            &aborted,
        )
        .await;
        match driven {
            Ok(rest) => leftover = rest,
            Err(err) => {
                let _ = child.kill().await;
                let detail = stderr.settled().await;
                if detail.is_empty() {
                    return Err(err);
                }
                return Err(err.context(detail));
            }
        }
    }

    let stream = SshStream {
        stdout,
        stdin,
        prefix: leftover,
    };

    // The handshake's INIT sits in ssh's stdin pipe until authentication is
    // done, so rather than trusting a timer, race it against the ways an
    // attempt ends early: ssh exiting, or the user cancelling a prompt.
    let config = russh_sftp::client::Config {
        request_timeout_secs: HANDSHAKE_TIMEOUT_SECS,
        ..Default::default()
    };
    let sftp = tokio::select! {
        result = SftpSession::new_with_config(stream, config) => match result {
            Ok(sftp) => Arc::new(sftp),
            Err(err) => {
                let _ = child.kill().await;
                let detail = stderr.settled().await;
                let base = handshake_error(err, &spec.host);
                return Err(if detail.is_empty() {
                    base
                } else {
                    base.context(detail)
                });
            }
        },
        status = child.wait() => {
            // ssh gave up before sftp-server ever answered: wrong password,
            // LoginGraceTime, unreachable host. Its stderr says which.
            let detail = stderr.settled().await;
            if aborted.is_cancelled() {
                anyhow::bail!("connection cancelled");
            }
            if detail.is_empty() {
                anyhow::bail!(
                    "ssh exited before the SFTP session started ({})",
                    describe_exit(status)
                );
            }
            anyhow::bail!(detail);
        }
        _ = aborted.cancelled() => {
            let _ = child.kill().await;
            anyhow::bail!("connection cancelled");
        }
    };
    sftp.set_timeout(REQUEST_TIMEOUT_SECS);

    let initial_dir = match &spec.remote_dir {
        Some(dir) => sftp
            .canonicalize(dir.clone())
            .await
            .unwrap_or_else(|_| dir.clone()),
        None => sftp
            .canonicalize(".")
            .await
            .context("resolving remote home directory")?,
    };

    // Watch the child: report unexpected death, kill on cancel.
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        let events = events.clone();
        let sftp = sftp.clone();
        tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => {
                    if !cancel.is_cancelled() {
                        let detail = stderr.settled().await;
                        let reason = if detail.is_empty() {
                            format!("ssh exited ({})", describe_exit(status))
                        } else {
                            detail
                        };
                        let _ = events.send(Event::SessionClosed { id, reason }).await;
                    }
                }
                _ = cancel.cancelled() => {
                    let _ = sftp.close().await;
                    let _ = child.kill().await;
                }
            }
            // Keep the askpass server alive as long as the session:
            // ssh may re-prompt (e.g. rekey with ProxyJump chains).
            drop(askpass);
        });
    }

    Ok(Connection {
        sftp,
        initial_dir,
        cancel,
    })
}

/// The ssh arguments that make a connection reach the same account the same
/// way, whatever the session is for: port, login name, auth settings, then
/// the `LINUXSCP_SSH_ARGS` debug hook and the spec's own extra arguments.
/// Everything up to (but not including) the destination host, so callers
/// append `host` and whatever command or subsystem they need.
pub fn base_ssh_args(spec: &ConnectSpec) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(port) = spec.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
        args.push("-l".into());
        args.push(user.into());
    }
    match spec.auth {
        AuthMethod::Agent => {}
        AuthMethod::Password => {
            // Force an interactive login so ssh actually prompts (routed to
            // our askpass) rather than silently using a key. keyboard-
            // interactive goes first, as in OpenSSH's own default order: it
            // is what PAM stacks such as Duo require, and those servers
            // reject plain password auth outright, so trying it first only
            // burned the saved password and produced "Permission denied,
            // please try again". A server offering only password auth is
            // unaffected, since ssh skips methods it doesn't advertise.
            for opt in [
                "PreferredAuthentications=keyboard-interactive,password",
                "PubkeyAuthentication=no",
                "NumberOfPasswordPrompts=3",
            ] {
                args.push("-o".into());
                args.push(opt.into());
            }
        }
        AuthMethod::Key => {
            if let Some(path) = spec.identity_file.as_deref().filter(|p| !p.is_empty()) {
                // IdentitiesOnly stops the agent from offering other keys
                // first, so the chosen PEM is actually the one tried.
                args.push("-i".into());
                args.push(path.into());
                args.push("-o".into());
                args.push("IdentitiesOnly=yes".into());
            }
        }
    }
    // Debug/power-user hook: extra ssh arguments for every connection.
    if let Some(extra) = std::env::var("LINUXSCP_SSH_ARGS")
        .ok()
        .and_then(|extra| shell_words::split(&extra).ok())
    {
        args.extend(extra);
    }
    args.extend(spec.extra_ssh_args.iter().cloned());
    args
}

/// Remote bootstrap for elevated sessions. Runs under `ssh -tt`, so it owns
/// a pty. It locates sftp-server, then elevates; the elevated shell flips
/// the pty to raw mode (making it 8-bit clean), prints a sentinel and execs
/// sftp-server, at which point the pty carries the binary SFTP protocol.
fn bootstrap_script(elevation: Elevation, server_override: Option<&str>) -> String {
    let find_server = match server_override {
        Some(path) => format!("P={}", shell_quote(path)),
        None => format!(
            "P=; for c in {SFTP_SERVER_CANDIDATES}; do [ -x \"$c\" ] && P=\"$c\" && break; done; \
             [ -n \"$P\" ] || P=$(command -v sftp-server) || {{ echo LSCP_NOSERVER; exit 127; }}"
        ),
    };
    // The elevated command is double-quoted so `$P` is expanded by the
    // unprivileged outer shell before sudo/su ever runs; the escaped inner
    // quotes keep paths with spaces intact for the root shell.
    const INNER: &str = r#""stty raw -echo; printf 'LSCP_READY\n'; exec \"$P\"""#;
    match elevation {
        Elevation::Sudo => format!("{find_server}; exec sudo -p LSCP_PASS: sh -c {INNER}"),
        // LC_ALL=C pins su's password prompt to English so we can detect it.
        Elevation::Su => format!("{find_server}; LC_ALL=C exec su -s /bin/sh -c {INNER} - root"),
        Elevation::None => unreachable!(),
    }
}

/// Single-quote `s` for a POSIX shell (also understood by csh and fish, which
/// matters where the remote login shell is not POSIX).
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Turn a failed handshake into something a user can act on.
fn handshake_error(err: russh_sftp::client::error::Error, host: &str) -> anyhow::Error {
    match err {
        russh_sftp::client::error::Error::Timeout => anyhow::anyhow!(
            "no response from the SFTP server on {host} after {HANDSHAKE_TIMEOUT_SECS} seconds"
        ),
        err => anyhow::Error::new(err).context("establishing SFTP session"),
    }
}

fn describe_exit(status: std::io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => status.to_string(),
        Err(err) => format!("wait failed: {err}"),
    }
}

/// ssh's stderr, collected in the background. It is the only place messages
/// like "Permission denied (publickey)" show up, so it is what the user gets
/// to see when an attempt fails.
struct StderrTail {
    buf: Arc<Mutex<Vec<u8>>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl StderrTail {
    fn spawn(mut stderr: ChildStderr) -> Self {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let reader = {
            let buf = buf.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut buf = buf.lock().unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                            let excess = buf.len().saturating_sub(16 * 1024);
                            if excess > 0 {
                                buf.drain(..excess);
                            }
                        }
                    }
                }
            })
        };
        Self {
            buf,
            reader: Some(reader),
        }
    }

    /// The last few meaningful lines, for once ssh has exited (or been
    /// killed). Waits briefly for the reader to drain what ssh wrote on its
    /// way out, so a final "Permission denied" isn't lost to the race
    /// between the exit status arriving and the pipe becoming readable.
    async fn settled(&mut self) -> String {
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(STDERR_SETTLE, reader).await;
        }
        let buf = self.buf.lock().unwrap();
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.contains("Pseudo-terminal"))
            .collect();
        let skip = lines.len().saturating_sub(4);
        lines[skip..].join("\n")
    }
}

/// Scan bootstrap output, answering password prompts via the UI, until the
/// READY sentinel. Returns bytes read past the sentinel (start of SFTP data).
/// `aborted` is the askpass server's signal that the user cancelled ssh's
/// own login prompt, which ends the attempt here too.
async fn drive_elevation(
    spec: &ConnectSpec,
    stdout: &mut ChildStdout,
    stdin: &mut ChildStdin,
    child: &mut Child,
    events: &async_channel::Sender<Event>,
    aborted: &CancellationToken,
) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut asked = 0u32;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);

    loop {
        if let Some(pos) = find(&buf, READY) {
            return Ok(buf.split_off(pos + READY.len()));
        }
        if find(&buf, NOSERVER).is_some() {
            anyhow::bail!(
                "sftp-server binary not found on the remote host; \
                 set an explicit path in the connection settings"
            );
        }
        let wants_password = find(&buf, PASS_PROMPT).is_some() || ends_with_prompt(&buf, SU_PROMPT);
        if wants_password {
            asked += 1;
            if asked > 3 {
                anyhow::bail!("authentication failed (too many attempts)");
            }
            let title = match spec.elevation {
                Elevation::Sudo => format!("sudo password on {}", spec.host),
                _ => format!("root password on {}", spec.host),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            events
                .send(Event::Prompt(PromptRequest {
                    prompt: title,
                    kind: PromptKind::Secret,
                    reply: tx,
                }))
                .await
                .ok()
                .context("UI closed")?;
            let Some(password) = rx.await.unwrap_or(None) else {
                anyhow::bail!("connection cancelled");
            };
            stdin.write_all(password.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            buf.clear();
        }

        let mut chunk = [0u8; 4096];
        let n = tokio::select! {
            n = stdout.read(&mut chunk) => n.context("reading ssh output")?,
            _ = tokio::time::sleep_until(deadline) => {
                anyhow::bail!("timed out waiting for elevation on {}", spec.host)
            }
            status = child.wait() => {
                if aborted.is_cancelled() {
                    anyhow::bail!("connection cancelled");
                }
                anyhow::bail!("ssh exited during elevation ({})", describe_exit(status));
            }
            _ = aborted.cancelled() => {
                anyhow::bail!("connection cancelled");
            }
        };
        if n == 0 {
            anyhow::bail!("connection closed during elevation");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            anyhow::bail!("unexpected output from remote bootstrap");
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// True when the buffer ends with `needle` optionally followed by spaces —
/// how `su` renders "Password: " while waiting for input.
fn ends_with_prompt(buf: &[u8], needle: &[u8]) -> bool {
    let trimmed = buf
        .iter()
        .rposition(|&b| b != b' ' && b != b'\r' && b != b'\n')
        .map(|i| &buf[..=i])
        .unwrap_or(&[]);
    trimmed.ends_with(needle)
}

/// stdout/stdin of the ssh child glued into one duplex stream, with an
/// optional prefix of bytes that were read during elevation but belong to
/// the SFTP protocol.
struct SshStream {
    stdout: ChildStdout,
    stdin: ChildStdin,
    prefix: Vec<u8>,
}

impl AsyncRead for SshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            let rest = self.prefix.split_off(n);
            buf.put_slice(&self.prefix);
            self.prefix = rest;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_scanning() {
        assert_eq!(find(b"xxLSCP_READY\nyy", READY), Some(2));
        assert!(ends_with_prompt(b"\r\nPassword: ", SU_PROMPT));
        assert!(!ends_with_prompt(b"Password: ok\n", SU_PROMPT));
    }

    #[test]
    fn bootstrap_scripts_are_single_line() {
        for elevation in [Elevation::Sudo, Elevation::Su] {
            let script = bootstrap_script(elevation, None);
            assert!(!script.contains('\n'), "{script}");
            assert!(script.contains("LSCP_READY"));
        }
        let with_path = bootstrap_script(Elevation::Sudo, Some("/opt/sftp server"));
        assert!(with_path.contains("'/opt/sftp server'"));
    }

    /// Password logins must try keyboard-interactive before password: PAM
    /// stacks such as Duo only work through the former, and a server that
    /// offers only password auth is unaffected by the order.
    #[test]
    fn password_auth_prefers_keyboard_interactive() {
        let mut spec = ConnectSpec::new("example.edu");
        spec.auth = AuthMethod::Password;
        let args = base_ssh_args(&spec);
        assert!(
            args.contains(&"PreferredAuthentications=keyboard-interactive,password".to_string()),
            "{args:?}"
        );
        assert!(
            args.contains(&"PubkeyAuthentication=no".to_string()),
            "{args:?}"
        );
    }

    /// The exact-sequence tests assume the debug hook is not set in the
    /// environment running them.
    fn debug_hook_is_unset() -> bool {
        std::env::var_os("LINUXSCP_SSH_ARGS").is_none()
    }

    /// Everything that selects the account and the route comes first, the
    /// spec's extra arguments last, and the host is left to the caller.
    #[test]
    fn base_args_cover_port_user_key_and_extras() {
        if !debug_hook_is_unset() {
            return;
        }
        let mut spec = ConnectSpec::new("example.edu");
        spec.port = Some(2222);
        spec.user = Some("alice".into());
        spec.auth = AuthMethod::Key;
        spec.identity_file = Some("/home/alice/.ssh/cluster.pem".into());
        spec.extra_ssh_args = vec!["-J".into(), "bastion".into()];
        let args = base_ssh_args(&spec);
        let expected: Vec<String> = [
            "-p",
            "2222",
            "-l",
            "alice",
            "-i",
            "/home/alice/.ssh/cluster.pem",
            "-o",
            "IdentitiesOnly=yes",
            "-J",
            "bastion",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(args, expected);
        assert!(!args.iter().any(|a| a == "example.edu"));
    }

    /// Agent auth with nothing else set adds no arguments at all, so ssh's
    /// own config and defaults apply untouched.
    #[test]
    fn base_args_are_empty_for_plain_agent_auth() {
        if !debug_hook_is_unset() {
            return;
        }
        let spec = ConnectSpec::new("example.edu");
        assert!(base_ssh_args(&spec).is_empty());
        // A blank user or identity path is treated as unset.
        let mut blank = ConnectSpec::new("example.edu");
        blank.user = Some(String::new());
        blank.auth = AuthMethod::Key;
        blank.identity_file = Some(String::new());
        assert!(base_ssh_args(&blank).is_empty());
    }
}
