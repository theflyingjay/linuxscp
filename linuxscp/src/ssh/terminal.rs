//! "Open Terminal Here" for remote panes: the command line that puts an
//! interactive ssh login into a terminal emulator, landing in the pane's
//! directory, with the same connection settings as the file session (login
//! name, port, key, auth preferences, extra arguments).
//!
//! Prompts — password, Duo, host key — happen in the terminal itself, as
//! with any ssh command; the GUI askpass is not involved.

use super::connect::{base_ssh_args, shell_quote};
use crate::types::ConnectSpec;

/// The script /bin/sh runs on the remote side, with the directory as `$1`.
const REMOTE_SCRIPT: &str = r#"cd "$1" 2>/dev/null; command -v "$SHELL" >/dev/null 2>&1 || { SHELL=/bin/sh; export SHELL; }; exec "$SHELL" -l"#;

/// Local wrapper around ssh. Status 255 is ssh's own failure code —
/// authentication, connection, host key — and exactly the case where the
/// terminal would otherwise close on the error message, so the window stays
/// open until Enter is pressed. Any other status is the remote shell's own
/// (an `exit` after a failed command, say) and passes straight through.
const HOLD_ON_FAILURE: &str = r#"ssh "$@"; s=$?; [ "$s" -eq 255 ] || exit "$s"; printf '\n[LinuxSCP] ssh failed; press Enter to close this window.'; read -r _"#;

/// What ssh asks the remote side to run: change to `dir` and start the
/// user's login shell there.
///
/// The user's remote login shell parses this line, and it may be csh/tcsh,
/// fish or something else with non-POSIX syntax — common on clusters. So it
/// is only asked to `exec` /bin/sh with a single-quoted script, which means
/// the same thing to all of them (no `$` outside the quotes, no `!`
/// anywhere), and /bin/sh does the real work.
pub fn remote_command(dir: &str) -> String {
    format!("exec /bin/sh -c '{REMOTE_SCRIPT}' sh {}", shell_quote(dir))
}

/// ssh's arguments for an interactive login into `spec`'s account at `dir`.
pub fn ssh_args(spec: &ConnectSpec, dir: &str) -> Vec<String> {
    let mut args = base_ssh_args(spec);
    args.push("-t".into());
    args.push(spec.host.clone());
    args.push("--".into());
    args.push(remote_command(dir));
    args
}

/// The argv a terminal emulator runs: ssh behind the hold-on-failure wrapper.
pub fn command(spec: &ConnectSpec, dir: &str) -> Vec<String> {
    let mut argv: Vec<String> = ["/bin/sh", "-c", HOLD_ON_FAILURE, "linuxscp-ssh"]
        .into_iter()
        .map(String::from)
        .collect();
    argv.extend(ssh_args(spec, dir));
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AuthMethod;

    #[test]
    fn remote_command_is_one_quoted_line() {
        let cmd = remote_command("/srv/it's here");
        assert!(!cmd.contains('\n'), "{cmd}");
        assert!(cmd.starts_with("exec /bin/sh -c '"), "{cmd}");
        assert!(cmd.ends_with(r#" sh '/srv/it'\''s here'"#), "{cmd}");
        // Nothing for a csh login shell to expand before handing over.
        assert!(!cmd.contains('!'), "{cmd}");
    }

    #[test]
    fn ssh_args_carry_the_session_settings() {
        let mut spec = ConnectSpec::new("cluster.example.edu");
        spec.user = Some("alice".into());
        spec.port = Some(2222);
        spec.auth = AuthMethod::Password;
        let args = ssh_args(&spec, "/scratch/alice");
        let pos = |needle: &str| args.iter().position(|a| a == needle).expect(needle);
        assert_eq!(args[pos("-l") + 1], "alice");
        assert_eq!(args[pos("-p") + 1], "2222");
        assert!(args.contains(&"PreferredAuthentications=keyboard-interactive,password".into()));
        // Options first, then the destination, then the remote command.
        let host = pos("cluster.example.edu");
        assert!(host > pos("-t"));
        assert_eq!(args[host + 1], "--");
        assert_eq!(args[host + 2], remote_command("/scratch/alice"));
        assert_eq!(args.len(), host + 3);
    }

    #[test]
    fn command_wraps_ssh_in_the_hold_open_shell() {
        let spec = ConnectSpec::new("host");
        let argv = command(&spec, "/");
        assert_eq!(
            &argv[..4],
            &["/bin/sh", "-c", HOLD_ON_FAILURE, "linuxscp-ssh"]
        );
        assert_eq!(&argv[4..], &ssh_args(&spec, "/")[..]);
    }

    /// Run the remote command through every shell on this machine that a
    /// remote login shell could plausibly be, feeding `pwd` to the shell it
    /// starts, and check each one lands in the (awkwardly named) directory.
    #[test]
    fn remote_command_works_under_non_posix_login_shells() {
        let dir = std::env::temp_dir().join(format!("linuxscp term {}'s dir", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_str().unwrap();
        let cmd = remote_command(dir_str);

        let mut tried = Vec::new();
        for shell in ["sh", "dash", "bash", "zsh", "tcsh", "csh", "fish"] {
            let Some(path) = find_in_path(shell) else {
                continue;
            };
            let output = std::process::Command::new(&path)
                .arg("-c")
                .arg(&cmd)
                .env("SHELL", "/bin/sh")
                .env("ENV", "/dev/null")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child.stdin.take().unwrap().write_all(b"pwd\n")?;
                    child.wait_with_output()
                })
                .unwrap_or_else(|err| panic!("running {shell}: {err}"));
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains(dir_str),
                "{shell}: status {:?}\nstdout: {stdout}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            tried.push(shell);
        }
        std::fs::remove_dir(&dir).unwrap();
        assert!(tried.contains(&"sh"), "no /bin/sh to test with?");
    }

    fn find_in_path(bin: &str) -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|p| p.join(bin))
            .find(|p| p.is_file())
    }
}
