//! The transfer engine: queued jobs, recursive copies between any two
//! backends, pause/resume/cancel, `.filepart` resumable transfers and
//! conflict resolution via the UI.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use crate::types::{
    Backend, ConflictDecision, ConflictRequest, Event, FsEntry, TransferAction, TransferId,
    TransferRequest, TransferSnapshot, TransferState,
};
use crate::{fsops, sessions};

pub const PART_SUFFIX: &str = ".filepart";
const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ctrl {
    paused: bool,
    cancelled: bool,
}

fn controls() -> &'static Mutex<HashMap<TransferId, watch::Sender<Ctrl>>> {
    static CONTROLS: OnceLock<Mutex<HashMap<TransferId, watch::Sender<Ctrl>>>> = OnceLock::new();
    CONTROLS.get_or_init(Default::default)
}

pub fn control(id: TransferId, action: TransferAction) {
    let map = controls().lock().unwrap();
    if let Some(tx) = map.get(&id) {
        tx.send_modify(|c| match action {
            TransferAction::Pause => c.paused = true,
            TransferAction::Resume => c.paused = false,
            TransferAction::Cancel => c.cancelled = true,
            TransferAction::Dismiss => {}
        });
    }
}

/// Queue a transfer; progress arrives as [`Event::TransferUpdate`]s.
pub fn start(request: TransferRequest, events: async_channel::Sender<Event>) -> TransferId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);

    let (ctrl_tx, ctrl_rx) = watch::channel(Ctrl {
        paused: false,
        cancelled: false,
    });
    controls().lock().unwrap().insert(id, ctrl_tx);

    crate::runtime::runtime().spawn(async move {
        let mut job = Job::new(id, request, events, ctrl_rx);
        let result = job.run().await;
        match result {
            Ok(()) => {}
            Err(err) if job.cancelled() => {
                tracing::info!("transfer {id} cancelled: {err:#}");
                job.finish(TransferState::Cancelled, None).await;
            }
            Err(err) => {
                tracing::warn!("transfer {id} failed: {err:#}");
                job.finish(TransferState::Failed, Some(format!("{err:#}")))
                    .await;
            }
        }
        controls().lock().unwrap().remove(&id);
    });
    id
}

/// One file to copy, produced by the scanner.
struct WorkItem {
    src_path: String,
    dst_final: String,
    size: u64,
}

/// Totals the scanner discovers as it walks, read live by the copy loop so
/// the count and byte total climb while files are already transferring.
/// `done` flips true once the whole tree has been walked, at which point the
/// totals are final and the UI can show a real percentage and ETA.
#[derive(Default)]
struct Discovery {
    files: AtomicU32,
    bytes: AtomicU64,
    done: std::sync::atomic::AtomicBool,
}

struct Job {
    id: TransferId,
    request: TransferRequest,
    events: async_channel::Sender<Event>,
    ctrl: watch::Receiver<Ctrl>,
    snapshot: TransferSnapshot,
    last_emit: Instant,
    /// (bytes_done, at) samples for the speed estimate.
    speed_anchor: (u64, Instant),
    overwrite_all: bool,
    skip_all: bool,
}

impl Job {
    fn new(
        id: TransferId,
        request: TransferRequest,
        events: async_channel::Sender<Event>,
        ctrl: watch::Receiver<Ctrl>,
    ) -> Self {
        let title = if request.items.len() == 1 {
            format!(
                "{} {}",
                if request.move_src { "Move" } else { "Copy" },
                request.items[0].name
            )
        } else {
            format!(
                "{} {} items",
                if request.move_src { "Move" } else { "Copy" },
                request.items.len()
            )
        };
        Self {
            id,
            events,
            ctrl,
            snapshot: TransferSnapshot {
                id,
                title,
                state: TransferState::Queued,
                current_file: String::new(),
                done_bytes: 0,
                total_bytes: 0,
                files_done: 0,
                files_total: 0,
                speed_bps: 0.0,
                scanning: true,
                error: None,
            },
            last_emit: Instant::now() - Duration::from_secs(1),
            speed_anchor: (0, Instant::now()),
            request,
            overwrite_all: false,
            skip_all: false,
        }
    }

    fn cancelled(&self) -> bool {
        self.ctrl.borrow().cancelled
    }

    async fn emit(&mut self, force: bool) {
        if !force && self.last_emit.elapsed() < Duration::from_millis(120) {
            return;
        }
        self.last_emit = Instant::now();
        // Refresh the speed estimate roughly once per second.
        let (anchor_bytes, anchor_at) = self.speed_anchor;
        let elapsed = anchor_at.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let delta = self.snapshot.done_bytes.saturating_sub(anchor_bytes) as f64;
            let inst = delta / elapsed;
            self.snapshot.speed_bps = if self.snapshot.speed_bps == 0.0 {
                inst
            } else {
                0.6 * self.snapshot.speed_bps + 0.4 * inst
            };
            self.speed_anchor = (self.snapshot.done_bytes, Instant::now());
        }
        let _ = self
            .events
            .send(Event::TransferUpdate(self.snapshot.clone()))
            .await;
    }

    async fn finish(&mut self, state: TransferState, error: Option<String>) {
        self.snapshot.state = state;
        self.snapshot.error = error;
        self.emit(true).await;
    }

    /// Block while paused; error out when cancelled.
    async fn checkpoint(&mut self) -> anyhow::Result<()> {
        loop {
            let ctrl = *self.ctrl.borrow();
            if ctrl.cancelled {
                anyhow::bail!("cancelled");
            }
            if !ctrl.paused {
                return Ok(());
            }
            if self.snapshot.state != TransferState::Paused {
                self.snapshot.state = TransferState::Paused;
                self.emit(true).await;
            }
            let mut ctrl_rx = self.ctrl.clone();
            ctrl_rx.changed().await.ok();
        }
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        let src = self.request.src_backend;
        let dst = self.request.dst_backend;

        self.snapshot.state = TransferState::Scanning;
        self.emit(true).await;

        // Scan and copy run concurrently: the scanner walks the tree,
        // creating destination directories and streaming files to copy,
        // while this task copies them as they arrive. The first file starts
        // moving almost immediately instead of after the whole tree is
        // counted, and the file/byte totals climb in parallel.
        let discovery = Arc::new(Discovery::default());
        let (tx, rx) = async_channel::unbounded::<anyhow::Result<WorkItem>>();
        let scanner = {
            let mut scanner = Scanner {
                src,
                dst,
                ctrl: self.ctrl.clone(),
                tx,
                discovery: discovery.clone(),
            };
            let items = self.request.items.clone();
            let dst_dir = self.request.dst_dir.clone();
            crate::runtime::runtime().spawn(async move {
                for item in &items {
                    if let Err(err) = Box::pin(scanner.walk(item, &dst_dir)).await {
                        // Report the failure downstream, then stop.
                        let _ = scanner.tx.send(Err(err)).await;
                        return;
                    }
                }
                // The whole tree is now counted: the totals are final, so the
                // UI can switch from an indeterminate bar to a real
                // percentage and ETA even before copying finishes draining
                // the (possibly large) backlog of discovered files.
                scanner.discovery.done.store(true, Ordering::Relaxed);
                // Dropping `scanner` (and its sender) closes the channel.
            })
        };

        // Copy files as the scanner produces them.
        let mut any_skipped = false;
        let mut running = false;
        loop {
            // Wake periodically while waiting so the discovered totals keep
            // updating even when the scanner is ahead of the copier.
            let msg = loop {
                match tokio::time::timeout(Duration::from_millis(150), rx.recv()).await {
                    Ok(Ok(msg)) => break Some(msg),
                    Ok(Err(_closed)) => break None,
                    Err(_timeout) => {
                        self.checkpoint().await?;
                        self.pull_totals(&discovery);
                        self.emit(false).await;
                    }
                }
            };
            let Some(msg) = msg else { break };
            let item = msg?;
            if !running {
                running = true;
                self.snapshot.state = TransferState::Running;
            }
            self.checkpoint().await?;
            self.pull_totals(&discovery);
            let skipped = self
                .copy_file(src, dst, &item)
                .await
                .with_context(|| format!("copying {}", fsops::file_name(&item.src_path)))?;
            any_skipped |= skipped;
            self.snapshot.files_done += 1;
            self.emit(true).await;
        }

        // The channel closed: the scan finished (or a send failed because we
        // bailed). Reap the scanner and surface a cancel if one landed while
        // we were blocked waiting for the next item.
        let _ = scanner.await;
        self.checkpoint().await?;
        self.pull_totals(&discovery);

        // For moves, delete sources — only when nothing was skipped, so we
        // never remove something that wasn't copied.
        if self.request.move_src {
            if any_skipped {
                tracing::info!("move: keeping sources because some files were skipped");
            } else {
                let items = self.request.items.clone();
                for item in &items {
                    fsops::delete(src, item)
                        .await
                        .with_context(|| format!("removing source {}", item.name))?;
                }
            }
        }

        self.finish(TransferState::Done, None).await;
        Ok(())
    }

    /// Copy the scanner's discovered totals into the snapshot for the UI.
    /// Once the scan is done these are final and the bar/ETA become exact.
    fn pull_totals(&mut self, discovery: &Discovery) {
        self.snapshot.files_total = discovery.files.load(Ordering::Relaxed);
        self.snapshot.total_bytes = discovery.bytes.load(Ordering::Relaxed);
        self.snapshot.scanning = !discovery.done.load(Ordering::Relaxed);
    }

    /// Copy one file honoring conflicts and resume. Returns true if skipped.
    async fn copy_file(
        &mut self,
        src: Backend,
        dst: Backend,
        item: &WorkItem,
    ) -> anyhow::Result<bool> {
        let name = fsops::file_name(&item.dst_final).to_owned();
        self.snapshot.current_file = name.clone();
        self.emit(true).await;

        let part_path = format!("{}{}", item.dst_final, PART_SUFFIX);
        let mut offset: u64 = 0;

        // Leftover partial from an interrupted run: silently continue it.
        match fsops::stat(dst, &part_path).await {
            Ok(part) if part.size < item.size => offset = part.size,
            _ => {}
        }

        // Destination already exists: ask the user.
        let existing = if offset == 0 {
            fsops::stat(dst, &item.dst_final).await.ok()
        } else {
            None
        };
        if let Some(existing) = existing {
            let decision = if self.overwrite_all {
                ConflictDecision::Overwrite
            } else if self.skip_all {
                ConflictDecision::Skip
            } else {
                self.ask_conflict(&name, item.size, existing.size).await?
            };
            match decision {
                ConflictDecision::Overwrite | ConflictDecision::OverwriteAll => {}
                ConflictDecision::Resume => {
                    if existing.size < item.size {
                        // Continue writing the real file from its length.
                        fsops::rename(dst, &item.dst_final, &part_path).await?;
                        offset = existing.size;
                    } else {
                        self.snapshot.done_bytes += item.size;
                        return Ok(true);
                    }
                }
                ConflictDecision::Skip | ConflictDecision::SkipAll => {
                    self.snapshot.done_bytes += item.size;
                    return Ok(true);
                }
                ConflictDecision::CancelJob => anyhow::bail!("cancelled"),
            }
        }

        self.snapshot.done_bytes += offset;

        let mut reader = open_read(src, &item.src_path, offset).await?;
        let mut writer = open_write(dst, &part_path, offset).await?;

        let mut buf = vec![0u8; CHUNK];
        loop {
            self.checkpoint().await?;
            let n = reader.read(&mut buf).await.context("read failed")?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).await.context("write failed")?;
            self.snapshot.done_bytes += n as u64;
            self.emit(false).await;
        }
        writer.shutdown().await.context("finalizing file")?;
        drop(writer);
        drop(reader);

        // Overwrite semantics: replace the destination atomically-ish.
        if fsops::exists(dst, &item.dst_final).await {
            fsops::delete(
                dst,
                &FsEntry {
                    name: name.clone(),
                    path: item.dst_final.clone(),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                    mtime: None,
                    mode: None,
                    owner: None,
                    group: None,
                    link_target: None,
                },
            )
            .await
            .ok();
        }
        fsops::rename(dst, &part_path, &item.dst_final)
            .await
            .context("renaming completed file into place")?;

        // Preserve permissions best-effort.
        let src_mode = fsops::stat(src, &item.src_path)
            .await
            .ok()
            .and_then(|meta| meta.mode);
        if let Some(mode) = src_mode {
            fsops::chmod(dst, &item.dst_final, mode).await.ok();
        }
        Ok(false)
    }

    async fn ask_conflict(
        &mut self,
        file_name: &str,
        src_size: u64,
        dst_size: u64,
    ) -> anyhow::Result<ConflictDecision> {
        self.snapshot.state = TransferState::WaitingConflict;
        self.emit(true).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(Event::Conflict(ConflictRequest {
                transfer_id: self.id,
                file_name: file_name.to_owned(),
                src_size,
                dst_size,
                reply: tx,
            }))
            .await
            .ok()
            .context("UI closed")?;
        let decision = rx.await.unwrap_or(ConflictDecision::CancelJob);
        match decision {
            ConflictDecision::OverwriteAll => self.overwrite_all = true,
            ConflictDecision::SkipAll => self.skip_all = true,
            _ => {}
        }
        self.snapshot.state = TransferState::Running;
        self.emit(true).await;
        Ok(decision)
    }
}

/// Walks the source tree on its own task, creating destination directories
/// and streaming files to copy. Runs concurrently with the copy loop so the
/// transfer and the count happen at the same time.
struct Scanner {
    src: Backend,
    dst: Backend,
    ctrl: watch::Receiver<Ctrl>,
    tx: async_channel::Sender<anyhow::Result<WorkItem>>,
    discovery: Arc<Discovery>,
}

impl Scanner {
    /// Block while paused, and stop when cancelled — so a paused transfer
    /// doesn't keep hitting the remote, and a cancel halts the walk.
    async fn checkpoint(&mut self) -> anyhow::Result<()> {
        loop {
            let ctrl = *self.ctrl.borrow();
            if ctrl.cancelled {
                anyhow::bail!("cancelled");
            }
            if !ctrl.paused {
                return Ok(());
            }
            let mut ctrl_rx = self.ctrl.clone();
            ctrl_rx.changed().await.ok();
        }
    }

    async fn walk(&mut self, entry: &FsEntry, dst_dir: &str) -> anyhow::Result<()> {
        self.checkpoint().await?;
        let dst_path = fsops::join(dst_dir, &entry.name);
        if entry.is_dir {
            // Create the directory before streaming any of its files, so the
            // copier never races ahead of its parent existing.
            if !fsops::exists(self.dst, &dst_path).await {
                fsops::mkdir(self.dst, &dst_path)
                    .await
                    .with_context(|| format!("creating directory {dst_path}"))?;
            }
            let children = fsops::list_dir(self.src, &entry.path)
                .await
                .with_context(|| format!("listing {}", entry.path))?;
            for child in &children {
                Box::pin(self.walk(child, &dst_path)).await?;
            }
        } else {
            self.discovery.files.fetch_add(1, Ordering::Relaxed);
            self.discovery
                .bytes
                .fetch_add(entry.size, Ordering::Relaxed);
            let item = WorkItem {
                src_path: entry.path.clone(),
                dst_final: dst_path,
                size: entry.size,
            };
            // A send error means the copier stopped (bailed or was
            // cancelled); stop walking rather than keep producing.
            if self.tx.send(Ok(item)).await.is_err() {
                anyhow::bail!("copy stopped");
            }
        }
        Ok(())
    }
}

async fn open_read(
    backend: Backend,
    path: &str,
    offset: u64,
) -> anyhow::Result<Box<dyn AsyncRead + Send + Unpin>> {
    match backend {
        Backend::Local => {
            let mut file = tokio::fs::File::open(path)
                .await
                .with_context(|| format!("opening {path}"))?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Box::new(file))
        }
        Backend::Remote(id) => {
            let sftp = sessions::get(id).context("session is closed")?.sftp;
            let mut file = sftp
                .open_with_flags(path.to_owned(), OpenFlags::READ)
                .await
                .with_context(|| format!("opening remote {path}"))?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Box::new(file))
        }
    }
}

async fn open_write(
    backend: Backend,
    path: &str,
    offset: u64,
) -> anyhow::Result<Box<dyn AsyncWrite + Send + Unpin>> {
    match backend {
        Backend::Local => {
            let mut opts = tokio::fs::OpenOptions::new();
            opts.write(true).create(true);
            if offset == 0 {
                opts.truncate(true);
            }
            let mut file = opts
                .open(path)
                .await
                .with_context(|| format!("creating {path}"))?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Box::new(file))
        }
        Backend::Remote(id) => {
            let sftp = sessions::get(id).context("session is closed")?.sftp;
            let flags = if offset == 0 {
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
            } else {
                OpenFlags::WRITE | OpenFlags::CREATE
            };
            let mut file = sftp
                .open_with_flags(path.to_owned(), flags)
                .await
                .with_context(|| format!("creating remote {path}"))?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Box::new(file))
        }
    }
}
