//! Secret storage for saved-site passwords and key passphrases, backed by
//! the OS keyring (Secret Service / gnome-keyring via D-Bus).
//!
//! The `keyring` crate's secret-service backend drives its own async runtime
//! internally, which panics if called from within our tokio runtime. Every
//! operation therefore runs on a short-lived dedicated OS thread that has no
//! ambient runtime, which is safe to call from GTK handlers or tokio tasks.

const SERVICE: &str = "io.github.linuxscp.LinuxSCP";

fn run<T, F>(f: F) -> keyring::Result<T>
where
    F: FnOnce() -> keyring::Result<T> + Send,
    T: Send,
{
    std::thread::scope(|scope| scope.spawn(f).join().expect("keyring thread panicked"))
}

fn entry(site_id: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, site_id)
}

/// Store (or replace) the secret for a site. An empty secret clears it.
pub fn set(site_id: &str, secret: &str) -> keyring::Result<()> {
    let site_id = site_id.to_owned();
    let secret = secret.to_owned();
    run(move || {
        if secret.is_empty() {
            return match entry(&site_id)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e),
            };
        }
        entry(&site_id)?.set_password(&secret)
    })
}

/// Fetch the secret for a site, if one is stored.
pub fn get(site_id: &str) -> Option<String> {
    let site_id = site_id.to_owned();
    run(move || entry(&site_id)?.get_password()).ok()
}

/// Delete the secret for a site (no error if absent).
pub fn delete(site_id: &str) {
    let site_id = site_id.to_owned();
    let _ = run(move || match entry(&site_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e),
    });
}
