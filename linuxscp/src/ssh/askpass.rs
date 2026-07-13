//! Unix-socket server backing the `linuxscp-askpass` helper.
//!
//! Each connection attempt gets its own socket. When OpenSSH needs a
//! password / passphrase / host-key confirmation it runs our helper, which
//! connects here; we forward the prompt to the GTK thread as an
//! [`Event::Prompt`] and relay the answer back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::types::{Event, PromptKind, PromptRequest};

/// One-shot secret used to auto-answer the first password/passphrase prompt
/// for a saved site; later prompts fall through to the UI.
type Preset = Arc<Mutex<Option<String>>>;

pub struct AskpassServer {
    pub sock_path: PathBuf,
    cancel: CancellationToken,
}

impl AskpassServer {
    /// Bind a fresh socket and start accepting helper connections. `preset`
    /// auto-answers the first secret prompt (saved password/passphrase).
    pub fn spawn(
        events: async_channel::Sender<Event>,
        preset: Option<String>,
    ) -> anyhow::Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("linuxscp");
        std::fs::create_dir_all(&dir).context("creating askpass socket dir")?;

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sock_path = dir.join(format!("askpass-{}-{n}.sock", std::process::id()));
        let listener = UnixListener::bind(&sock_path).context("binding askpass socket")?;

        let preset: Preset = Arc::new(Mutex::new(preset));
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        let path_for_task = sock_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_child.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let events = events.clone();
                        let preset = preset.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, events, preset).await {
                                tracing::warn!("askpass client error: {err:#}");
                            }
                        });
                    }
                }
            }
            let _ = std::fs::remove_file(&path_for_task);
        });

        Ok(Self { sock_path, cancel })
    }

    /// Environment variables the spawned ssh process needs.
    pub fn ssh_env(&self) -> anyhow::Result<Vec<(&'static str, String)>> {
        let helper = helper_path().context("locating linuxscp-askpass helper")?;
        Ok(vec![
            ("SSH_ASKPASS", helper.to_string_lossy().into_owned()),
            ("SSH_ASKPASS_REQUIRE", "force".into()),
            (
                "LINUXSCP_ASKPASS_SOCK",
                self.sock_path.to_string_lossy().into_owned(),
            ),
        ])
    }
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// The helper lives next to our own binary in the build tree and in /usr/bin
/// when installed; fall back to PATH.
fn helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        // Also look one level up: test binaries live in target/debug/deps.
        for dir in exe.ancestors().skip(1).take(2) {
            let candidate = dir.join("linuxscp-askpass");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join("linuxscp-askpass"))
        .find(|p| p.is_file())
}

async fn handle_client(
    mut stream: UnixStream,
    events: async_channel::Sender<Event>,
    preset: Preset,
) -> anyhow::Result<()> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len < 64 * 1024, "askpass prompt too large");
    let mut prompt = vec![0u8; len];
    stream.read_exact(&mut prompt).await?;
    let prompt = String::from_utf8_lossy(&prompt).into_owned();

    let kind = classify(&prompt);

    // Auto-answer the first secret prompt with a saved credential, if any.
    // If it's wrong, ssh re-invokes us and the preset is already spent, so we
    // fall through to prompting the user.
    let preset_secret = if kind == PromptKind::Secret {
        preset.lock().unwrap().take()
    } else {
        None
    };
    if let Some(secret) = preset_secret {
        reply(&mut stream, 0, secret.as_bytes()).await?;
        return Ok(());
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    events
        .send(Event::Prompt(PromptRequest {
            prompt,
            kind,
            reply: tx,
        }))
        .await
        .ok()
        .context("UI event channel closed")?;

    let answer = rx.await.unwrap_or(None);
    let (status, payload): (u8, &[u8]) = match (&answer, kind) {
        (Some(_), PromptKind::YesNo) => (1, b""),
        (Some(text), PromptKind::Secret) => (0, text.as_bytes()),
        (None, _) => (2, b""),
    };
    reply(&mut stream, status, payload).await
}

/// Write a status byte + length-prefixed payload back to the helper.
async fn reply(stream: &mut UnixStream, status: u8, payload: &[u8]) -> anyhow::Result<()> {
    stream.write_all(&[status]).await?;
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

fn classify(prompt: &str) -> PromptKind {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("(yes/no") || lower.contains("are you sure") {
        PromptKind::YesNo
    } else {
        PromptKind::Secret
    }
}

/// Remove stale sockets from previous runs (crashes, SIGKILL).
pub fn cleanup_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let pid = name
            .strip_prefix("askpass-")
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<i32>().ok());
        // If the owning process is gone, the socket is stale.
        let stale = pid.is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists());
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
