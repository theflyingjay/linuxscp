//! gettext-based localization.
//!
//! User-visible strings are wrapped with [`tr!`](crate::tr), which translates
//! the English message id through the catalog installed as
//! `/usr/share/locale/<lang>/LC_MESSAGES/linuxscp.mo` (built from `po/`).
//! Strings without placeholders can also use [`gettext`] directly.
//!
//! Message ids are the English source strings, so a missing or partial
//! translation simply leaves that string in English.

use std::fmt::{Display, Write as _};

/// gettext text domain; must match the installed `.mo` file name.
pub const DOMAIN: &str = "linuxscp";

/// Where the catalogs are installed (`scripts/build-deb.sh` and the
/// `Makefile` both use the system locale directory).
const LOCALEDIR: &str = "/usr/share/locale";

/// Switch the process to the user's locale and bind the translation domain.
///
/// Call once, as early as possible in `main`. `gettext` resolves catalogs
/// for the current C locale, which stays "C" (no translations) until
/// `setlocale` runs; GTK makes the same call in `gtk_init`.
pub fn init() {
    // SAFETY: called once, before any threads are spawned, so no other
    // thread can observe the locale changing under it.
    unsafe {
        gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
    }
    let _ = gettextrs::bindtextdomain(DOMAIN, LOCALEDIR);
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = gettextrs::textdomain(DOMAIN);
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
                    if let Some(arg) = args.get(next) {
                        let _ = write!(out, "{arg}");
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
    use super::format_translated;

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
