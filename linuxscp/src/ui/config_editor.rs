//! In-app text editor for ~/.ssh/config, opened from the site manager so
//! hosts can be added or fixed without leaving the app.

use adw::prelude::*;

use linuxscp::ssh::config::ssh_dir;

/// Show the editor; `on_saved` runs after a successful write so the caller
/// can re-parse the host list.
pub fn show(parent: &impl IsA<gtk::Widget>, on_saved: impl Fn() + 'static) {
    let path = ssh_dir().join("config");
    let content = std::fs::read_to_string(&path).unwrap_or_default();

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let title = adw::WindowTitle::new("Edit SSH Config", &path.to_string_lossy());
    header.set_title_widget(Some(&title));
    let cancel_btn = gtk::Button::with_label("Cancel");
    let save_btn = gtk::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&save_btn);

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&content);
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .monospace(true)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&view)
        .build();

    let hint = gtk::Label::new(Some(
        "Host blocks defined here appear in the host list and are used by \
         the system ssh — aliases, jump hosts, keys and all.",
    ));
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.set_margin_start(12);
    hint.set_margin_end(12);
    hint.set_margin_top(6);
    hint.set_margin_bottom(8);

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_box.append(&scroller);
    content_box.append(&hint);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content_box));

    let dialog = adw::Dialog::builder()
        .title("Edit SSH Config")
        .content_width(720)
        .content_height(560)
        .child(&toolbar)
        .build();

    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        let parent = parent.clone().upcast::<gtk::Widget>();
        save_btn.connect_clicked(move |_| {
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
            match write_config(&path, text.as_str()) {
                Ok(()) => {
                    on_saved();
                    dialog.close();
                }
                Err(err) => {
                    let alert = adw::AlertDialog::builder()
                        .heading("Could Not Save")
                        .body(format!("{err}"))
                        .build();
                    alert.add_response("ok", "OK");
                    alert.present(Some(&parent));
                }
            }
        });
    }

    dialog.present(Some(parent));
    view.grab_focus();
}

/// Write the config, creating ~/.ssh (0700) and the file (0600) when they
/// do not exist yet, and preserving the mode when they do.
fn write_config(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if let Some(dir) = path.parent().filter(|dir| !dir.exists()) {
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}
