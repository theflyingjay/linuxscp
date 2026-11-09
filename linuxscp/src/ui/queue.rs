//! Bottom transfer queue: one row per job with progress, speed, ETA and
//! pause/resume/cancel controls.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::entry_object::format_size;
use linuxscp::transfers;
use linuxscp::types::{
    SessionId, TransferAction, TransferDirection, TransferId, TransferKind, TransferSnapshot,
    TransferState,
};

/// Names a session for the rows' session labels (see
/// [`QueueView::set_session_names`]).
type SessionNamer = Box<dyn Fn(SessionId) -> Option<String>>;

struct Row {
    root: gtk::Box,
    title: gtk::Label,
    /// Which session the job belongs to, at the right end of the title line.
    session: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    direction: gtk::Image,
    pause_btn: gtk::Button,
    cancel_btn: gtk::Button,
}

/// Shortest the queue can be dragged: about one row.
pub const MIN_HEIGHT: i32 = 56;

pub struct QueueView {
    /// The queue panel. Hidden while there are no jobs; the app docks it
    /// under the file panes in a vertical paned, so its height is whatever
    /// the user dragged the divider to.
    pub root: gtk::Box,
    list: gtk::Box,
    rows: RefCell<HashMap<TransferId, Row>>,
    /// Height the panel last had while shown (or should open at), so it
    /// comes back at the same size and the app can remember it.
    height: Cell<i32>,
    /// Fired whenever a job reaches a terminal state (see `set_on_finished`).
    on_finished: RefCell<Option<Box<dyn Fn()>>>,
    /// Names sessions for the rows' session labels (see `set_session_names`).
    session_names: RefCell<Option<SessionNamer>>,
}

impl QueueView {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(8);
        list.set_margin_end(8);

        // No height cap: the paned divider above the queue decides how
        // tall it is, and the list scrolls within that.
        let scroller = gtk::ScrolledWindow::builder()
            .min_content_height(MIN_HEIGHT)
            .vexpand(true)
            .child(&list)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&scroller);
        root.set_visible(false);

        Rc::new(Self {
            root,
            list,
            rows: RefCell::new(HashMap::new()),
            height: Cell::new(MIN_HEIGHT),
            on_finished: RefCell::new(None),
            session_names: RefCell::new(None),
        })
    }

    /// Height the panel should open at (restored from settings).
    pub fn set_height(&self, height: i32) {
        self.height.set(height.max(MIN_HEIGHT));
    }

    /// The panel's current height while it is shown, otherwise the height
    /// it last had (or was told to open at).
    pub fn height(&self) -> i32 {
        if self.root.is_visible() && self.root.height() > 0 {
            self.root.height()
        } else {
            self.height.get()
        }
    }

    /// Callback fired whenever a transfer reaches any terminal state — Done,
    /// Cancelled or Failed (the app refreshes the panes). Even an interrupted
    /// transfer has usually already changed the destination (and, for moves,
    /// the source), so the views must catch up regardless of how it ended.
    pub fn set_on_finished(&self, cb: impl Fn() + 'static) {
        *self.on_finished.borrow_mut() = Some(Box::new(cb));
    }

    /// Resolver turning a session id into the name shown on its rows (the
    /// app installs one that matches the tab titles). `None` means the
    /// session is unknown — normally already closed — and the row keeps the
    /// last name it had.
    pub fn set_session_names(&self, resolve: impl Fn(SessionId) -> Option<String> + 'static) {
        *self.session_names.borrow_mut() = Some(Box::new(resolve));
    }

    /// Text for a row's session label: "Local" for jobs that never leave
    /// this machine, the session's name otherwise, and "a → b" for copies
    /// between two servers. `None` when any session can't be named (no
    /// resolver, or it has already closed) so the caller can keep the
    /// previous text.
    fn session_text(&self, ids: &[SessionId]) -> Option<String> {
        if ids.is_empty() {
            return Some("Local".to_string());
        }
        let resolve = self.session_names.borrow();
        let resolve = resolve.as_ref()?;
        let names = ids
            .iter()
            .map(|id| resolve(*id))
            .collect::<Option<Vec<_>>>()?;
        Some(names.join(" → "))
    }

    pub fn update(self: &Rc<Self>, snapshot: TransferSnapshot) {
        let mut rows = self.rows.borrow_mut();
        let row = rows.entry(snapshot.id).or_insert_with(|| {
            let row = build_row(self, snapshot.id);
            // Newest job on top, so the most recent transfer/delete is
            // visible without scrolling down the queue.
            self.list.prepend(&row.root);
            self.root.set_visible(true);
            row
        });

        row.title.set_text(&snapshot.title);
        // Re-resolved on every snapshot so a flipped tab-naming preference
        // shows up live; once the session is gone the last name sticks.
        if let Some(text) = self.session_text(&snapshot.sessions) {
            if row.session.text().as_str() != text {
                row.session.set_text(&text);
                row.session.set_tooltip_text(Some(&text));
            }
        }
        // Direction badge: stays on the row for its whole life, including
        // the dismissable history entry, so finished jobs still read as
        // uploads or downloads at a glance.
        match snapshot.direction {
            Some(TransferDirection::Upload) => {
                row.direction.set_icon_name(Some("go-up-symbolic"));
                row.direction.add_css_class("upload");
                row.direction.set_tooltip_text(Some("Upload"));
                row.direction.set_visible(true);
            }
            Some(TransferDirection::Download) => {
                row.direction.set_icon_name(Some("go-down-symbolic"));
                row.direction.add_css_class("download");
                row.direction.set_tooltip_text(Some("Download"));
                row.direction.set_visible(true);
            }
            None => row.direction.set_visible(false),
        }
        let is_op = matches!(
            snapshot.kind,
            TransferKind::Delete | TransferKind::Attributes
        );
        // While the totals are still being counted they're only lower
        // bounds, so a percentage would lurch around and read far too high
        // early on. Pulse an indeterminate bar until the total is known,
        // then switch to an exact fraction (bytes for copies, item counts
        // for mutations, which move no bytes).
        let counting = snapshot.scanning && !snapshot.state.is_terminal();
        if counting {
            row.progress.pulse();
        } else {
            let fraction = if snapshot.total_bytes > 0 {
                snapshot.done_bytes as f64 / snapshot.total_bytes as f64
            } else if snapshot.files_total > 0 {
                snapshot.files_done as f64 / snapshot.files_total as f64
            } else {
                0.0
            };
            row.progress.set_fraction(fraction.clamp(0.0, 1.0));
        }

        let detail = match snapshot.state {
            TransferState::Queued => "Queued".to_string(),
            TransferState::Scanning => format!("Scanning… {} files found", snapshot.files_total),
            TransferState::Running => running_detail(&snapshot),
            TransferState::Paused => "Paused".to_string(),
            TransferState::WaitingConflict => "Waiting for answer…".to_string(),
            TransferState::Done => done_detail(&snapshot),
            TransferState::Failed => snapshot
                .error
                .clone()
                .unwrap_or_else(|| "Failed".to_string()),
            TransferState::Cancelled => "Cancelled".to_string(),
        };
        row.detail.set_text(&detail);

        match snapshot.state {
            TransferState::Paused => {
                row.pause_btn.set_icon_name("media-playback-start-symbolic");
                row.pause_btn.set_tooltip_text(Some("Resume"));
            }
            _ => {
                row.pause_btn.set_icon_name("media-playback-pause-symbolic");
                row.pause_btn.set_tooltip_text(Some("Pause"));
            }
        }
        // Mutations support cancel but not pause.
        row.pause_btn
            .set_visible(!is_op && !snapshot.state.is_terminal());

        if snapshot.state.is_terminal() {
            row.pause_btn.set_visible(false);
            row.cancel_btn.set_icon_name("window-close-symbolic");
            row.cancel_btn.set_tooltip_text(Some("Dismiss"));
            if snapshot.state == TransferState::Failed {
                row.detail.add_css_class("error");
            }
            if snapshot.state == TransferState::Done {
                row.progress.set_fraction(1.0);
                // Completed mutations clear themselves after a moment (the
                // pane refresh is the real receipt); copies stay dismissable
                // by hand, and failures always stick around.
                if is_op {
                    let queue = self.clone();
                    let id = snapshot.id;
                    glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                        queue.dismiss(id);
                    });
                }
            }
            // Terminal states are emitted exactly once, so this fires once
            // per job however it ended: partial results from a cancelled or
            // failed transfer should show up in the panes too.
            if let Some(cb) = self.on_finished.borrow().as_ref() {
                cb();
            }
        }
    }

    fn dismiss(self: &Rc<Self>, id: TransferId) {
        if let Some(row) = self.rows.borrow_mut().remove(&id) {
            self.list.remove(&row.root);
        }
        if self.rows.borrow().is_empty() {
            // Remember the height while the panel is still allocated.
            if self.root.height() > 0 {
                self.height.set(self.root.height());
            }
            self.root.set_visible(false);
        }
    }
}

fn build_row(queue: &Rc<QueueView>, id: TransferId) -> Row {
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.add_css_class("heading");
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);

    // Which session the job belongs to, so rows from several tabs can be
    // told apart: plain dim caption text like the panes' connection chip.
    // Capped so a long host name trims itself (full text in the tooltip)
    // rather than squeezing the title. "Remote" only shows if the session
    // can't be named at all, which `update` normally replaces at once.
    let session = gtk::Label::new(Some("Remote"));
    session.set_xalign(1.0);
    session.set_valign(gtk::Align::Center);
    session.add_css_class("caption");
    session.add_css_class("dim-label");
    session.set_ellipsize(gtk::pango::EllipsizeMode::End);
    session.set_max_width_chars(32);

    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_row.append(&title);
    title_row.append(&session);

    let detail = gtk::Label::new(None);
    detail.set_xalign(0.0);
    detail.add_css_class("caption");
    detail.add_css_class("dim-label");
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let progress = gtk::ProgressBar::new();
    progress.set_hexpand(true);
    progress.set_valign(gtk::Align::Center);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text_box.set_hexpand(true);
    text_box.append(&title_row);
    text_box.append(&progress);
    text_box.append(&detail);

    // Which way the bytes cross the local/remote boundary (green ↑ upload,
    // blue ↓ download). Hidden until a snapshot says the job has one.
    let direction = gtk::Image::new();
    direction.add_css_class("transfer-direction");
    direction.set_valign(gtk::Align::Center);
    direction.set_visible(false);

    let pause_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
    pause_btn.add_css_class("flat");
    pause_btn.set_valign(gtk::Align::Center);
    let cancel_btn = gtk::Button::from_icon_name("process-stop-symbolic");
    cancel_btn.add_css_class("flat");
    cancel_btn.set_valign(gtk::Align::Center);
    cancel_btn.set_tooltip_text(Some("Cancel"));

    {
        let queue = queue.clone();
        pause_btn.connect_clicked(move |btn| {
            let resuming = btn.icon_name().is_some_and(|n| n.contains("start"));
            transfers::control(
                id,
                if resuming {
                    TransferAction::Resume
                } else {
                    TransferAction::Pause
                },
            );
            let _ = &queue;
        });
    }
    {
        let queue = queue.clone();
        cancel_btn.connect_clicked(move |btn| {
            let dismissing = btn.icon_name().is_some_and(|n| n.contains("close"));
            if dismissing {
                queue.dismiss(id);
            } else {
                transfers::control(id, TransferAction::Cancel);
            }
        });
    }

    let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    root.append(&text_box);
    root.append(&pause_btn);
    // Keep the badge immediately left of the cancel/dismiss "x".
    root.append(&direction);
    root.append(&cancel_btn);

    Row {
        root,
        title,
        session,
        detail,
        progress,
        direction,
        pause_btn,
        cancel_btn,
    }
}

/// Detail line for a running transfer. While the scan is still in progress
/// the total is partial, so we show what's been done plus a "still counting"
/// hint and withhold the ETA (which would be meaningless). Once the total is
/// known we show an exact "done / total", speed and time remaining.
/// Mutations (delete / attribute changes) report item counts instead.
fn running_detail(snap: &TransferSnapshot) -> String {
    match snap.kind {
        TransferKind::Delete => {
            return format!("{} removed…", snap.files_done);
        }
        TransferKind::Attributes => {
            return if snap.scanning {
                format!("Collecting items… {} changed", snap.files_done)
            } else {
                format!("{} / {} changed", snap.files_done, snap.files_total)
            };
        }
        TransferKind::Copy | TransferKind::Move => {}
    }
    let mut text = if snap.scanning {
        // "+" signals the totals are still climbing.
        format!(
            "{} • {}/{}+ files • counting…",
            format_size(snap.done_bytes),
            snap.files_done,
            snap.files_total,
        )
    } else {
        format!(
            "{} / {} • {}/{} files",
            format_size(snap.done_bytes),
            format_size(snap.total_bytes),
            snap.files_done,
            snap.files_total,
        )
    };

    if snap.speed_bps > 1.0 {
        text.push_str(&format!("  •  {}/s", format_size(snap.speed_bps as u64)));
        // Only estimate time once the total is final; otherwise the remaining
        // byte count is unknown and any ETA would be wrong.
        if !snap.scanning {
            let remaining = snap.total_bytes.saturating_sub(snap.done_bytes);
            let eta = remaining as f64 / snap.speed_bps;
            text.push_str(&format!("  •  {} left", format_eta(eta)));
        }
    }

    if !snap.current_file.is_empty() {
        text.push_str(&format!("  •  {}", snap.current_file));
    }
    text
}

/// Detail line for a finished job, per kind.
fn done_detail(snap: &TransferSnapshot) -> String {
    match snap.kind {
        TransferKind::Delete => format!(
            "Removed {} {}",
            snap.files_done,
            if snap.files_done == 1 {
                "item"
            } else {
                "items"
            }
        ),
        TransferKind::Attributes => format!(
            "Changed {} {}",
            snap.files_done,
            if snap.files_done == 1 {
                "item"
            } else {
                "items"
            }
        ),
        TransferKind::Copy | TransferKind::Move => format!(
            "Done — {} in {} files",
            format_size(snap.total_bytes),
            snap.files_total
        ),
    }
}

fn format_eta(seconds: f64) -> String {
    let s = seconds as u64;
    if s >= 48 * 3600 {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
