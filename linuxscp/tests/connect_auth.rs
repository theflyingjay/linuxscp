//! How a connection attempt behaves around interactive prompts, against the
//! local sshd from scripts/test-server.sh: a slow answer must not time out
//! the SFTP handshake, cancelling a prompt must end the attempt, and an ssh
//! that dies must fail fast rather than waiting for a timer.
//!
//! Run with:
//!   cargo test -p linuxscp --test connect_auth -- --ignored --test-threads=1

use std::process::Command;
use std::time::{Duration, Instant};

use linuxscp::ssh;
use linuxscp::types::{AuthMethod, ConnectSpec, Event, PromptKind};

const TEST_DIR: &str = "/tmp/linuxscp-test";
const TEST_PORT: u16 = 2299;
const PASSPHRASE: &str = "slow-but-steady";

/// A passphrase-protected copy of the test key. The server already
/// authorizes it, so ssh gets as far as having to decrypt it — and asks for
/// the passphrase through askpass, the same path a password prompt takes.
fn encrypted_key(name: &str) -> String {
    let dir = format!("{TEST_DIR}/keys");
    std::fs::create_dir_all(&dir).expect("creating key dir");
    let path = format!("{dir}/{name}");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}.pub"));
    std::fs::copy(format!("{TEST_DIR}/home/.ssh/id_ed25519"), &path)
        .expect("test key missing; is the test sshd running?");
    std::fs::copy(
        format!("{TEST_DIR}/home/.ssh/id_ed25519.pub"),
        format!("{path}.pub"),
    )
    .expect("test public key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", PASSPHRASE, "-f", &path])
        .stdout(std::process::Stdio::null())
        .status()
        .expect("running ssh-keygen");
    assert!(status.success(), "ssh-keygen -p failed");
    path
}

/// Connect to the test sshd with only the given key. `-F /dev/null` keeps
/// the harness's unencrypted identity (and the user's own config) out of
/// the picture, so the passphrase prompt is the only way in.
fn key_spec(key: &str) -> ConnectSpec {
    let mut spec = ConnectSpec::new("127.0.0.1");
    spec.port = Some(TEST_PORT);
    spec.auth = AuthMethod::Key;
    spec.identity_file = Some(key.to_owned());
    spec.extra_ssh_args = vec![
        "-F".into(),
        "/dev/null".into(),
        "-o".into(),
        format!("UserKnownHostsFile={TEST_DIR}/home/.ssh/known_hosts"),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    spec
}

/// Answer prompts on a background task: host-key questions with "yes", and
/// secrets with whatever `secret` returns (after its own delay).
fn answer_prompts<F, Fut>(
    events_rx: async_channel::Receiver<Event>,
    secret: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<String>> + Send,
{
    tokio::spawn(async move {
        while let Ok(event) = events_rx.recv().await {
            if let Event::Prompt(prompt) = event {
                let answer = match prompt.kind {
                    PromptKind::YesNo => Some("yes".to_string()),
                    PromptKind::Secret => secret().await,
                };
                let _ = prompt.reply.send(answer);
            }
        }
    })
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn slow_prompt_does_not_time_out_the_handshake() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let key = encrypted_key("slow");
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        // Answer like someone fetching their phone for a Duo push: well past
        // the 10 s that russh-sftp would otherwise give the handshake.
        let answerer = answer_prompts(events_rx, || async {
            tokio::time::sleep(Duration::from_secs(15)).await;
            Some(PASSPHRASE.to_string())
        });

        let conn = ssh::connect::connect(1, key_spec(&key), events_tx)
            .await
            .expect("connect should wait for the slow prompt");
        answerer.abort();
        assert!(!conn.initial_dir.is_empty());
        conn.cancel.cancel();
    });
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn cancelled_prompt_ends_the_attempt() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let key = encrypted_key("cancel");
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let answerer = answer_prompts(events_rx, || async { None });

        let started = Instant::now();
        let err = ssh::connect::connect(2, key_spec(&key), events_tx)
            .await
            .err()
            .expect("a cancelled prompt must fail the connection");
        answerer.abort();
        let message = format!("{err:#}");
        assert!(message.contains("cancelled"), "{message}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}: {message}",
            started.elapsed()
        );
    });
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn ssh_exit_fails_fast() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        // Nothing listens on a port we just released, so ssh exits at once
        // with "Connection refused" — which must surface right away, not
        // after the handshake budget runs out.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let mut spec = ConnectSpec::new("127.0.0.1");
        spec.port = Some(port);
        spec.extra_ssh_args = vec!["-F".into(), "/dev/null".into()];
        let (events_tx, _events_rx) = async_channel::unbounded::<Event>();

        let started = Instant::now();
        let err = ssh::connect::connect(3, spec, events_tx)
            .await
            .err()
            .expect("connecting to a closed port must fail");
        let message = format!("{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}: {message}",
            started.elapsed()
        );
        assert!(
            !message.contains("Timeout"),
            "should report ssh's exit, not a handshake timeout: {message}"
        );
    });
}
