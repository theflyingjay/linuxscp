//! Secret storage for saved-site passwords and key passphrases, backed by
//! the OS keyring (Secret Service / gnome-keyring over D-Bus) through
//! libsecret, GNOME's client library for it.
//!
//! Items carry two attributes: `service` (our app id) and `username` (the
//! opaque site id). Those are the attributes the `keyring` crate used before
//! this module switched to libsecret, and the schema is created with
//! `DONT_MATCH_NAME` so lookups match on them alone rather than on a schema
//! tag — passwords saved by earlier releases stay readable, no migration.
//! Lookups return whichever matching item the service lists first, not the
//! newest, so [`set`] removes the previous item (whatever extra attributes
//! it carried) before storing, keeping exactly one item per site.
//!
//! libsecret's sync calls block on a D-Bus round trip (and on the unlock
//! prompt when the keyring is locked). Each operation runs on a short-lived
//! dedicated OS thread, so it is safe to call from GTK handlers and tokio
//! tasks alike without entangling the caller's main context or runtime.

use std::collections::HashMap;

use libsecret::{Schema, SchemaAttributeType, SchemaFlags};

const SERVICE: &str = "io.github.theflyingjay.LinuxSCP";

pub type Error = glib::Error;

fn run<T, F>(f: F) -> Result<T, Error>
where
    F: FnOnce() -> Result<T, Error> + Send,
    T: Send,
{
    std::thread::scope(|scope| scope.spawn(f).join().expect("keyring thread panicked"))
}

fn schema() -> Schema {
    Schema::new(
        SERVICE,
        SchemaFlags::DONT_MATCH_NAME,
        HashMap::from([
            ("service", SchemaAttributeType::String),
            ("username", SchemaAttributeType::String),
        ]),
    )
}

fn attributes(site_id: &str) -> HashMap<&str, &str> {
    HashMap::from([("service", SERVICE), ("username", site_id)])
}

fn lookup(site_id: &str) -> Result<Option<glib::GString>, Error> {
    libsecret::password_lookup_sync(Some(&schema()), attributes(site_id), gio::Cancellable::NONE)
}

/// Remove every item for the site; not an error if there is none.
fn clear(site_id: &str) -> Result<(), Error> {
    // password_clear only deletes unlocked items. A lookup first unlocks a
    // matching item if the keyring is locked (prompting the user), so
    // nothing is left behind to shadow a new value.
    if lookup(site_id)?.is_some() {
        libsecret::password_clear_sync(
            Some(&schema()),
            attributes(site_id),
            gio::Cancellable::NONE,
        )?;
    }
    Ok(())
}

/// Store (or replace) the secret for a site. An empty secret clears it.
pub fn set(site_id: &str, secret: &str) -> Result<(), Error> {
    run(|| {
        clear(site_id)?;
        if secret.is_empty() {
            return Ok(());
        }
        libsecret::password_store_sync(
            Some(&schema()),
            attributes(site_id),
            Some(libsecret::COLLECTION_DEFAULT),
            &format!("LinuxSCP site {site_id}"),
            secret,
            gio::Cancellable::NONE,
        )
    })
}

/// Fetch the secret for a site, if one is stored.
pub fn get(site_id: &str) -> Option<String> {
    run(|| lookup(site_id))
        .ok()
        .flatten()
        .map(|secret| secret.to_string())
}

/// Delete the secret for a site (no error if absent).
pub fn delete(site_id: &str) {
    let _ = run(|| clear(site_id));
}
