//! Registry of live SFTP sessions, shared between the GTK thread (which
//! creates and closes sessions) and tokio tasks (which use them).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use russh_sftp::client::SftpSession;
use tokio_util::sync::CancellationToken;

use crate::ssh::connect::{REQUEST_TIMEOUT_SECS, TRANSFER_TIMEOUT_SECS};
use crate::types::SessionId;

/// uid/gid → name maps read from the remote /etc/passwd and /etc/group, so
/// listings can show "root" instead of "0". Loaded lazily, once per session.
#[derive(Debug, Default)]
pub struct IdNames {
    pub users: HashMap<u32, String>,
    pub groups: HashMap<u32, String>,
}

impl IdNames {
    /// Parse `name:x:id:...` lines (the shared /etc/passwd and /etc/group
    /// format). Malformed lines are skipped.
    pub fn parse_id_file(content: &str) -> HashMap<u32, String> {
        content
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(':');
                let name = parts.next()?;
                parts.next()?; // password field
                let id = parts.next()?.parse::<u32>().ok()?;
                Some((id, name.to_owned()))
            })
            .collect()
    }

    pub fn uid_of(&self, name: &str) -> Option<u32> {
        Self::id_by_name(&self.users, name)
    }

    pub fn gid_of(&self, name: &str) -> Option<u32> {
        Self::id_by_name(&self.groups, name)
    }

    fn id_by_name(map: &HashMap<u32, String>, name: &str) -> Option<u32> {
        map.iter()
            .find(|(_, entry)| entry.as_str() == name)
            .map(|(id, _)| *id)
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub sftp: Arc<SftpSession>,
    pub cancel: CancellationToken,
    pub host: String,
    /// Lazily-loaded remote id → name maps (see [`IdNames`]).
    pub id_names: Arc<tokio::sync::OnceCell<IdNames>>,
    /// Held while [`Self::send_write`] has the session's timeout switched.
    write_gate: Arc<Mutex<()>>,
}

impl SessionHandle {
    pub fn new(sftp: Arc<SftpSession>, cancel: CancellationToken, host: String) -> Self {
        Self {
            sftp,
            cancel,
            host,
            id_names: Arc::new(tokio::sync::OnceCell::new()),
            write_gate: Arc::default(),
        }
    }

    /// Send a write of file data: a request that gets
    /// [`TRANSFER_TIMEOUT_SECS`] to be answered, not the request timeout
    /// the session applies to everything else.
    ///
    /// russh-sftp has one timeout per session, and each request keeps the
    /// value that was in force when it was sent. So the session is switched
    /// over for just as long as `send` runs, and `send` must do nothing but
    /// queue the write, which takes microseconds. The gate keeps two
    /// transfers on one session from interleaving their switches. A request
    /// that another task sends during those microseconds gets the longer
    /// timeout as well, which would only show if the server then never
    /// answered it.
    pub(crate) fn send_write<T>(&self, send: impl FnOnce() -> T) -> T {
        /// Puts the request timeout back and only then, as its field is
        /// dropped, opens the gate to the next write.
        struct Switched<'a> {
            sftp: &'a SftpSession,
            _gate: MutexGuard<'a, ()>,
        }
        impl Drop for Switched<'_> {
            fn drop(&mut self) {
                self.sftp.set_timeout(REQUEST_TIMEOUT_SECS);
            }
        }
        // A poisoned gate means a transfer panicked while sending; the
        // timeout was put back as it unwound, so carry on.
        let gate = self
            .write_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _switched = Switched {
            sftp: &self.sftp,
            _gate: gate,
        };
        self.sftp.set_timeout(TRANSFER_TIMEOUT_SECS);
        send()
    }
}

fn registry() -> &'static Mutex<HashMap<SessionId, SessionHandle>> {
    static REGISTRY: OnceLock<Mutex<HashMap<SessionId, SessionHandle>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub fn register(id: SessionId, handle: SessionHandle) {
    registry().lock().unwrap().insert(id, handle);
}

pub fn get(id: SessionId) -> Option<SessionHandle> {
    registry().lock().unwrap().get(&id).cloned()
}

/// Remove the session and kill its ssh process.
pub fn close(id: SessionId) {
    if let Some(handle) = registry().lock().unwrap().remove(&id) {
        handle.cancel.cancel();
    }
}

/// Remove a dead session without touching the process (it already exited).
pub fn forget(id: SessionId) {
    registry().lock().unwrap().remove(&id);
}

pub fn next_id() -> SessionId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_passwd_format() {
        let map = IdNames::parse_id_file(
            "root:x:0:0:root:/root:/bin/bash\n\
             daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
             malformed line\n\
             jacob:x:1000:1000::/home/jacob:/bin/bash\n",
        );
        assert_eq!(map.get(&0).map(String::as_str), Some("root"));
        assert_eq!(map.get(&1000).map(String::as_str), Some("jacob"));
        assert_eq!(map.len(), 3);
    }
}
