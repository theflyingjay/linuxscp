//! Registry of live SFTP sessions, shared between the GTK thread (which
//! creates and closes sessions) and tokio tasks (which use them).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use russh_sftp::client::SftpSession;
use tokio_util::sync::CancellationToken;

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
}

impl SessionHandle {
    pub fn new(sftp: Arc<SftpSession>, cancel: CancellationToken, host: String) -> Self {
        Self {
            sftp,
            cancel,
            host,
            id_names: Arc::new(tokio::sync::OnceCell::new()),
        }
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
