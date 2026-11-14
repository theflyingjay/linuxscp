//! gettext-based localization.
//!
//! User-visible strings are wrapped with [`tr!`](crate::tr), which translates
//! the English message id through the catalog installed as
//! `<localedir>/<lang>/LC_MESSAGES/linuxscp.mo` (built from `po/`).
//!
//! Message ids are the English source strings, so a missing or partial
//! translation simply leaves that string in English.

use std::fmt::{Display, Write as _};
use std::path::PathBuf;

/// gettext text domain; must match the installed `.mo` file name.
pub const DOMAIN: &str = "linuxscp";

/// Languages with a catalog in `po/`, as (locale name, native name). The
/// native name is what the Preferences picker shows, so it is not itself
/// translated.
pub const AVAILABLE: [(&str, &str); 2] = [("en", "English"), ("zh_CN", "简体中文")];

/// True for a locale this build actually ships a catalog for.
///
/// `settings.json` is user-editable, so the saved value is never trusted:
/// gettext resolves `LANGUAGE` as a path component, where `..` would load a
/// catalog from outside the locale directory and a NUL byte panics.
pub fn is_supported(language: &str) -> bool {
    AVAILABLE.iter().any(|(code, _)| *code == language)
}

/// Switch the process to the user's locale and bind the translation domain.
///
/// Call once, as early as possible in `main`. `gettext` resolves catalogs
/// for the current C locale, which stays "C" (no translations) until
/// `setlocale` runs; GTK makes the same call in `gtk_init`.
///
/// An explicit language chosen in Preferences (stored in the settings file)
/// overrides the environment through `LANGUAGE`, which gettext consults
/// before `LC_MESSAGES`. gettext cannot retranslate live widgets, so the
/// choice takes effect on the next launch.
pub fn init() {
    apply_saved_language();
    // SAFETY: called once, before any threads are spawned, so no other
    // thread can observe the locale changing under it.
    unsafe {
        gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
    }
    let _ = gettextrs::bindtextdomain(DOMAIN, locale_dir());
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = gettextrs::textdomain(DOMAIN);
}

/// Honour the language picked in Preferences, if any.
fn apply_saved_language() {
    let Some(language) = crate::settings::Settings::load().language else {
        return;
    };
    if !is_supported(&language) {
        tracing::warn!("ignoring unknown language {language:?} from settings");
        return;
    }
    // SAFETY: called before any threads are spawned (see `init`).
    unsafe {
        std::env::set_var("LANGUAGE", language);
    }
}

/// Where the `.mo` files live.
///
/// Depends on how the app was installed, so try, in order: an explicit
/// override for packagers and debugging, the directory baked in at build
/// time, then the distro (`/usr`), `make install` (`/usr/local`, the
/// Makefile default) and Flatpak (`/app`) locations. A directory only wins
/// if it actually holds one of our catalogs, so a `/usr/local` install is
/// not shadowed by the distro package's empty `/usr/share/locale`.
fn locale_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LINUXSCP_LOCALEDIR") {
        return PathBuf::from(dir);
    }

    let mut candidates = Vec::with_capacity(4);
    if let Some(dir) = option_env!("LINUXSCP_LOCALEDIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from("/usr/share/locale"));
    candidates.push(PathBuf::from("/usr/local/share/locale"));
    candidates.push(PathBuf::from("/app/share/locale"));

    candidates
        .iter()
        .find(|dir| has_catalog(dir))
        .or_else(|| candidates.first())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/usr/share/locale"))
}

/// True when `dir` holds a catalog this build ships.
fn has_catalog(dir: &std::path::Path) -> bool {
    AVAILABLE.iter().any(|(code, _)| {
        dir.join(code)
            .join("LC_MESSAGES")
            .join("linuxscp.mo")
            .is_file()
    })
}

/// Translate a message id (the English source string).
pub fn gettext(msgid: &str) -> String {
    gettextrs::gettext(msgid)
}

/// Translate a plural form for `n`.
///
/// Languages with more than two plural forms (Chinese has one) are handled
/// by gettext's `ngettext`; the English `singular`/`plural` pair are the
/// message ids extracted into the catalog.
pub fn ngettext(singular: &str, plural: &str, n: u32) -> String {
    gettextrs::ngettext(singular, plural, n)
}

/// Expand a translated template with positional `{}` placeholders.
///
/// `format!` needs a literal format string, but translations are only known
/// at run time, so this does the substitution itself. As in `format!`, `{{`
/// and `}}` produce literal braces. A placeholder the translator got wrong
/// (say `{0}`) is copied through rather than panicking.
pub fn format_translated(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    let mut next = 0;

    while let Some(c) = chars.next() {
        match c {
            '{' => match chars.peek() {
                Some('{') => {
                    chars.next();
                    out.push('{');
                }
                Some('}') => {
                    chars.next();
                    match args.get(next) {
                        Some(arg) => {
                            let _ = write!(out, "{arg}");
                        }
                        // More placeholders than arguments: keep the
                        // placeholder visible instead of silently dropping
                        // text.
                        None => out.push_str("{}"),
                    }
                    next += 1;
                }
                _ => out.push('{'),
            },
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            _ => out.push(c),
        }
    }

    out
}

/// Translate a message and substitute its positional `{}` placeholders.
///
/// ```ignore
/// tr!("Connected to {}", host);
/// tr!("Cancel"); // no placeholders: same as gettext("Cancel")
/// ```
#[macro_export]
macro_rules! tr {
    ($msgid:expr $(,)?) => {
        $crate::i18n::gettext($msgid)
    };
    ($msgid:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format_translated(
            &$crate::i18n::gettext($msgid),
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::{format_translated, has_catalog, is_supported};

    #[test]
    fn only_shipped_languages_are_accepted() {
        assert!(is_supported("en"));
        assert!(is_supported("zh_CN"));
        // The saved value comes from a user-editable file: a path traversal
        // or an embedded NUL must never reach gettext.
        assert!(!is_supported(""));
        assert!(!is_supported("../../tmp"));
        assert!(!is_supported("zh_CN\0"));
        assert!(!is_supported("zh_CN.UTF-8"));
    }

    #[test]
    fn a_directory_only_counts_when_it_holds_a_catalog() {
        let dir = std::env::temp_dir().join(format!("linuxscp-i18n-{}", std::process::id()));
        let mo = dir.join("zh_CN").join("LC_MESSAGES").join("linuxscp.mo");
        assert!(!has_catalog(&dir));

        std::fs::create_dir_all(mo.parent().unwrap()).unwrap();
        std::fs::write(&mo, b"").unwrap();
        assert!(has_catalog(&dir));

        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!has_catalog(&dir));
    }

    #[test]
    fn substitutes_positional_placeholders_in_order() {
        assert_eq!(
            format_translated("{} of {} files", &[&1, &2]),
            "1 of 2 files"
        );
    }

    #[test]
    fn leaves_escaped_braces_alone() {
        assert_eq!(format_translated("{{}}", &[]), "{}");
        assert_eq!(format_translated("a }} b", &[]), "a } b");
    }

    #[test]
    fn missing_arguments_leave_the_template_intact() {
        assert_eq!(format_translated("{} {}", &[&"x"]), "x {}");
    }

    #[test]
    fn unknown_placeholder_syntax_is_copied_through() {
        assert_eq!(format_translated("{0}", &[&"x"]), "{0}");
    }
}
