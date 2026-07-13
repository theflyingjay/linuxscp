//! Bottom transfer queue: one row per job with progress, speed, ETA and
//! pause/resume/cancel controls.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::entry_object::format_size;
use linuxscp::transfers;
use linuxscp::types::{TransferAction, TransferId, TransferKind, TransferSnapshot, TransferState};

struct Row {
    root: gtk::Box,
    title: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    pause_btn: gtk::Button,
    cancel_btn: gtk::Button,
}

pub struct QueueView {
    pub revealer: gtk::Revealer,
    list: gtk::Box,
    rows: RefCell<HashMap<TransferId, Row>>,
    /// Jobs the user hasn't dismissed yet (keeps the revealer open).
    on_finished: RefCell<Option<Box<dyn Fn()>>>,
}

impl QueueView {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(8);
        list.set_margin_end(8);

        let scroller = gtk::ScrolledWindow::builder()
            .max_content_height(200)
            .propagate_natural_height(true)
            .child(&list)
            .build();

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        frame.append(&scroller);

        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .reveal_child(false)
            .child(&frame)
            .build();

        Rc::new(Self {
            revealer,
            list,
            rows: RefCell::new(HashMap::new()),
            on_finished: RefCell::new(None),
        })
    }

    /// Callback fired whenever a transfer reaches any terminal state — Done,
    /// Cancelled or Failed (the app refreshes the panes). Even an interrupted
    /// transfer has usually already changed the destination (and, for moves,
    /// the source), so the views must catch up regardless of how it ended.
    pub fn set_on_finished(&self, cb: impl Fn() + 'static) {
        *self.on_finished.borrow_mut() = Some(Box::new(cb));
    }

    pub fn update(self: &Rc<Self>, snapshot: TransferSnapshot) {
        let mut rows = self.rows.borrow_mut();
        let row = rows.entry(snapshot.id).or_insert_with(|| {
            let row = build_row(self, snapshot.id);
            self.list.append(&row.root);
            self.revealer.set_reveal_child(true);
            row
        });

        row.title.set_text(&snapshot.title);
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
            self.revealer.set_reveal_child(false);
        }
    }
}

fn build_row(queue: &Rc<QueueView>, id: TransferId) -> Row {
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.add_css_class("heading");
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);

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
    text_box.append(&title);
    text_box.append(&progress);
    text_box.append(&detail);

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
    root.append(&cancel_btn);

    Row {
        root,
        title,
        detail,
        progress,
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
