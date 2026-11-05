//! The PuTTY import path end to end: a `.ppk` fixture is converted exactly
//! as the Site Manager does it, and the converted key then logs into the
//! local sshd from scripts/test-server.sh through the normal connect path,
//! with the passphrase auto-answered the way a saved site's would be.
//!
//! Run with:
//!   ./scripts/test-server.sh start
//!   cargo test -p linuxscp --test ppk_import -- --ignored --test-threads=1

use std::io::Write as _;
use std::path::{Path, PathBuf};

use linuxscp::ssh;
use linuxscp::ssh::key_import::{ImportRequest, import_into};
use linuxscp::types::{AuthMethod, ConnectSpec, Event, PromptKind, SessionId};
use zeroize::Zeroizing;

const TEST_DIR: &str = "/tmp/linuxscp-test";
const TEST_PORT: u16 = 2299;
/// Matches src/ssh/testdata/ppk/generate.sh.
const PASSPHRASE: &str = "linuxscp-test-passphrase";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ssh/testdata/ppk")
}

/// Authorize the fixture's public half on the test sshd. sshd re-reads the
/// file on every attempt, so no restart is needed; `StrictModes no` means
/// its permissions don't matter either.
fn authorize(kind: &str) {
    let line = std::fs::read_to_string(fixtures().join(format!("{kind}.openssh.pub")))
        .expect("fixture public key");
    let path = format!("{TEST_DIR}/sshd/authorized_keys");
    let current = std::fs::read_to_string(&path)
        .expect("test sshd missing; run scripts/test-server.sh start");
    if current.contains(line.trim()) {
        return;
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opening authorized_keys");
    if !current.is_empty() && !current.ends_with('\n') {
        file.write_all(b"\n").unwrap();
    }
    file.write_all(line.as_bytes()).unwrap();
}

/// Convert `ppk` (encrypted, keeping the passphrase) into a scratch
/// directory and connect with it as a saved site would: the passphrase is
/// preset, so ssh's prompt is answered without the UI, and a second prompt
/// would mean the key or the passphrase were wrong.
fn import_and_log_in(id: SessionId, kind: &str, ppk: &str) {
    authorize(kind);
    // Stands in for ~/.ssh; the importer creates it (0700) but, like for
    // the real thing, expects the parent to exist.
    let dir = PathBuf::from(format!("{TEST_DIR}/ppk-import/{kind}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    let request = ImportRequest {
        source: fixtures().join(ppk),
        passphrase: Some(Zeroizing::new(PASSPHRASE.to_string())),
        dest_name: "imported".into(),
        protect_with_passphrase: true,
    };
    let imported = import_into(&request, &dir).expect("importing the fixture");
    assert!(imported.passphrase_kept);
    assert!(imported.public_path.is_file());

    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let mut spec = ConnectSpec::new("127.0.0.1");
        spec.port = Some(TEST_PORT);
        spec.auth = AuthMethod::Key;
        spec.identity_file = Some(imported.private_path.to_string_lossy().into_owned());
        spec.secret = Some(PASSPHRASE.to_string());
        spec.extra_ssh_args = vec![
            "-F".into(),
            "/dev/null".into(),
            "-o".into(),
            format!("UserKnownHostsFile={TEST_DIR}/home/.ssh/known_hosts"),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
        ];
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let answerer = tokio::spawn(async move {
            while let Ok(event) = events_rx.recv().await {
                if let Event::Prompt(prompt) = event {
                    let answer = match prompt.kind {
                        PromptKind::YesNo => Some("yes".to_string()),
                        // The preset passphrase answers the first secret
                        // prompt; any other one is a failure, so cancel.
                        PromptKind::Secret => None,
                    };
                    let _ = prompt.reply.send(answer);
                }
            }
        });

        let conn = ssh::connect::connect(id, spec, events_tx)
            .await
            .unwrap_or_else(|err| panic!("{ppk}: converted key should log in: {err:#}"));
        answerer.abort();
        assert!(!conn.initial_dir.is_empty());
        conn.cancel.cancel();
    });
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn imported_ed25519_key_logs_in() {
    import_and_log_in(21, "ed25519", "ed25519-v3-aes.ppk");
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn imported_rsa_key_logs_in() {
    // A v2 file: the format WinSCP installs older than 2021 wrote.
    import_and_log_in(22, "rsa", "rsa-v2-aes.ppk");
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn imported_ecdsa_key_logs_in() {
    import_and_log_in(23, "ecdsa256", "ecdsa256-v3-aes.ppk");
}

/// Selecting the `.ppk` itself as the identity file is caught before ssh
/// runs, with directions to the converter rather than OpenSSH's "invalid
/// format" followed by a password prompt.
#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn unconverted_ppk_is_refused_with_directions() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let mut spec = ConnectSpec::new("127.0.0.1");
        spec.port = Some(TEST_PORT);
        spec.auth = AuthMethod::Key;
        spec.identity_file = Some(
            fixtures()
                .join("ed25519-v3-aes.ppk")
                .to_string_lossy()
                .into_owned(),
        );
        spec.extra_ssh_args = vec!["-F".into(), "/dev/null".into()];
        let (events_tx, _events_rx) = async_channel::unbounded::<Event>();
        let err = ssh::connect::connect(24, spec, events_tx)
            .await
            .err()
            .expect("a .ppk identity must be refused");
        let message = format!("{err:#}");
        assert!(
            message.contains("PuTTY") && message.contains("Convert"),
            "{message}"
        );
    });
}
