//! The transfer engine: queued jobs, recursive copies between any two
//! backends, pause/resume/cancel, `.filepart` resumable transfers and
//! conflict resolution via the UI.
//!
//! Layout: a `Scanner` task walks the source tree and streams files into a
//! channel while several copy workers drain it concurrently. Small-file
//! throughput over SSH is bound by protocol round-trips (stat/open/close/
//! rename per file), not bandwidth, so overlapping several files at once is
//! what makes a "large folder with many files" fast on a real network.

use std::collections::HashMap;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{self, Poll};
use std::time::{Duration, Instant};

use anyhow::Context;
use russh_sftp::client::fs::File as RemoteFile;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::watch;

use crate::types::{
    Backend, ConflictDecision, ConflictRequest, Event, FsEntry, TransferAction, TransferDirection,
    TransferId, TransferKind, TransferRequest, TransferSnapshot, TransferState,
};
use crate::{fsops, sessions};

pub const PART_SUFFIX: &str = ".filepart";
const CHUNK: usize = 256 * 1024;
/// Files copied concurrently. Requests multiplex over the one SFTP channel,
/// so this hides per-file latency without opening extra connections.
const PARALLEL_FILES: usize = 4;
/// Below this size a file is written straight to its final name: the
/// `.filepart` stage-and-rename (probe stat + delete + rename = three round
/// trips) costs more than re-sending the whole file after an interruption.
const RESUME_THRESHOLD: u64 = 512 * 1024;

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

fn next_id() -> TransferId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn register(id: TransferId) -> watch::Receiver<Ctrl> {
    let (ctrl_tx, ctrl_rx) = watch::channel(Ctrl {
        paused: false,
        cancelled: false,
    });
    controls().lock().unwrap().insert(id, ctrl_tx);
    ctrl_rx
}

/// Queue a transfer; progress arrives as [`Event::TransferUpdate`]s.
pub fn start(request: TransferRequest, events: async_channel::Sender<Event>) -> TransferId {
    let id = next_id();
    let ctrl_rx = register(id);

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

/// Attribute changes requested from the Properties dialog, with owner and
/// group still as names (resolved on the job so the dialog never blocks).
#[derive(Debug, Clone)]
pub struct AttrRequest {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<u32>,
    pub add_x_dirs: bool,
    pub recursive: bool,
}

/// Queue a recursive delete as a watchable job: the row shows the removal
/// count climbing, supports cancel, and the panes refresh when it finishes.
pub fn start_delete(
    backend: Backend,
    entries: Vec<FsEntry>,
    events: async_channel::Sender<Event>,
) -> TransferId {
    let title = if entries.len() == 1 {
        format!("Delete {}", entries[0].name)
    } else {
        format!("Delete {} items", entries.len())
    };
    start_op(
        TransferKind::Delete,
        title,
        events,
        move |progress| async move {
            for entry in &entries {
                fsops::delete_tracked(backend, entry, Some(&progress)).await?;
            }
            Ok(())
        },
    )
}

/// Queue an owner/group/permissions change (optionally recursive) as a
/// watchable job, mirroring how WinSCP reports long chmod runs.
pub fn start_attrs(
    backend: Backend,
    entries: Vec<FsEntry>,
    request: AttrRequest,
    events: async_channel::Sender<Event>,
) -> TransferId {
    let title = if entries.len() == 1 {
        format!("Change {}", entries[0].name)
    } else {
        format!("Change {} items", entries.len())
    };
    start_op(
        TransferKind::Attributes,
        title,
        events,
        move |progress| async move {
            let (uid, gid) = fsops::resolve_ids(backend, request.owner, request.group).await?;
            let changes = fsops::AttrChanges {
                mode: request.mode,
                uid,
                gid,
                add_x_dirs: request.add_x_dirs,
            };
            fsops::apply_attrs(
                backend,
                &entries,
                &changes,
                request.recursive,
                Some(&progress),
            )
            .await?;
            Ok(())
        },
    )
}

/// Shared runner for mutation jobs (delete / attributes): spawns the work
/// with an [`fsops::OpProgress`], relays cancel from the queue controls, and
/// emits snapshots while the counters climb so the UI can watch completion.
fn start_op<F, Fut>(
    kind: TransferKind,
    title: String,
    events: async_channel::Sender<Event>,
    work: F,
) -> TransferId
where
    F: FnOnce(Arc<fsops::OpProgress>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let id = next_id();
    let mut ctrl_rx = register(id);

    crate::runtime::runtime().spawn(async move {
        let progress = Arc::new(fsops::OpProgress::default());
        let snapshot = |state: TransferState, error: Option<String>| {
            let total = progress.total.load(Ordering::Relaxed);
            TransferSnapshot {
                id,
                title: title.clone(),
                kind,
                // Mutations change things in place; nothing moves between
                // the local and remote sides.
                direction: None,
                state,
                current_file: String::new(),
                done_bytes: 0,
                total_bytes: 0,
                files_done: progress.done.load(Ordering::Relaxed) as u32,
                files_total: total as u32,
                speed_bps: 0.0,
                // Totals unknown (deletes never pre-count; attribute jobs
                // publish theirs once the work list is built).
                scanning: total == 0,
                error,
            }
        };
        let emit = |snap: TransferSnapshot| {
            let events = events.clone();
            async move {
                let _ = events.send(Event::TransferUpdate(snap)).await;
            }
        };
        emit(snapshot(TransferState::Running, None)).await;

        let mut task = crate::runtime::runtime().spawn(work(progress.clone()));
        let result = loop {
            tokio::select! {
                res = &mut task => break res,
                _ = tokio::time::sleep(Duration::from_millis(150)) => {
                    if ctrl_rx.borrow_and_update().cancelled {
                        progress
                            .cancelled
                            .store(true, Ordering::Relaxed);
                    }
                    emit(snapshot(TransferState::Running, None)).await;
                }
            }
        };

        let cancelled = ctrl_rx.borrow().cancelled;
        let terminal = match result {
            Ok(Ok(())) => snapshot(TransferState::Done, None),
            Err(join_err) => snapshot(TransferState::Failed, Some(join_err.to_string())),
            Ok(Err(err)) if cancelled => {
                tracing::info!("op {id} cancelled: {err:#}");
                snapshot(TransferState::Cancelled, None)
            }
            Ok(Err(err)) => {
                tracing::warn!("op {id} failed: {err:#}");
                snapshot(TransferState::Failed, Some(format!("{err:#}")))
            }
        };
        emit(terminal).await;
        controls().lock().unwrap().remove(&id);
    });
    id
}

/// One file to copy, produced by the scanner. `mode` rides along from the
/// directory listing so the copier doesn't have to stat the source again.
struct WorkItem {
    src_path: String,
    dst_final: String,
    size: u64,
    mode: Option<u32>,
}

/// Totals the scanner discovers as it walks, read live by the copy loop so
/// the count and byte total climb while files are already transferring.
/// `done` flips true once the whole tree has been walked, at which point the
/// totals are final and the UI can show a real percentage and ETA.
#[derive(Default)]
struct Discovery {
    files: AtomicU32,
    bytes: AtomicU64,
    done: AtomicBool,
}

/// Everything the copy workers and the supervising job share: live counters,
/// the snapshot template, conflict policy and the single emit path (globally
/// rate-limited, so thousands of small files can't flood the UI loop).
struct Shared {
    id: TransferId,
    events: async_channel::Sender<Event>,
    discovery: Arc<Discovery>,
    /// State/title/current-file live here; counters live in the atomics.
    template: Mutex<TransferSnapshot>,
    done_bytes: AtomicU64,
    files_done: AtomicU32,
    /// A worker picked up the first file (Scanning -> Running).
    started: AtomicBool,
    any_skipped: AtomicBool,
    /// A worker failed; siblings drain out instead of starting new files.
    failed: AtomicBool,
    overwrite_all: AtomicBool,
    skip_all: AtomicBool,
    /// Serializes conflict prompts so the user sees one dialog at a time.
    conflict_gate: tokio::sync::Mutex<()>,
    /// Workers currently blocked on a conflict answer.
    waiting_conflicts: AtomicU32,
    /// (anchor_bytes, anchor_time, smoothed bps).
    speed: Mutex<(u64, Instant, f64)>,
    last_emit: Mutex<Instant>,
    /// Terminal snapshot sent; silences any stragglers still winding down.
    finished: AtomicBool,
}

impl Shared {
    /// Snapshot of the live counters, with the speed estimate refreshed
    /// roughly once per second.
    fn snapshot(&self) -> TransferSnapshot {
        let mut t = self.template.lock().unwrap();
        t.files_total = self.discovery.files.load(Ordering::Relaxed);
        t.total_bytes = self.discovery.bytes.load(Ordering::Relaxed);
        t.scanning = !self.discovery.done.load(Ordering::Relaxed);
        t.done_bytes = self.done_bytes.load(Ordering::Relaxed);
        t.files_done = self.files_done.load(Ordering::Relaxed);
        let mut speed = self.speed.lock().unwrap();
        let elapsed = speed.1.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let inst = t.done_bytes.saturating_sub(speed.0) as f64 / elapsed;
            let smoothed = if speed.2 == 0.0 {
                inst
            } else {
                0.6 * speed.2 + 0.4 * inst
            };
            *speed = (t.done_bytes, Instant::now(), smoothed);
        }
        t.speed_bps = speed.2;
        t.clone()
    }

    /// Send a snapshot to the UI. Unforced emits are limited to one per
    /// ~120ms across all workers; nothing is sent after the terminal one.
    async fn emit(&self, force: bool) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut last = self.last_emit.lock().unwrap();
            if !force && last.elapsed() < Duration::from_millis(120) {
                return;
            }
            *last = Instant::now();
        }
        let _ = self
            .events
            .send(Event::TransferUpdate(self.snapshot()))
            .await;
    }

    fn set_state(&self, state: TransferState) {
        self.template.lock().unwrap().state = state;
    }

    fn set_current(&self, name: &str) {
        self.template.lock().unwrap().current_file = name.to_owned();
    }
}

/// Block while paused; error out when cancelled. Shared by the job task,
/// the scanner and every worker (each has its own receiver clone).
async fn checkpoint(ctrl: &mut watch::Receiver<Ctrl>) -> anyhow::Result<()> {
    loop {
        let c = *ctrl.borrow();
        if c.cancelled {
            anyhow::bail!("cancelled");
        }
        if !c.paused {
            return Ok(());
        }
        ctrl.changed().await.ok();
    }
}

struct Job {
    request: TransferRequest,
    ctrl: watch::Receiver<Ctrl>,
    shared: Arc<Shared>,
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
        let direction = match (
            request.src_backend.is_remote(),
            request.dst_backend.is_remote(),
        ) {
            (false, true) => Some(TransferDirection::Upload),
            (true, false) => Some(TransferDirection::Download),
            // local↔local and remote↔remote copies have no direction.
            _ => None,
        };
        let template = TransferSnapshot {
            id,
            title,
            kind: if request.move_src {
                TransferKind::Move
            } else {
                TransferKind::Copy
            },
            direction,
            state: TransferState::Queued,
            current_file: String::new(),
            done_bytes: 0,
            total_bytes: 0,
            files_done: 0,
            files_total: 0,
            speed_bps: 0.0,
            scanning: true,
            error: None,
        };
        let shared = Arc::new(Shared {
            id,
            events,
            discovery: Arc::new(Discovery::default()),
            template: Mutex::new(template),
            done_bytes: AtomicU64::new(0),
            files_done: AtomicU32::new(0),
            started: AtomicBool::new(false),
            any_skipped: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            // Pre-answered conflict policy: skip the dialog entirely.
            overwrite_all: AtomicBool::new(request.overwrite),
            skip_all: AtomicBool::new(false),
            conflict_gate: tokio::sync::Mutex::new(()),
            waiting_conflicts: AtomicU32::new(0),
            speed: Mutex::new((0, Instant::now(), 0.0)),
            last_emit: Mutex::new(Instant::now() - Duration::from_secs(1)),
            finished: AtomicBool::new(false),
        });
        Self {
            request,
            ctrl,
            shared,
        }
    }

    fn cancelled(&self) -> bool {
        self.ctrl.borrow().cancelled
    }

    async fn finish(&mut self, state: TransferState, error: Option<String>) {
        // Flag first so any worker still winding down can't emit a stale
        // running snapshot after the terminal one.
        self.shared.finished.store(true, Ordering::Relaxed);
        {
            let mut t = self.shared.template.lock().unwrap();
            t.state = state;
            t.error = error;
        }
        let _ = self
            .shared
            .events
            .send(Event::TransferUpdate(self.shared.snapshot()))
            .await;
    }

    /// Pause/cancel handling for the supervising task: reflects the paused
    /// state in the UI (workers just block silently on their own receivers).
    async fn checkpoint(&mut self) -> anyhow::Result<()> {
        loop {
            let c = *self.ctrl.borrow();
            if c.cancelled {
                anyhow::bail!("cancelled");
            }
            if !c.paused {
                return Ok(());
            }
            if self.shared.template.lock().unwrap().state != TransferState::Paused {
                self.shared.set_state(TransferState::Paused);
                self.shared.emit(true).await;
            }
            self.ctrl.changed().await.ok();
            let c = *self.ctrl.borrow();
            if !c.paused && !c.cancelled {
                // Resumed: restore the visible state.
                self.shared
                    .set_state(if self.shared.started.load(Ordering::Relaxed) {
                        TransferState::Running
                    } else {
                        TransferState::Scanning
                    });
                self.shared.emit(true).await;
            }
        }
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        let src = self.request.src_backend;
        let dst = self.request.dst_backend;

        self.shared.set_state(TransferState::Scanning);
        self.shared.emit(true).await;

        // Scan and copy run concurrently: the scanner walks the tree,
        // creating destination directories and streaming files to copy,
        // while the workers copy them as they arrive. The first file starts
        // moving almost immediately and the totals climb in parallel.
        let (tx, rx) = async_channel::unbounded::<anyhow::Result<WorkItem>>();
        let scanner = {
            let mut scanner = Scanner {
                src,
                dst,
                ctrl: self.ctrl.clone(),
                tx,
                discovery: self.shared.discovery.clone(),
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
                // The whole tree is now counted: totals are final, so the UI
                // can switch from the indeterminate bar to a real percentage
                // and ETA even while the copy backlog drains.
                scanner.discovery.done.store(true, Ordering::Relaxed);
                // Dropping `scanner` (and its sender) closes the channel.
            })
        };

        let workers: Vec<_> = (0..PARALLEL_FILES)
            .map(|_| {
                let worker = Worker {
                    src,
                    dst,
                    ctrl: self.ctrl.clone(),
                    rx: rx.clone(),
                    shared: self.shared.clone(),
                };
                crate::runtime::runtime().spawn(worker.run())
            })
            .collect();
        drop(rx);

        // Supervise: keep the UI ticking (totals climb even while workers
        // grind through big files) and honor pause/cancel promptly.
        let mut all = Box::pin(futures::future::join_all(workers));
        loop {
            tokio::select! {
                results = &mut all => {
                    for result in results {
                        result.context("copy worker crashed")??;
                    }
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(120)) => {
                    self.checkpoint().await?;
                    self.shared.emit(false).await;
                }
            }
        }

        let _ = scanner.await;
        self.checkpoint().await?;

        // For moves, delete sources — only when nothing was skipped, so we
        // never remove something that wasn't copied.
        if self.request.move_src {
            if self.shared.any_skipped.load(Ordering::Relaxed) {
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
}

/// One of the concurrent copy loops: pulls files from the scanner's channel
/// until it closes, copying each with conflict/resume handling.
struct Worker {
    src: Backend,
    dst: Backend,
    ctrl: watch::Receiver<Ctrl>,
    rx: async_channel::Receiver<anyhow::Result<WorkItem>>,
    shared: Arc<Shared>,
}

impl Worker {
    async fn run(mut self) -> anyhow::Result<()> {
        loop {
            // A sibling failed: stop starting new files so the job can end.
            if self.shared.failed.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Ok(msg) = self.rx.recv().await else {
                return Ok(()); // channel closed: scan finished and drained
            };
            let item = match msg {
                Ok(item) => item,
                Err(err) => {
                    self.shared.failed.store(true, Ordering::Relaxed);
                    self.rx.close();
                    return Err(err); // scanner failure
                }
            };
            if !self.shared.started.swap(true, Ordering::Relaxed) {
                self.shared.set_state(TransferState::Running);
            }
            checkpoint(&mut self.ctrl).await?;
            match self.copy_file(&item).await {
                Ok(skipped) => {
                    if skipped {
                        self.shared.any_skipped.store(true, Ordering::Relaxed);
                    }
                    let done = self.shared.files_done.fetch_add(1, Ordering::Relaxed) + 1;
                    // Force the first completion out so short transfers still
                    // show life immediately; after that the rate limit rules.
                    self.shared.emit(done == 1).await;
                }
                Err(err) => {
                    self.shared.failed.store(true, Ordering::Relaxed);
                    self.rx.close();
                    return Err(err)
                        .with_context(|| format!("copying {}", fsops::file_name(&item.src_path)));
                }
            }
        }
    }

    /// Copy one file honoring conflicts and resume. Returns true if skipped.
    async fn copy_file(&mut self, item: &WorkItem) -> anyhow::Result<bool> {
        let name = fsops::file_name(&item.dst_final).to_owned();
        self.shared.set_current(&name);
        self.shared.emit(false).await;

        // Small files skip the `.filepart` stage-and-rename dance: over a
        // real link its extra round trips cost more than re-sending the
        // whole file would after an interruption.
        let use_part = item.size >= RESUME_THRESHOLD;
        let part_path = format!("{}{}", item.dst_final, PART_SUFFIX);
        let mut offset: u64 = 0;

        // Leftover partial from an interrupted run: silently continue it.
        if use_part {
            match fsops::stat(self.dst, &part_path).await {
                Ok(part) if part.size < item.size => offset = part.size,
                _ => {}
            }
        }

        // Destination already exists: ask the user. Remember the answer to
        // the existence probe so we don't have to re-ask the server later.
        let existing = if offset == 0 {
            fsops::stat(self.dst, &item.dst_final).await.ok()
        } else {
            None
        };
        let mut dst_exists = existing.is_some();
        if let Some(existing) = existing {
            let decision = self
                .decide_conflict(&name, item.size, existing.size)
                .await?;
            match decision {
                ConflictDecision::Overwrite | ConflictDecision::OverwriteAll => {}
                ConflictDecision::Resume => {
                    if existing.size >= item.size {
                        self.shared
                            .done_bytes
                            .fetch_add(item.size, Ordering::Relaxed);
                        return Ok(true);
                    }
                    if use_part {
                        // Continue writing the real file from its length.
                        fsops::rename(self.dst, &item.dst_final, &part_path).await?;
                        offset = existing.size;
                        dst_exists = false;
                    }
                    // For a small file resuming saves nothing: rewrite whole.
                }
                ConflictDecision::Skip | ConflictDecision::SkipAll => {
                    self.shared
                        .done_bytes
                        .fetch_add(item.size, Ordering::Relaxed);
                    return Ok(true);
                }
                ConflictDecision::CancelJob => anyhow::bail!("cancelled"),
            }
        }

        self.shared.done_bytes.fetch_add(offset, Ordering::Relaxed);

        let write_path = if use_part {
            &part_path
        } else {
            &item.dst_final
        };
        let mut reader = open_read(self.src, &item.src_path, offset).await?;
        let mut writer = match open_write(self.dst, write_path, offset).await {
            Ok(writer) => writer,
            Err(err) => {
                reader.close().await;
                return Err(err);
            }
        };

        let mut buf = vec![0u8; CHUNK];
        let copied: anyhow::Result<()> = async {
            loop {
                checkpoint(&mut self.ctrl).await?;
                let n = reader.read(&mut buf).await.context("read failed")?;
                if n == 0 {
                    break;
                }
                writer.write_all(&buf[..n]).await.context("write failed")?;
                self.shared
                    .done_bytes
                    .fetch_add(n as u64, Ordering::Relaxed);
                self.shared.emit(false).await;
            }
            writer.shutdown().await.context("finalizing file")?;
            Ok(())
        }
        .await;
        // Settle both handles before surfacing the outcome (see [`Stream`]);
        // on success the writer was already closed by its shutdown above.
        reader.close().await;
        if copied.is_err() {
            writer.close().await;
        }
        copied?;

        if use_part {
            // Overwrite semantics: the destination existed and the user
            // chose to replace it, so move it out of the rename's way.
            if dst_exists {
                fsops::delete(
                    self.dst,
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
            fsops::rename(self.dst, &part_path, &item.dst_final)
                .await
                .context("renaming completed file into place")?;
        }

        // Preserve permissions best-effort, from the listing we already have.
        if let Some(mode) = item.mode {
            fsops::chmod(self.dst, &item.dst_final, mode).await.ok();
        }
        Ok(false)
    }

    /// Resolve a conflict, serializing prompts across workers and honoring
    /// sticky "apply to all" answers.
    async fn decide_conflict(
        &mut self,
        file_name: &str,
        src_size: u64,
        dst_size: u64,
    ) -> anyhow::Result<ConflictDecision> {
        if self.shared.overwrite_all.load(Ordering::Relaxed) {
            return Ok(ConflictDecision::Overwrite);
        }
        if self.shared.skip_all.load(Ordering::Relaxed) {
            return Ok(ConflictDecision::Skip);
        }
        let _gate = self.shared.conflict_gate.lock().await;
        // A sibling may have answered "all" — or cancelled the whole job —
        // while we waited for the gate; don't prompt the user again.
        if self.shared.overwrite_all.load(Ordering::Relaxed) {
            return Ok(ConflictDecision::Overwrite);
        }
        if self.shared.skip_all.load(Ordering::Relaxed) {
            return Ok(ConflictDecision::Skip);
        }
        if self.shared.failed.load(Ordering::Relaxed) || self.ctrl.borrow().cancelled {
            anyhow::bail!("cancelled");
        }

        self.shared
            .waiting_conflicts
            .fetch_add(1, Ordering::Relaxed);
        self.shared.set_state(TransferState::WaitingConflict);
        self.shared.emit(true).await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = self
            .shared
            .events
            .send(Event::Conflict(ConflictRequest {
                transfer_id: self.shared.id,
                file_name: file_name.to_owned(),
                src_size,
                dst_size,
                reply: tx,
            }))
            .await;
        let decision = if sent.is_ok() {
            rx.await.unwrap_or(ConflictDecision::CancelJob)
        } else {
            ConflictDecision::CancelJob
        };

        if self
            .shared
            .waiting_conflicts
            .fetch_sub(1, Ordering::Relaxed)
            == 1
        {
            self.shared.set_state(TransferState::Running);
            self.shared.emit(true).await;
        }
        match decision {
            ConflictDecision::OverwriteAll => {
                self.shared.overwrite_all.store(true, Ordering::Relaxed)
            }
            ConflictDecision::SkipAll => self.shared.skip_all.store(true, Ordering::Relaxed),
            _ => {}
        }
        Ok(decision)
    }
}

/// Walks the source tree on its own task, creating destination directories
/// and streaming files to copy. Runs concurrently with the copy workers so
/// the transfer and the count happen at the same time.
struct Scanner {
    src: Backend,
    dst: Backend,
    ctrl: watch::Receiver<Ctrl>,
    tx: async_channel::Sender<anyhow::Result<WorkItem>>,
    discovery: Arc<Discovery>,
}

impl Scanner {
    async fn walk(&mut self, entry: &FsEntry, dst_dir: &str) -> anyhow::Result<()> {
        checkpoint(&mut self.ctrl).await?;
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
                mode: entry.mode,
            };
            // A send error means the copiers stopped (bailed or cancelled);
            // stop walking rather than keep producing.
            if self.tx.send(Ok(item)).await.is_err() {
                anyhow::bail!("copy stopped");
            }
        }
        Ok(())
    }
}

/// One endpoint of a copy. A concrete type rather than a boxed trait object
/// so the copy loop can settle handles explicitly: dropping a remote
/// [`RemoteFile`] sends its close packet fire-and-forget, and russh-sftp
/// (2.3.0) never decrements its client-side open-handle counter on that
/// path. Each drop then permanently burns one of the handle slots the
/// server advertised via `limits@openssh.com` (~1000 on stock OpenSSH),
/// after which every open on the session fails with "Limit exceeded:
/// handle limit reached". Only the awaited close performed by `shutdown()`
/// keeps the counter honest.
enum Stream {
    Local(tokio::fs::File),
    Remote(RemoteFile),
}

impl Stream {
    /// Explicitly close a remote handle, waiting for the server's ack so
    /// the session's handle accounting stays correct (see type docs).
    /// Best-effort: a handle that won't settle within the grace period is
    /// dropped as before — one leaked counter slot — so a dead connection
    /// can delay a worker but never wedge it.
    async fn close(&mut self) {
        const GRACE: Duration = Duration::from_secs(30);
        let Stream::Remote(file) = self else {
            return; // local files release their fd on drop
        };
        let settle = async {
            // After a failed or cancelled copy, shutdown surfaces each
            // queued write error before performing the close itself, so it
            // can take one attempt per errored in-flight write (at most
            // russh-sftp's max_concurrent_writes, 8) before the one that
            // closes. More attempts than that aren't going to succeed.
            for _ in 0..10 {
                match file.shutdown().await {
                    Ok(()) => return true,
                    // Session torn down: the handle died with it.
                    Err(err) if err.kind() == io::ErrorKind::BrokenPipe => return true,
                    Err(_) => {}
                }
            }
            false
        };
        if !matches!(tokio::time::timeout(GRACE, settle).await, Ok(true)) {
            tracing::warn!("remote file failed to close; leaking a handle slot");
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Local(f) => Pin::new(f).poll_read(cx, buf),
            Stream::Remote(f) => Pin::new(f).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Stream::Local(f) => Pin::new(f).poll_write(cx, buf),
            Stream::Remote(f) => Pin::new(f).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Local(f) => Pin::new(f).poll_flush(cx),
            Stream::Remote(f) => Pin::new(f).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Local(f) => Pin::new(f).poll_shutdown(cx),
            Stream::Remote(f) => Pin::new(f).poll_shutdown(cx),
        }
    }
}

async fn open_read(backend: Backend, path: &str, offset: u64) -> anyhow::Result<Stream> {
    match backend {
        Backend::Local => {
            let mut file = tokio::fs::File::open(path)
                .await
                .with_context(|| format!("opening {path}"))?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Stream::Local(file))
        }
        Backend::Remote(id) => {
            let sftp = sessions::get(id).context("session is closed")?.sftp;
            let mut file = sftp
                .open_with_flags(path.to_owned(), OpenFlags::READ)
                .await
                .with_context(|| format!("opening remote {path}"))?;
            if offset > 0 {
                // Never touches the server for SeekFrom::Start, so this
                // can't fail and leak the just-opened handle.
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok(Stream::Remote(file))
        }
    }
}

async fn open_write(backend: Backend, path: &str, offset: u64) -> anyhow::Result<Stream> {
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
            Ok(Stream::Local(file))
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
            Ok(Stream::Remote(file))
        }
    }
}
