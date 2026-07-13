//! Spawning the system OpenSSH binary and turning it into an SFTP session.
//!
//! Plain connections use the sftp subsystem (`ssh -s host sftp`). Elevated
//! connections (sudo / su -) force a remote pty, drive the password prompt,
//! switch the pty to raw mode and exec sftp-server over it — the same trick
//! WinSCP uses, so root file management works without touching sshd config.

use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::Context;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
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

    let mut cmd = Command::new("ssh");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in askpass.ssh_env()? {
        cmd.env(key, value);
    }
    apply_auth_args(&mut cmd, &spec);
    // Debug/power-user hook: extra ssh arguments for every connection.
    if let Ok(args) = std::env::var("LINUXSCP_SSH_ARGS")
        .map_err(anyhow::Error::from)
        .and_then(|extra| Ok(shell_words::split(&extra)?))
    {
        cmd.args(args);
    }
    for arg in &spec.extra_ssh_args {
        cmd.arg(arg);
    }

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
    let stderr = child.stderr.take().expect("piped stderr");

    // Collect ssh's stderr for error reporting; it is the only place
    // messages like "Permission denied (publickey)" show up.
    let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    {
        let stderr_buf = stderr_buf.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut chunk = [0u8; 4096];
            loop {
                use tokio::io::AsyncReadExt as _;
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buf = stderr_buf.lock().unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        let excess = buf.len().saturating_sub(16 * 1024);
                        if excess > 0 {
                            buf.drain(..excess);
                        }
                    }
                }
            }
        });
    }
    let ssh_error = {
        let stderr_buf = stderr_buf.clone();
        move || {
            let buf = stderr_buf.lock().unwrap();
            let text = String::from_utf8_lossy(&buf);
            let lines: Vec<&str> = text
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.contains("Pseudo-terminal"))
                .collect();
            lines
                .into_iter()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        }
    };

    // For elevated sessions, drive the password prompt until the remote
    // bootstrap reports the pty is raw and sftp-server is about to start.
    let mut stdin = stdin;
    let mut leftover = Vec::new();
    if spec.elevation != Elevation::None {
        match drive_elevation(&spec, &mut stdout, &mut stdin, &mut child, &events).await {
            Ok(rest) => leftover = rest,
            Err(err) => {
                let _ = child.kill().await;
                let detail = ssh_error();
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

    let sftp = match SftpSession::new(stream).await {
        Ok(sftp) => Arc::new(sftp),
        Err(err) => {
            let _ = child.kill().await;
            let detail = ssh_error();
            let base = anyhow::Error::new(err).context("establishing SFTP session");
            return Err(if detail.is_empty() {
                base
            } else {
                base.context(detail)
            });
        }
    };
    sftp.set_timeout(30);

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
                        let reason = {
                            let detail = ssh_error();
                            if detail.is_empty() {
                                format!("ssh exited: {status:?}")
                            } else {
                                detail
                            }
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

/// Translate the spec's connection and auth settings into ssh arguments.
fn apply_auth_args(cmd: &mut Command, spec: &ConnectSpec) {
    if let Some(port) = spec.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
        cmd.arg("-l").arg(user);
    }
    match spec.auth {
        AuthMethod::Agent => {}
        AuthMethod::Password => {
            // Force password/keyboard-interactive so ssh actually prompts
            // (routed to our askpass), rather than silently using a key.
            cmd.arg("-o")
                .arg("PreferredAuthentications=password,keyboard-interactive")
                .arg("-o")
                .arg("PubkeyAuthentication=no")
                .arg("-o")
                .arg("NumberOfPasswordPrompts=3");
        }
        AuthMethod::Key => {
            if let Some(path) = spec.identity_file.as_deref().filter(|p| !p.is_empty()) {
                // IdentitiesOnly stops the agent from offering other keys
                // first, so the chosen PEM is actually the one tried.
                cmd.arg("-i").arg(path).arg("-o").arg("IdentitiesOnly=yes");
            }
        }
    }
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

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Scan bootstrap output, answering password prompts via the UI, until the
/// READY sentinel. Returns bytes read past the sentinel (start of SFTP data).
async fn drive_elevation(
    spec: &ConnectSpec,
    stdout: &mut ChildStdout,
    stdin: &mut ChildStdin,
    child: &mut Child,
    events: &async_channel::Sender<Event>,
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
                anyhow::bail!("ssh exited during elevation ({status:?})");
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
}
