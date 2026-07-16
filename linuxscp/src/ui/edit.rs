//! WinSCP-style double-click editing. Files open in the system's default
//! text editor (whatever handles text/plain — even for images, matching
//! WinSCP). Local files open in place; remote files are downloaded to a
//! private temp copy that is watched, and every save is uploaded straight
//! back to the server, so editing a remote config file is just
//! double-click → edit → save.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::entry_object::format_size;
use linuxscp::runtime::runtime;
use linuxscp::types::{
    Backend, Event, FsEntry, SessionId, TransferId, TransferRequest, TransferSnapshot,
    TransferState,
};
use linuxscp::{fsops, transfers};

/// Files bigger than this prompt before opening, WinSCP style: the whole
/// file has to come down (and go back up on every save), and text editors
/// get slow with huge buffers.
const WARN_SIZE: u64 = 10 * 1024 * 1024;

/// Quiet period after the last change event before uploading, so editors
/// that save in several steps (write a temp file, rename it into place)
/// trigger one upload rather than a burst.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// A remote file being edited, keyed by session + full remote path.
type Key = (SessionId, String);

/// Watch state for one remote file's local temp copy.
struct EditSession {
    /// The remote side the saves go back to.
    backend: Backend,
    /// Remote directory the file lives in (upload destination).
    remote_dir: String,
    /// The temp copy handed to the editor.
    local_path: PathBuf,
    /// Keeps the gio watch alive; dropping it would cancel the monitor.
    _monitor: gio::FileMonitor,
    /// Pending debounce timer for change events.
    debounce: RefCell<Option<glib::SourceId>>,
    /// An upload is currently in flight.
    uploading: Cell<bool>,
    /// Saved again while uploading; re-upload when the current one ends.
    dirty: Cell<bool>,
}

/// A queue job started on behalf of the editor, resolved when the app's
/// event loop sees its terminal snapshot.
enum Pending {
    /// Download finishing → start watching and launch the editor.
    Download(PendingDownload),
    /// Upload finishing → release the in-flight flag, flush a dirty save.
    Upload(Key),
}

struct PendingDownload {
    key: Key,
    backend: Backend,
    remote_dir: String,
    local_path: PathBuf,
}

pub struct EditManager {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    events_tx: async_channel::Sender<Event>,
    sessions: RefCell<HashMap<Key, Rc<EditSession>>>,
    pending: RefCell<HashMap<TransferId, Pending>>,
}

impl EditManager {
    pub fn new(
        window: &adw::ApplicationWindow,
        toasts: &adw::ToastOverlay,
        events_tx: async_channel::Sender<Event>,
    ) -> Rc<Self> {
        // Stale copies from previous runs: the app is single-instance and
        // every live edit re-downloads on startup anyway, so clear them
        // rather than let temp copies of remote files pile up on disk.
        std::fs::remove_dir_all(cache_root()).ok();
        Rc::new(Self {
            window: window.clone(),
            toasts: toasts.clone(),
            events_tx,
            sessions: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
        })
    }

    /// Entry point for an activated (double-clicked) file.
    pub fn open(self: &Rc<Self>, backend: Backend, entry: FsEntry) {
        if entry.size > WARN_SIZE {
            let this = self.clone();
            confirm_large(&self.window, &entry, move |entry| {
                this.begin(backend, entry);
            });
        } else {
            self.begin(backend, entry);
        }
    }

    fn begin(self: &Rc<Self>, backend: Backend, entry: FsEntry) {
        match backend {
            // Local saves already land in place; no copy, no watching.
            Backend::Local => self.launch_editor(Path::new(&entry.path)),
            Backend::Remote(id) => self.open_remote(id, entry),
        }
    }

    fn open_remote(self: &Rc<Self>, id: SessionId, entry: FsEntry) {
        let key: Key = (id, entry.path.clone());
        // Already being edited: reopen the existing copy so unsaved changes
        // sitting in the editor aren't clobbered by a fresh download.
        if let Some(session) = self.sessions.borrow().get(&key) {
            self.launch_editor(&session.local_path);
            return;
        }
        // Download already on its way; the editor opens when it lands.
        let downloading = self.pending.borrow().values().any(
            |p| matches!(p, Pending::Download(dl) if dl.key == key),
        );
        if downloading {
            return;
        }

        let remote_dir = fsops::parent(&entry.path);
        let local_dir = session_dir(id, &remote_dir);
        if let Err(err) = std::fs::create_dir_all(&local_dir) {
            self.toast(&format!("Could not create a temp folder: {err}"));
            return;
        }
        let local_path = local_dir.join(&entry.name);
        let job = transfers::start(
            TransferRequest {
                src_backend: Backend::Remote(id),
                dst_backend: Backend::Local,
                items: vec![entry],
                dst_dir: local_dir.to_string_lossy().into_owned(),
                move_src: false,
                // A leftover copy of the same file is not worth a prompt.
                overwrite: true,
            },
            self.events_tx.clone(),
        );
        self.pending.borrow_mut().insert(
            job,
            Pending::Download(PendingDownload {
                key,
                backend: Backend::Remote(id),
                remote_dir,
                local_path,
            }),
        );
    }

    /// Called by the app's event loop for every transfer snapshot; reacts
    /// once per tracked job, when it reaches a terminal state.
    pub fn on_job_terminal(self: &Rc<Self>, snapshot: &TransferSnapshot) {
        if !snapshot.state.is_terminal() {
            return;
        }
        let Some(pending) = self.pending.borrow_mut().remove(&snapshot.id) else {
            return;
        };
        match pending {
            Pending::Download(dl) => {
                // Failures/cancellations already show on the queue row.
                if snapshot.state == TransferState::Done {
                    self.watch_and_open(dl);
                }
            }
            Pending::Upload(key) => {
                let session = self.sessions.borrow().get(&key).cloned();
                if let Some(session) = session {
                    session.uploading.set(false);
                    // A save landed mid-upload: push the newest contents.
                    if session.dirty.take() {
                        self.start_upload(key);
                    }
                }
            }
        }
    }

    /// The temp copy is on disk: watch it for saves and hand it to the
    /// editor.
    fn watch_and_open(self: &Rc<Self>, dl: PendingDownload) {
        let monitor = match gio::File::for_path(&dl.local_path)
            .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(err) => {
                self.toast(&format!("Could not watch the temp copy: {err}"));
                return;
            }
        };

        let session = Rc::new(EditSession {
            backend: dl.backend,
            remote_dir: dl.remote_dir,
            local_path: dl.local_path.clone(),
            _monitor: monitor.clone(),
            debounce: RefCell::new(None),
            uploading: Cell::new(false),
            dirty: Cell::new(false),
        });

        {
            let this = self.clone();
            let key = dl.key.clone();
            let session = session.clone();
            monitor.connect_changed(move |_, _, _, event| {
                use gio::FileMonitorEvent as E;
                // Editors save as plain writes (Changed/ChangesDoneHint) or
                // atomically by renaming a temp file over ours (Created /
                // Renamed / MovedIn with WATCH_MOVES). Ignore the Deleted
                // half of an atomic save and attribute-only changes.
                if !matches!(
                    event,
                    E::Changed | E::ChangesDoneHint | E::Created | E::Renamed | E::MovedIn
                ) {
                    return;
                }
                // Restart the debounce window on every event in the burst.
                if let Some(old) = session.debounce.borrow_mut().take() {
                    old.remove();
                }
                let this = this.clone();
                let key = key.clone();
                let session_for_timer = session.clone();
                let source = glib::timeout_add_local_once(SAVE_DEBOUNCE, move || {
                    // The timer has fired; it must not be removed again.
                    session_for_timer.debounce.borrow_mut().take();
                    if session_for_timer.uploading.get() {
                        session_for_timer.dirty.set(true);
                    } else {
                        this.start_upload(key);
                    }
                });
                *session.debounce.borrow_mut() = Some(source);
            });
        }

        self.sessions.borrow_mut().insert(dl.key, session);
        self.launch_editor(&dl.local_path);
    }

    /// Send the saved temp copy back to the server as a regular queue job
    /// (green up-arrow, progress, error reporting for free).
    fn start_upload(self: &Rc<Self>, key: Key) {
        let Some(session) = self.sessions.borrow().get(&key).cloned() else {
            return;
        };
        session.uploading.set(true);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let local = session.local_path.to_string_lossy().into_owned();
            let stat = runtime()
                .spawn(async move { fsops::stat(Backend::Local, &local).await })
                .await;
            match stat {
                Ok(Ok(entry)) => {
                    let job = transfers::start(
                        TransferRequest {
                            src_backend: Backend::Local,
                            dst_backend: session.backend,
                            items: vec![entry],
                            dst_dir: session.remote_dir.clone(),
                            move_src: false,
                            // The remote file existing is the whole point.
                            overwrite: true,
                        },
                        this.events_tx.clone(),
                    );
                    this.pending.borrow_mut().insert(job, Pending::Upload(key));
                }
                Ok(Err(err)) => {
                    session.uploading.set(false);
                    this.toast(&format!("Could not read the saved file: {err:#}"));
                }
                Err(err) => {
                    session.uploading.set(false);
                    this.toast(&err.to_string());
                }
            }
        });
    }

    fn launch_editor(&self, path: &Path) {
        // Always the text editor — WinSCP opens everything (even images)
        // in the editor on double-click.
        let Some(app) = gio::AppInfo::default_for_type("text/plain", false) else {
            self.toast("No default text editor is configured");
            return;
        };
        let context = gtk::prelude::WidgetExt::display(&self.window).app_launch_context();
        let file = gio::File::for_path(path);
        if let Err(err) = app.launch(&[file], Some(&context)) {
            self.toast(&format!("Could not open the editor: {err}"));
        }
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

/// WinSCP-style size warning before opening a big file.
fn confirm_large(
    parent: &impl IsA<gtk::Widget>,
    entry: &FsEntry,
    on_confirm: impl Fn(FsEntry) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Open Large File?")
        .body(format!(
            "\u{201c}{}\u{201d} is {}. Large files can be slow to open and edit.",
            entry.name,
            format_size(entry.size),
        ))
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("open", "Open Anyway");
    dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
    let entry = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response == "open" {
            on_confirm(entry.clone());
        }
        dialog.close();
    });
    dialog.present(Some(parent));
}

/// Root of all temp copies: `~/.cache/linuxscp/edit`.
fn cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("linuxscp")
        .join("edit")
}

/// Directory for one remote directory's temp copies. The real file name is
/// kept (editors show it and pick syntax highlighting from it); the remote
/// directory is hashed so same-named files from different folders coexist.
fn session_dir(id: SessionId, remote_dir: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    remote_dir.hash(&mut hasher);
    cache_root()
        .join(id.to_string())
        .join(format!("{:016x}", hasher.finish()))
}
