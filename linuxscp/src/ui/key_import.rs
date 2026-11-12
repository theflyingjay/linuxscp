//! "Import PuTTY Key" sheet, opened when the site editor's key chooser lands
//! on a `.ppk`. It shows what the file holds (type, fingerprint, comment —
//! all readable without the passphrase), asks for the passphrase and a
//! name, converts the key to an OpenSSH file under `~/.ssh` off the GTK
//! thread, and hands the new path back to the form. The original `.ppk`
//! is never modified.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use zeroize::Zeroizing;

use linuxscp::runtime::runtime;
use linuxscp::ssh::key_import::{self, ImportError, ImportRequest, Imported};
use linuxscp::ssh::ppk::{self, MAX_FILE_LEN, PpkError};
use linuxscp::tr;

/// Show the sheet for `source`. `on_imported` receives the written key and,
/// when the converted key was encrypted with it, the passphrase, so the
/// caller can offer to remember it.
pub fn show(
    parent: &impl IsA<gtk::Widget>,
    source: PathBuf,
    on_imported: impl Fn(Imported, Option<Zeroizing<String>>) + 'static,
) {
    let info = match read_info(&source) {
        Ok(info) => info,
        Err(message) => {
            let alert = adw::AlertDialog::builder()
                .heading(&tr!("Cannot Import Key"))
                .body(message)
                .build();
            alert.add_response("ok", &tr!("OK"));
            alert.present(Some(parent));
            return;
        }
    };
    let file_name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // --- Header ---------------------------------------------------------
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &tr!("Import PuTTY Key"),
        &file_name,
    )));
    let cancel_btn = gtk::Button::with_label(&tr!("Cancel"));
    let import_btn = gtk::Button::with_label(&tr!("Import"));
    import_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&import_btn);

    // --- What the file holds --------------------------------------------
    let key_group = adw::PreferencesGroup::builder().title(&tr!("Key")).build();
    let add_fact = |title: &str, value: &str| {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(value)
            .subtitle_selectable(true)
            .build();
        row.add_css_class("property");
        key_group.add(&row);
    };
    add_fact(&tr!("Type"), &info.key_type());
    add_fact(&tr!("Fingerprint"), &info.fingerprint());
    if !info.comment.is_empty() {
        add_fact(&tr!("Comment"), &info.comment);
    }
    add_fact(
        &tr!("Format"),
        &tr!(
            "PuTTY PPK version {}{}",
            info.version,
            if info.encrypted {
                tr!(", encrypted")
            } else {
                tr!(", not encrypted")
            }
        ),
    );

    // --- Conversion options ---------------------------------------------
    let ssh_dir = key_import::ssh_dir().unwrap_or_else(|| PathBuf::from("~/.ssh"));
    let convert_group = adw::PreferencesGroup::builder()
        .title(&tr!("Convert"))
        .description(tr!(
            "OpenSSH, which LinuxSCP connects with, cannot read PuTTY's .ppk format. \
             A copy in OpenSSH format is written to {} and selected for this site; \
             the .ppk file is not changed.",
            ssh_dir.display()
        ))
        .build();
    let passphrase_row = adw::PasswordEntryRow::builder()
        .title(&tr!("Passphrase"))
        .visible(info.encrypted)
        .build();
    convert_group.add(&passphrase_row);
    let name_row = adw::EntryRow::builder()
        .title(&tr!("Save as"))
        .text(key_import::suggest_name(&source))
        .build();
    convert_group.add(&name_row);
    let protect_row = adw::SwitchRow::builder()
        .title(&tr!("Protect the converted key with the same passphrase"))
        .subtitle(&tr!(
            "You can let LinuxSCP remember the passphrase for this site"
        ))
        .active(true)
        .visible(info.encrypted)
        .build();
    convert_group.add(&protect_row);

    // A failed attempt marks the field at fault; editing it clears the mark.
    for row in [
        passphrase_row.clone().upcast::<gtk::Editable>(),
        name_row.clone().upcast::<gtk::Editable>(),
    ] {
        row.connect_changed(|row| row.remove_css_class("error"));
    }

    let error_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    error_label.add_css_class("error");

    let spinner = gtk::Spinner::builder().visible(false).build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&key_group);
    content.append(&convert_group);
    content.append(&error_label);
    content.append(&spinner);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroller));

    let dialog = adw::Dialog::builder()
        .title(&tr!("Import PuTTY Key"))
        .content_width(520)
        .child(&toolbar)
        .build();
    dialog.set_default_widget(Some(&import_btn));

    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }

    // Lock the form while the conversion runs (Argon2 can take a moment).
    let set_busy: Rc<dyn Fn(bool)> = {
        let controls: [gtk::Widget; 5] = [
            import_btn.clone().upcast(),
            cancel_btn.clone().upcast(),
            passphrase_row.clone().upcast(),
            name_row.clone().upcast(),
            protect_row.clone().upcast(),
        ];
        let spinner = spinner.clone();
        let dialog = dialog.clone();
        Rc::new(move |busy: bool| {
            for control in &controls {
                control.set_sensitive(!busy);
            }
            spinner.set_visible(busy);
            spinner.set_spinning(busy);
            dialog.set_can_close(!busy);
        })
    };

    dialog.present(Some(parent));
    if info.encrypted {
        passphrase_row.grab_focus();
    } else {
        name_row.grab_focus();
    }

    let on_imported: Rc<dyn Fn(Imported, Option<Zeroizing<String>>)> = Rc::new(on_imported);
    let encrypted = info.encrypted;
    import_btn.connect_clicked(move |_| {
        error_label.set_visible(false);
        passphrase_row.remove_css_class("error");
        name_row.remove_css_class("error");

        let dest_name = name_row.text().trim().to_string();
        if key_import::validate_name(&dest_name).is_err() {
            name_row.add_css_class("error");
            name_row.grab_focus();
            return;
        }
        let passphrase = Zeroizing::new(passphrase_row.text().to_string());
        if encrypted && passphrase.is_empty() {
            passphrase_row.add_css_class("error");
            passphrase_row.grab_focus();
            return;
        }

        let request = ImportRequest {
            source: source.clone(),
            passphrase: encrypted.then(|| passphrase.clone()),
            dest_name,
            protect_with_passphrase: protect_row.is_active(),
        };
        set_busy(true);

        let handle = runtime().spawn_blocking(move || key_import::import(&request));
        let set_busy = set_busy.clone();
        let dialog = dialog.clone();
        let on_imported = on_imported.clone();
        let error_label = error_label.clone();
        let passphrase_row = passphrase_row.clone();
        let name_row = name_row.clone();
        glib::spawn_future_local(async move {
            let result = match handle.await {
                Ok(result) => result,
                Err(_) => Err(ImportError::Verify),
            };
            set_busy(false);
            match result {
                Ok(imported) => {
                    let kept = imported.passphrase_kept.then(|| passphrase.clone());
                    dialog.close();
                    on_imported(imported, kept);
                }
                Err(err) => {
                    error_label.set_label(&sentence(err.to_string()));
                    error_label.set_visible(true);
                    match err {
                        ImportError::Ppk(
                            PpkError::WrongPassphrase | PpkError::PassphraseRequired,
                        ) => {
                            passphrase_row.add_css_class("error");
                            passphrase_row.grab_focus();
                        }
                        ImportError::NameTaken(..) | ImportError::BadName => {
                            name_row.add_css_class("error");
                            name_row.grab_focus();
                        }
                        _ => {}
                    }
                }
            }
        });
    });
}

/// Read and inspect the file on the calling thread; it is tiny. Errors are
/// already phrased for the user.
fn read_info(source: &std::path::Path) -> Result<ppk::PpkInfo, String> {
    use std::io::Read;
    let file = std::fs::File::open(source)
        .map_err(|err| tr!("Could not read {}: {}", source.display(), err))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Could not read {}: {err}", source.display()))?;
    if bytes.len() > MAX_FILE_LEN {
        return Err("The file is too large to be a key file.".into());
    }
    ppk::inspect(&bytes).map_err(|err| sentence(err.to_string()))
}

/// Error messages are written lowercase for composing; shown on their own
/// they read better as a sentence.
fn sentence(message: String) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>() + ".",
        None => message,
    }
}
