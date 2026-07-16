mod app;
mod ui;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.theflyingjay.LinuxSCP";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxscp=info".into()),
        )
        .init();

    // Initialize the tokio runtime early.
    linuxscp::runtime::runtime();

    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| {
        register_icon_search_paths();
        load_css();
    });
    application.connect_activate(|application| {
        // Re-activation (launching a second instance): raise what we have.
        if let Some(window) = application
            .active_window()
            .or_else(|| application.windows().into_iter().next())
        {
            window.present();
            return;
        }

        let app = app::App::build(application);

        let open_main = {
            let app = app.clone();
            move || {
                app.window.present();
                // WinSCP-style login: open the site list over the main
                // window so a saved site (and its directory) is one click.
                app.open_site_manager_active();
            }
        };

        let splash_time = ui::splash::duration();
        if splash_time.is_zero() {
            open_main();
        } else {
            let splash = ui::splash::show(application);
            glib::timeout_add_local_once(splash_time, move || {
                splash.close();
                open_main();
            });
        }

        // Keep the App alive for the lifetime of the window.
        std::mem::forget(app);
    });
    application.run()
}

/// App-level CSS (splash window styling, compact list rows).
fn load_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "window.splash { border-radius: 18px; }\n\
         /* Dense single-line rows (site manager lists), matching the file\n\
            lists' row height. Selectors mirror the theme's specificity. */\n\
         list.compact-list > row,\n\
         listview.compact-list > row {\n\
             min-height: 26px;\n\
             padding-top: 2px;\n\
             padding-bottom: 2px;\n\
         }\n\
         /* Highlight the row (or the whole list, for root drops) a dragged\n\
            site would land on. */\n\
         listview.compact-list > row box:drop(active) {\n\
             background: alpha(@accent_bg_color, 0.25);\n\
             border-radius: 6px;\n\
         }\n\
         listview.compact-list:drop(active) {\n\
             background: alpha(@accent_bg_color, 0.08);\n\
         }\n\
         /* Session tabs: recessed strip with boxed, bordered tabs so they\n\
            read as tabs in light and dark mode (WinSCP/browser style).\n\
            The theme strips tab styling entirely when only one tab exists\n\
            (tabbox.single-tab) - override that too, so a lone tab still\n\
            looks like a tab. */\n\
         box.session-strip {\n\
             background: @shade_color;\n\
         }\n\
         tabbar.session-tabs .box {\n\
             background: none;\n\
             box-shadow: none;\n\
             border: none;\n\
         }\n\
         tabbar.session-tabs tab {\n\
             margin: 5px 3px;\n\
             border: 1px solid alpha(currentColor, 0.2);\n\
             border-radius: 7px;\n\
             background: none;\n\
         }\n\
         tabbar.session-tabs tab:hover {\n\
             background: alpha(currentColor, 0.06);\n\
         }\n\
         tabbar.session-tabs tab:selected,\n\
         tabbar.session-tabs tabbox.single-tab tab {\n\
             background: @headerbar_bg_color;\n\
             border-color: alpha(currentColor, 0.35);\n\
             box-shadow: 0 1px 2px alpha(black, 0.25);\n\
         }\n\
         button.new-session-tab {\n\
             border: 1px dashed alpha(currentColor, 0.35);\n\
             border-radius: 7px;\n\
             color: alpha(currentColor, 0.8);\n\
         }\n\
         /* Queue direction badges: green up-arrow for uploads, blue\n\
            down-arrow for downloads (GNOME palette, ok in both themes). */\n\
         image.transfer-direction.upload { color: #26a269; }\n\
         image.transfer-direction.download { color: #3584e4; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Let the app find its icon whether installed system-wide or run straight
/// from the source tree (`cargo run`).
fn register_icon_search_paths() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let theme = gtk::IconTheme::for_display(&display);
    theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../data/icons"));
    if let Ok(exe) = std::env::current_exe() {
        // ../../data/icons relative to target/<profile>/linuxscp.
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            theme.add_search_path(root.join("data/icons"));
        }
    }
    theme.add_search_path("/usr/share/icons");
}
