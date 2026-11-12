//! Modal prompts driven by background events: ssh passwords, host-key
//! confirmations and transfer conflicts.

use std::cell::RefCell;
use std::rc::Rc;
use std::task::Poll;

use adw::prelude::*;
use gtk::glib;

use linuxscp::tr;
use linuxscp::types::{ConflictDecision, ConflictRequest, PromptKind, PromptRequest, Reply};

pub fn show_prompt(parent: &impl IsA<gtk::Widget>, request: PromptRequest) {
    match request.kind {
        PromptKind::Secret => show_secret(parent, request),
        PromptKind::YesNo => show_yes_no(parent, request),
    }
}

/// The reply channel of an open prompt, shared between the dialog's response
/// handler and the watcher that dismisses it; `None` once answered.
type SharedReply<T> = Rc<RefCell<Option<Reply<T>>>>;

/// Close `dialog` if the backend stops waiting for the answer — the ssh
/// attempt failed, was cancelled or timed out — so a stale password prompt
/// isn't left sitting under the "Connection Failed" alert. Resolves to the
/// handle the response handler must abort once the user has answered.
fn dismiss_when_abandoned<T: 'static>(
    dialog: &adw::AlertDialog,
    reply: &SharedReply<T>,
) -> glib::JoinHandle<()> {
    let dialog = dialog.clone();
    let reply = reply.clone();
    glib::spawn_future_local(async move {
        let abandoned = std::future::poll_fn(|cx| match reply.borrow_mut().as_mut() {
            Some(tx) => tx.poll_closed(cx).map(|()| true),
            None => Poll::Ready(false),
        })
        .await;
        if abandoned {
            // Taking the sender first means the response handler, which
            // close() triggers with the close response, has nothing to send.
            reply.borrow_mut().take();
            dialog.close();
        }
    })
}

fn show_secret(parent: &impl IsA<gtk::Widget>, request: PromptRequest) {
    let dialog = adw::AlertDialog::builder()
        .heading(tr!("Authentication Required"))
        .body(&request.prompt)
        .close_response("cancel")
        .default_response("ok")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    let reply: SharedReply<Option<String>> = Rc::new(RefCell::new(Some(request.reply)));
    let watcher = dismiss_when_abandoned(&dialog, &reply);
    let entry_clone = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if let Some(reply) = reply.borrow_mut().take() {
            watcher.abort();
            let answer = if response == "ok" {
                Some(entry_clone.text().to_string())
            } else {
                None
            };
            let _ = reply.send(answer);
        }
        dialog.close();
    });
    dialog.present(Some(parent));
    entry.grab_focus();
}

fn show_yes_no(parent: &impl IsA<gtk::Widget>, request: PromptRequest) {
    let dialog = adw::AlertDialog::builder()
        .heading(tr!("Confirm"))
        .body(&request.prompt)
        .body_use_markup(false)
        .close_response("no")
        .default_response("no")
        .build();
    dialog.add_response("no", "No");
    dialog.add_response("yes", "Yes");
    dialog.set_response_appearance("yes", adw::ResponseAppearance::Suggested);

    let reply: SharedReply<Option<String>> = Rc::new(RefCell::new(Some(request.reply)));
    let watcher = dismiss_when_abandoned(&dialog, &reply);
    dialog.connect_response(None, move |dialog, response| {
        if let Some(reply) = reply.borrow_mut().take() {
            watcher.abort();
            let answer = (response == "yes").then(|| "yes".to_string());
            let _ = reply.send(answer);
        }
        dialog.close();
    });
    dialog.present(Some(parent));
}

pub fn show_conflict(parent: &impl IsA<gtk::Widget>, request: ConflictRequest) {
    let body = tr!(
        "\u{201c}{}\u{201d} already exists in the destination.\n\nSource: {}\nDestination: {}",
        request.file_name,
        super::entry_object::format_size(request.src_size),
        super::entry_object::format_size(request.dst_size),
    );
    let dialog = adw::AlertDialog::builder()
        .heading(tr!("File Exists"))
        .body(&body)
        .close_response("cancel")
        .default_response("overwrite")
        .build();
    dialog.add_response("overwrite", &tr!("Overwrite"));
    dialog.add_response("overwrite_all", &tr!("Overwrite All"));
    if request.dst_size < request.src_size {
        dialog.add_response("resume", &tr!("Resume"));
    }
    dialog.add_response("skip", &tr!("Skip"));
    dialog.add_response("skip_all", &tr!("Skip All"));
    dialog.add_response("cancel", &tr!("Cancel Transfer"));
    dialog.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
    dialog.set_response_appearance("overwrite_all", adw::ResponseAppearance::Destructive);
    dialog.set_response_appearance("cancel", adw::ResponseAppearance::Default);

    let reply = std::cell::RefCell::new(Some(request.reply));
    dialog.connect_response(None, move |dialog, response| {
        if let Some(reply) = reply.borrow_mut().take() {
            let decision = match response {
                "overwrite" => ConflictDecision::Overwrite,
                "overwrite_all" => ConflictDecision::OverwriteAll,
                "resume" => ConflictDecision::Resume,
                "skip" => ConflictDecision::Skip,
                "skip_all" => ConflictDecision::SkipAll,
                _ => ConflictDecision::CancelJob,
            };
            let _ = reply.send(decision);
        }
        dialog.close();
    });
    dialog.present(Some(parent));
}

/// Simple one-line text input (mkdir, rename).
pub fn ask_text(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    initial: &str,
    accept_label: &str,
    on_done: impl Fn(String) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .close_response("cancel")
        .default_response("ok")
        .build();
    dialog.add_response("cancel", &tr!("Cancel"));
    dialog.add_response("ok", accept_label);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    let entry_clone = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response == "ok" {
            let text = entry_clone.text().to_string();
            if !text.is_empty() {
                on_done(text);
            }
        }
        dialog.close();
    });
    dialog.present(Some(parent));
    entry.grab_focus();
}

/// Destructive confirmation (delete).
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    action_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_response("cancel", &tr!("Cancel"));
    dialog.add_response("do", action_label);
    dialog.set_response_appearance("do", adw::ResponseAppearance::Destructive);
    dialog.connect_response(None, move |dialog, response| {
        if response == "do" {
            on_confirm();
        }
        dialog.close();
    });
    dialog.present(Some(parent));
}
