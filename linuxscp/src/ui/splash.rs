//! Borderless splash window shown briefly while the app starts, WinSCP-style.

use adw::prelude::*;

const DEFAULT_DURATION_MS: u64 = 1200;

/// Splash display time; `LINUXSCP_SPLASH_MS` overrides it (0 disables).
pub fn duration() -> std::time::Duration {
    let ms = std::env::var("LINUXSCP_SPLASH_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DURATION_MS);
    std::time::Duration::from_millis(ms)
}

pub fn show(app: &adw::Application) -> gtk::Window {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_valign(gtk::Align::Center);
    content.set_margin_top(32);
    content.set_margin_bottom(28);
    content.set_margin_start(48);
    content.set_margin_end(48);

    content.append(&logo_widget());

    let title = gtk::Label::new(Some("LinuxSCP"));
    title.add_css_class("title-1");
    title.set_margin_top(12);
    content.append(&title);

    let subtitle = gtk::Label::new(Some("Commander-style SFTP client for GNOME"));
    subtitle.add_css_class("dim-label");
    content.append(&subtitle);

    let version = gtk::Label::new(Some(concat!("Version ", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("caption");
    version.add_css_class("dim-label");
    content.append(&version);

    let spinner = gtk::Spinner::new();
    spinner.set_size_request(22, 22);
    spinner.set_margin_top(14);
    spinner.start();
    content.append(&spinner);

    let window = gtk::Window::builder()
        .application(app)
        .title("LinuxSCP")
        .decorated(false)
        .resizable(false)
        .default_width(420)
        .default_height(360)
        .child(&content)
        .build();
    window.add_css_class("splash");
    window.present();
    window
}

/// Full-resolution logo: source PNG in the dev tree, installed hicolor icon,
/// or the icon theme as a last resort.
fn logo_widget() -> gtk::Widget {
    const CANDIDATES: [&str; 2] = [
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../data/io.github.linuxscp.LinuxSCP.png"
        ),
        "/usr/share/icons/hicolor/512x512/apps/io.github.linuxscp.LinuxSCP.png",
    ];
    for path in CANDIDATES {
        if std::path::Path::new(path).is_file() {
            let picture = gtk::Picture::for_filename(path);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_size_request(160, 160);
            return picture.upcast();
        }
    }
    let image = gtk::Image::from_icon_name("io.github.linuxscp.LinuxSCP");
    image.set_pixel_size(160);
    image.upcast()
}
