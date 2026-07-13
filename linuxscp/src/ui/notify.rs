//! Transfer-completion feedback: a success sound (via GtkMediaFile) and a
//! desktop notification (via the GioApplication notification API).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use linuxscp::types::TransferState;

/// What feedback to give when a transfer reaches a given state, factoring in
/// the user's notification settings. `None` means stay silent (transfer is
/// still running, paused, or was cancelled by the user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPlan {
    pub play_sound: bool,
    pub show_desktop: bool,
    pub success: bool,
    pub title: &'static str,
}

pub fn completion_plan(
    state: TransferState,
    sound_on: bool,
    desktop_on: bool,
) -> Option<CompletionPlan> {
    let success = match state {
        TransferState::Done => true,
        TransferState::Failed => false,
        // Running/Paused/Scanning/Queued/WaitingConflict/Cancelled: no alert.
        _ => return None,
    };
    Some(CompletionPlan {
        // Only the success chime is shipped; failures notify silently.
        play_sound: sound_on && success,
        show_desktop: desktop_on,
        success,
        title: if success {
            "Transfer complete"
        } else {
            "Transfer failed"
        },
    })
}

/// Owns the media stream so playback isn't dropped mid-sound. Cloneable so
/// it can be shared with the app; playback is fire-and-forget.
#[derive(Clone)]
pub struct Notifier {
    app: adw::Application,
    /// Kept alive while a sound plays; replaced on each play.
    media: Rc<RefCell<Option<gtk::MediaFile>>>,
    sound_path: Option<PathBuf>,
}

impl Notifier {
    pub fn new(app: &adw::Application) -> Self {
        Self {
            app: app.clone(),
            media: Rc::new(RefCell::new(None)),
            sound_path: sound_path(),
        }
    }

    /// Play the success sound, if the asset was found.
    pub fn play_success(&self) {
        let Some(path) = &self.sound_path else {
            tracing::warn!("success sound asset not found");
            return;
        };
        let media = gtk::MediaFile::for_filename(path);
        // Log decode/backend errors instead of failing silently.
        media.connect_error_notify(|m| {
            if let Some(err) = m.error() {
                tracing::warn!("sound playback error: {err}");
            }
        });
        // Drop our reference once playback finishes so it can be collected.
        let holder = self.media.clone();
        media.connect_ended_notify(move |_| {
            holder.borrow_mut().take();
        });
        media.set_volume(1.0);
        media.play();
        self.media.borrow_mut().replace(media);
    }

    /// Send a desktop notification. `body` is the detail line.
    pub fn notify(&self, title: &str, body: &str, success: bool) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_icon(&gio::ThemedIcon::new(if success {
            "emblem-ok-symbolic"
        } else {
            "dialog-error-symbolic"
        }));
        notification.set_priority(gio::NotificationPriority::Normal);
        // A stable id coalesces repeat notifications instead of stacking.
        self.app
            .send_notification(Some("transfer-complete"), &notification);
    }
}

/// Locate `success.mp3`: dev tree first, then the installed data dir.
fn sound_path() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../data/sounds/success.mp3"
        )),
        PathBuf::from("/app/share/linuxscp/sounds/success.mp3"),
        PathBuf::from("/usr/share/linuxscp/sounds/success.mp3"),
        PathBuf::from("/usr/local/share/linuxscp/sounds/success.mp3"),
    ];
    let exe_root = std::env::current_exe().ok().and_then(|exe| {
        exe.parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(std::path::Path::to_path_buf)
    });
    if let Some(root) = exe_root {
        candidates.push(root.join("data/sounds/success.mp3"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Execute a [`CompletionPlan`]: play the sound and/or send the notification.
pub fn run_plan(notifier: &Notifier, plan: &CompletionPlan, detail: &str) {
    if plan.play_sound {
        notifier.play_success();
    }
    if plan.show_desktop {
        notifier.notify(plan.title, detail, plan.success);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_matrix() {
        // Success with both toggles on: sound + desktop.
        let p = completion_plan(TransferState::Done, true, true).unwrap();
        assert!(p.play_sound && p.show_desktop && p.success);

        // Failure never plays the success sound, but can still notify.
        let p = completion_plan(TransferState::Failed, true, true).unwrap();
        assert!(!p.play_sound && p.show_desktop && !p.success);

        // Toggles off: silence.
        let p = completion_plan(TransferState::Done, false, false).unwrap();
        assert!(!p.play_sound && !p.show_desktop);

        // Non-terminal / cancelled: no plan at all.
        assert!(completion_plan(TransferState::Running, true, true).is_none());
        assert!(completion_plan(TransferState::Cancelled, true, true).is_none());
    }
}
