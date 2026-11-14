//! Bottom activity log: one row per job (copies, moves, deletes, attribute
//! changes) showing progress, speed and ETA while it runs, then what
//! happened, how long it took and when it finished. The panel is always
//! shown and rows stay until the user removes them (the row's "x" or the
//! right-click menu), so the record of a session's activity survives the
//! jobs themselves.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::entry_object::format_size;
use linuxscp::tr;
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
    /// Elapsed time while the job runs; "duration • finished at" afterwards.
    timing: gtk::Label,
    progress: gtk::ProgressBar,
    direction: gtk::Image,
    pause_btn: gtk::Button,
    cancel_btn: gtk::Button,
    /// Last state seen, so the context menu knows which rows are finished.
    state: Cell<TransferState>,
}

/// Shortest the queue can be dragged: about one row.
pub const MIN_HEIGHT: i32 = 56;

pub struct QueueView {
    /// The panel. The app docks it under the file panes in a vertical
    /// paned, so its height is whatever the user dragged the divider to.
    pub root: gtk::Box,
    list: gtk::Box,
    /// Placeholder shown while there are no rows.
    empty: gtk::Label,
    rows: RefCell<HashMap<TransferId, Row>>,
    /// Height the panel should open at (restored from settings), used until
    /// it has been allocated.
    height: Cell<i32>,
    /// Fired whenever a job reaches a terminal state (see `set_on_finished`).
    on_finished: RefCell<Option<Box<dyn Fn()>>>,
    /// Names sessions for the rows' session labels (see `set_session_names`).
    session_names: RefCell<Option<SessionNamer>>,
    /// Right-click menu over the panel and its actions, enabled per click
    /// for whatever is under the pointer (see `open_menu`).
    popover: gtk::PopoverMenu,
    remove_action: gio::SimpleAction,
    clear_action: gio::SimpleAction,
    /// The finished row the menu was opened on (target of "Remove").
    menu_target: Cell<Option<TransferId>>,
}

impl QueueView {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(8);
        list.set_margin_end(8);

        // Rows are prepended, so this stays last and is simply hidden while
        // there are any. It expands so it sits centred in the empty panel.
        let empty = gtk::Label::new(Some(&tr!("Transfers and file operations appear here")));
        empty.add_css_class("dim-label");
        empty.set_vexpand(true);
        empty.set_valign(gtk::Align::Center);
        list.append(&empty);

        // No height cap: the paned divider above the queue decides how
        // tall it is, and the list scrolls within that.
        let scroller = gtk::ScrolledWindow::builder()
            .min_content_height(MIN_HEIGHT)
            .vexpand(true)
            .child(&list)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&scroller);

        let menu = gio::Menu::new();
        menu.append(Some(&tr!("Remove")), Some("queue.remove"));
        menu.append(Some(&tr!("Clear Finished")), Some("queue.clear-finished"));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&root);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);

        let remove_action = gio::SimpleAction::new("remove", None);
        let clear_action = gio::SimpleAction::new("clear-finished", None);
        let group = gio::SimpleActionGroup::new();
        group.add_action(&remove_action);
        group.add_action(&clear_action);
        root.insert_action_group("queue", Some(&group));

        let this = Rc::new(Self {
            root,
            list,
            empty,
            rows: RefCell::new(HashMap::new()),
            height: Cell::new(MIN_HEIGHT),
            on_finished: RefCell::new(None),
            session_names: RefCell::new(None),
            popover,
            remove_action,
            clear_action,
            menu_target: Cell::new(None),
        });

        {
            let weak = Rc::downgrade(&this);
            this.remove_action.connect_activate(move |_, _| {
                if let Some(queue) = weak.upgrade() {
                    if let Some(id) = queue.menu_target.take() {
                        queue.dismiss(id);
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.clear_action.connect_activate(move |_, _| {
                if let Some(queue) = weak.upgrade() {
                    queue.clear_finished();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            gesture.connect_pressed(move |_, _, x, y| {
                if let Some(queue) = weak.upgrade() {
                    queue.open_menu(x, y);
                }
            });
            this.root.add_controller(gesture);
        }

        this
    }

    /// Height the panel should open at (restored from settings).
    pub fn set_height(&self, height: i32) {
        self.height.set(height.max(MIN_HEIGHT));
    }

    /// The panel's current height once it has been allocated, otherwise the
    /// height it was told to open at.
    pub fn height(&self) -> i32 {
        if self.root.height() > 0 {
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
            return Some(tr!("Local"));
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
            self.empty.set_visible(false);
            row
        });
        row.state.set(snapshot.state);

        row.title.set_text(&snapshot.title);
        // Re-resolved on every snapshot so a flipped tab-naming preference
        // shows up live; once the session is gone the last name sticks.
        if let Some(text) = self.session_text(&snapshot.sessions) {
            if row.session.text().as_str() != text {
                row.session.set_text(&text);
                row.session.set_tooltip_text(Some(&text));
            }
        }
        // Direction badge: stays on the row for its whole life, so finished
        // jobs still read as uploads or downloads at a glance.
        match snapshot.direction {
            Some(TransferDirection::Upload) => {
                row.direction.set_icon_name(Some("go-up-symbolic"));
                row.direction.add_css_class("upload");
                row.direction.set_tooltip_text(Some(&tr!("Upload")));
                row.direction.set_visible(true);
            }
            Some(TransferDirection::Download) => {
                row.direction.set_icon_name(Some("go-down-symbolic"));
                row.direction.add_css_class("download");
                row.direction.set_tooltip_text(Some(&tr!("Download")));
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
            TransferState::Queued => tr!("Queued"),
            TransferState::Scanning => tr!("Scanning… {} files found", snapshot.files_total),
            TransferState::Running => running_detail(&snapshot),
            TransferState::Paused => tr!("Paused"),
            TransferState::WaitingConflict => tr!("Waiting for answer…"),
            TransferState::Done => done_detail(&snapshot),
            TransferState::Failed => match &snapshot.error {
                Some(err) => tr!("Failed — {}", err),
                None => tr!("Failed"),
            },
            TransferState::Cancelled => cancelled_detail(&snapshot),
        };
        row.detail.set_text(&detail);
        if !snapshot.state.is_terminal() {
            row.timing.set_text(&format_duration(snapshot.elapsed));
        }

        match snapshot.state {
            TransferState::Paused => {
                row.pause_btn.set_icon_name("media-playback-start-symbolic");
                row.pause_btn.set_tooltip_text(Some(&tr!("Resume")));
            }
            _ => {
                row.pause_btn.set_icon_name("media-playback-pause-symbolic");
                row.pause_btn.set_tooltip_text(Some(&tr!("Pause")));
            }
        }
        // Mutations support cancel but not pause.
        row.pause_btn
            .set_visible(!is_op && !snapshot.state.is_terminal());

        if snapshot.state.is_terminal() {
            row.pause_btn.set_visible(false);
            row.cancel_btn.set_icon_name("window-close-symbolic");
            row.cancel_btn.set_tooltip_text(Some(&tr!("Remove")));
            // The outcome line is the row's lasting record: colour it by
            // how the job ended (no longer dimmed, so the colour reads) and
            // keep the full text reachable when it is too long to fit.
            row.detail.set_tooltip_text(Some(&detail));
            match snapshot.state {
                TransferState::Done => {
                    row.progress.set_fraction(1.0);
                    row.detail.remove_css_class("dim-label");
                    row.detail.add_css_class("success");
                }
                TransferState::Failed => {
                    row.detail.remove_css_class("dim-label");
                    row.detail.add_css_class("error");
                }
                _ => {}
            }
            // Terminal snapshots arrive exactly once per job, so this is the
            // final word on timing: how long it took and when it ended.
            let finished_at = glib::DateTime::now_local().ok();
            let clock = finished_at
                .as_ref()
                .and_then(|now| now.format("%H:%M:%S").ok());
            let took = format_duration(snapshot.elapsed);
            row.timing.set_text(&match &clock {
                Some(clock) => format!("{took} • {clock}"),
                None => took.clone(),
            });
            if let Some(stamp) = finished_at.and_then(|now| now.format("%Y-%m-%d %H:%M:%S").ok()) {
                row.timing
                    .set_tooltip_text(Some(&tr!("Finished {}, took {}", stamp, took)));
            }
            // Terminal states are emitted exactly once, so this fires once
            // per job however it ended: partial results from a cancelled or
            // failed transfer should show up in the panes too.
            if let Some(cb) = self.on_finished.borrow().as_ref() {
                cb();
            }
        }
    }

    fn dismiss(&self, id: TransferId) {
        if let Some(row) = self.rows.borrow_mut().remove(&id) {
            self.list.remove(&row.root);
        }
        self.empty.set_visible(self.rows.borrow().is_empty());
    }

    /// Remove every row whose job has ended (Done, Failed or Cancelled).
    fn clear_finished(&self) {
        let finished: Vec<TransferId> = self
            .rows
            .borrow()
            .iter()
            .filter(|(_, row)| row.state.get().is_terminal())
            .map(|(id, _)| *id)
            .collect();
        for id in finished {
            self.dismiss(id);
        }
    }

    /// Show the right-click menu at (`x`, `y`) in panel coordinates. "Remove"
    /// targets the finished row under the pointer; "Clear Finished" needs at
    /// least one finished row. Running jobs are cancelled from their row.
    fn open_menu(&self, x: f64, y: f64) {
        let target = self.row_at(x, y).filter(|id| {
            self.rows
                .borrow()
                .get(id)
                .is_some_and(|row| row.state.get().is_terminal())
        });
        self.menu_target.set(target);
        self.remove_action.set_enabled(target.is_some());
        let any_finished = self
            .rows
            .borrow()
            .values()
            .any(|row| row.state.get().is_terminal());
        self.clear_action.set_enabled(any_finished);
        self.popover
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.popover.popup();
    }

    /// The job whose row is under (`x`, `y`) in panel coordinates, if any.
    fn row_at(&self, x: f64, y: f64) -> Option<TransferId> {
        let mut widget = self.root.pick(x, y, gtk::PickFlags::DEFAULT)?;
        // Walk up to the list's direct child, which is a row's root.
        loop {
            let parent = widget.parent()?;
            if &parent == self.list.upcast_ref::<gtk::Widget>() {
                break;
            }
            widget = parent;
        }
        self.rows
            .borrow()
            .iter()
            .find(|(_, row)| row.root.upcast_ref::<gtk::Widget>() == &widget)
            .map(|(id, _)| *id)
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
    let session = gtk::Label::new(Some(&tr!("Remote")));
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
    detail.set_hexpand(true);
    detail.add_css_class("caption");
    detail.add_css_class("dim-label");
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);

    // Elapsed time while running, then "duration • finished at": the same
    // quiet caption style as the session label, at the end of the detail line.
    let timing = gtk::Label::new(None);
    timing.set_xalign(1.0);
    timing.set_valign(gtk::Align::Center);
    timing.add_css_class("caption");
    timing.add_css_class("dim-label");

    let detail_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    detail_row.append(&detail);
    detail_row.append(&timing);

    let progress = gtk::ProgressBar::new();
    progress.set_hexpand(true);
    progress.set_valign(gtk::Align::Center);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text_box.set_hexpand(true);
    text_box.append(&title_row);
    text_box.append(&progress);
    text_box.append(&detail_row);

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
    cancel_btn.set_tooltip_text(Some(&tr!("Cancel")));

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
    // Keep the badge immediately left of the cancel/remove "x".
    root.append(&direction);
    root.append(&cancel_btn);

    Row {
        root,
        title,
        session,
        detail,
        timing,
        progress,
        direction,
        pause_btn,
        cancel_btn,
        state: Cell::new(TransferState::Queued),
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
            return tr!("{} removed…", snap.files_done);
        }
        TransferKind::Attributes => {
            return if snap.scanning {
                tr!("Collecting items… {} changed", snap.files_done)
            } else {
                tr!("{} / {} changed", snap.files_done, snap.files_total)
            };
        }
        TransferKind::Copy | TransferKind::Move => {}
    }
    let mut text = if snap.scanning {
        // "+" signals the totals are still climbing.
        tr!(
            "{} • {}/{}+ files • counting…",
            format_size(snap.done_bytes),
            snap.files_done,
            snap.files_total,
        )
    } else {
        tr!(
            "{} / {} • {}/{} files",
            format_size(snap.done_bytes),
            format_size(snap.total_bytes),
            snap.files_done,
            snap.files_total,
        )
    };

    if snap.speed_bps > 1.0 {
        text.push_str(&tr!("  •  {}/s", format_size(snap.speed_bps as u64)));
        // Only estimate time once the total is final; otherwise the remaining
        // byte count is unknown and any ETA would be wrong.
        if !snap.scanning {
            let remaining = snap.total_bytes.saturating_sub(snap.done_bytes);
            let eta = remaining as f64 / snap.speed_bps;
            text.push_str(&tr!("  •  {} left", format_eta(eta)));
        }
    }

    if !snap.current_file.is_empty() {
        text.push_str(&format!("  •  {}", snap.current_file));
    }
    text
}

/// Detail line for a successfully finished job, per kind. Copies also get
/// their average throughput, since the live speed is gone from the row.
fn done_detail(snap: &TransferSnapshot) -> String {
    match snap.kind {
        TransferKind::Delete => tr!("Removed {}", count(snap.files_done, "item", "items")),
        TransferKind::Attributes => tr!("Changed {}", count(snap.files_done, "item", "items")),
        TransferKind::Copy | TransferKind::Move => {
            let mut text = tr!(
                "Done — {} in {}",
                format_size(snap.total_bytes),
                count(snap.files_total, "file", "files")
            );
            let secs = snap.elapsed.as_secs_f64();
            if snap.total_bytes > 0 && secs > 0.0 {
                let average = (snap.total_bytes as f64 / secs) as u64;
                text.push_str(&tr!(" • {}/s", format_size(average)));
            }
            text
        }
    }
}

/// Detail line for a cancelled job: what had already been done by then,
/// since a partial copy or deletion has still changed things.
fn cancelled_detail(snap: &TransferSnapshot) -> String {
    match snap.kind {
        TransferKind::Delete => tr!("Cancelled — {} removed", snap.files_done),
        TransferKind::Attributes => tr!("Cancelled — {} changed", snap.files_done),
        TransferKind::Copy | TransferKind::Move => {
            if snap.scanning {
                // The totals were still being counted, so only what got
                // done is certain.
                tr!(
                    "Cancelled — {} • {}",
                    format_size(snap.done_bytes),
                    count(snap.files_done, "file", "files")
                )
            } else {
                tr!(
                    "Cancelled — {} of {} • {}/{} files",
                    format_size(snap.done_bytes),
                    format_size(snap.total_bytes),
                    snap.files_done,
                    snap.files_total
                )
            }
        }
    }
}

/// "1 file" / "3 files".
fn count(n: u32, singular: &str, plural: &str) -> String {
    format!("{} {}", n, linuxscp::i18n::ngettext(singular, plural, n))
}

/// How long a job has been running or took: "< 1 s", "12 s", "1m 05s",
/// "1h 02m".
fn format_duration(duration: Duration) -> String {
    let s = duration.as_secs();
    if s < 1 {
        tr!("< 1 s")
    } else if s < 60 {
        tr!("{} s", s)
    } else if s < 3600 {
        tr!("{}m {}s", s / 60, format!("{:02}", s % 60))
    } else {
        tr!("{}h {}m", s / 3600, format!("{:02}", (s % 3600) / 60))
    }
}

fn format_eta(seconds: f64) -> String {
    let s = seconds as u64;
    if s >= 48 * 3600 {
        tr!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        tr!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        tr!("{}m {}s", s / 60, s % 60)
    } else {
        tr!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(Duration::from_millis(400)), "< 1 s");
        assert_eq!(format_duration(Duration::from_secs(12)), "12 s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h 02m");
    }

    #[test]
    fn counts_pluralise() {
        assert_eq!(count(1, "file", "files"), "1 file");
        assert_eq!(count(0, "item", "items"), "0 items");
        assert_eq!(count(3, "file", "files"), "3 files");
    }
}
