//! Unified filesystem operations over the local disk and remote SFTP
//! sessions. Every function is async and safe to call from any tokio task.

use std::path::PathBuf;

use anyhow::Context;
use russh_sftp::protocol::FileType;

use crate::sessions::{self, IdNames};
use crate::types::{Backend, FsEntry, SessionId};

fn sftp(id: SessionId) -> anyhow::Result<std::sync::Arc<russh_sftp::client::SftpSession>> {
    Ok(sessions::get(id).context("session is closed")?.sftp)
}

/// The session's uid/gid → name maps, loading them from the remote
/// /etc/passwd and /etc/group on first use. Unreadable files simply leave
/// the maps empty and listings fall back to numeric ids.
async fn id_names(id: SessionId) -> Option<std::sync::Arc<tokio::sync::OnceCell<IdNames>>> {
    let handle = sessions::get(id)?;
    let cell = handle.id_names.clone();
    let sftp = handle.sftp.clone();
    cell.get_or_init(|| async move {
        let read = |path: &'static str| {
            let sftp = sftp.clone();
            async move {
                sftp.read(path)
                    .await
                    .ok()
                    .map(|bytes| IdNames::parse_id_file(&String::from_utf8_lossy(&bytes)))
                    .unwrap_or_default()
            }
        };
        IdNames {
            users: read("/etc/passwd").await,
            groups: read("/etc/group").await,
        }
    })
    .await;
    Some(cell)
}

/// Join a child onto a directory path, for both local and remote paths.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent directory of a path ("/" is its own parent).
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

pub fn file_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

pub async fn list_dir(backend: Backend, path: &str) -> anyhow::Result<Vec<FsEntry>> {
    match backend {
        Backend::Local => {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || local::list_dir(&path)).await?
        }
        Backend::Remote(id) => {
            let sftp = sftp(id)?;
            let names_cell = id_names(id).await;
            let names = names_cell.as_ref().and_then(|cell| cell.get());
            remote::list_dir(&sftp, path, names).await
        }
    }
}

pub async fn home_dir(backend: Backend) -> anyhow::Result<String> {
    match backend {
        Backend::Local => Ok(dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .to_string_lossy()
            .into_owned()),
        Backend::Remote(id) => Ok(sftp(id)?.canonicalize(".").await?),
    }
}

pub async fn canonicalize(backend: Backend, path: &str) -> anyhow::Result<String> {
    match backend {
        Backend::Local => Ok(tokio::fs::canonicalize(path)
            .await?
            .to_string_lossy()
            .into_owned()),
        Backend::Remote(id) => Ok(sftp(id)?.canonicalize(path).await?),
    }
}

pub async fn stat(backend: Backend, path: &str) -> anyhow::Result<FsEntry> {
    match backend {
        Backend::Local => {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || {
                let meta = std::fs::symlink_metadata(&path)?;
                Ok(local::entry_from_meta(&path, &meta))
            })
            .await?
        }
        Backend::Remote(id) => {
            let attrs = sftp(id)?.symlink_metadata(path.to_owned()).await?;
            let names_cell = id_names(id).await;
            let names = names_cell.as_ref().and_then(|cell| cell.get());
            Ok(remote::entry_from_attrs(
                path,
                file_name(path),
                &attrs,
                names,
            ))
        }
    }
}

pub async fn exists(backend: Backend, path: &str) -> bool {
    stat(backend, path).await.is_ok()
}

pub async fn mkdir(backend: Backend, path: &str) -> anyhow::Result<()> {
    match backend {
        Backend::Local => Ok(tokio::fs::create_dir(path).await?),
        Backend::Remote(id) => Ok(sftp(id)?.create_dir(path.to_owned()).await?),
    }
}

pub async fn mkdir_all(backend: Backend, path: &str) -> anyhow::Result<()> {
    match backend {
        Backend::Local => Ok(tokio::fs::create_dir_all(path).await?),
        Backend::Remote(id) => {
            let sftp = sftp(id)?;
            let mut current = String::new();
            for part in path.split('/').filter(|p| !p.is_empty()) {
                current.push('/');
                current.push_str(part);
                match sftp.metadata(current.clone()).await {
                    Ok(_) => continue,
                    Err(_) => sftp
                        .create_dir(current.clone())
                        .await
                        .with_context(|| format!("creating {current}"))?,
                }
            }
            Ok(())
        }
    }
}

pub async fn rename(backend: Backend, from: &str, to: &str) -> anyhow::Result<()> {
    match backend {
        Backend::Local => Ok(tokio::fs::rename(from, to).await?),
        Backend::Remote(id) => Ok(sftp(id)?.rename(from.to_owned(), to.to_owned()).await?),
    }
}

pub async fn chmod(backend: Backend, path: &str, mode: u32) -> anyhow::Result<()> {
    match backend {
        Backend::Local => {
            use std::os::unix::fs::PermissionsExt;
            Ok(tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?)
        }
        Backend::Remote(id) => {
            let sftp = sftp(id)?;
            let mut attrs = sftp.metadata(path.to_owned()).await?;
            // Preserve the file-type bits, replace the permission bits.
            let type_bits = attrs.permissions.unwrap_or(0) & !0o7777;
            attrs.permissions = Some(type_bits | (mode & 0o7777));
            attrs.size = None;
            attrs.uid = None;
            attrs.gid = None;
            attrs.atime = None;
            attrs.mtime = None;
            Ok(sftp.set_metadata(path.to_owned(), attrs).await?)
        }
    }
}

pub async fn chown(
    backend: Backend,
    path: &str,
    uid: Option<u32>,
    gid: Option<u32>,
) -> anyhow::Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    match backend {
        Backend::Local => {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || std::os::unix::fs::chown(path, uid, gid)).await??;
            Ok(())
        }
        Backend::Remote(id) => {
            let sftp = sftp(id)?;
            // SFTPv3 sends uid and gid together, so fill the missing half
            // from the file's current attributes.
            let current = sftp.metadata(path.to_owned()).await?;
            let mut attrs = russh_sftp::protocol::FileAttributes::empty();
            attrs.uid = uid.or(current.uid);
            attrs.gid = gid.or(current.gid);
            if attrs.uid.is_none() || attrs.gid.is_none() {
                anyhow::bail!("server did not report current owner ids for {path}");
            }
            Ok(sftp.set_metadata(path.to_owned(), attrs).await?)
        }
    }
}

/// Resolve user/group names to numeric ids on the given backend. Accepts
/// plain numeric ids too. `None` inputs stay `None`.
pub async fn resolve_ids(
    backend: Backend,
    user: Option<String>,
    group: Option<String>,
) -> anyhow::Result<(Option<u32>, Option<u32>)> {
    let parse = |name: &str| name.parse::<u32>().ok();

    let uid = match &user {
        None => None,
        Some(name) => Some(match parse(name) {
            Some(id) => id,
            None => lookup_id(backend, name, true)
                .await
                .with_context(|| format!("unknown user \u{201c}{name}\u{201d}"))?,
        }),
    };
    let gid = match &group {
        None => None,
        Some(name) => Some(match parse(name) {
            Some(id) => id,
            None => lookup_id(backend, name, false)
                .await
                .with_context(|| format!("unknown group \u{201c}{name}\u{201d}"))?,
        }),
    };
    Ok((uid, gid))
}

async fn lookup_id(backend: Backend, name: &str, is_user: bool) -> anyhow::Result<u32> {
    match backend {
        Backend::Local => {
            let file = if is_user { "/etc/passwd" } else { "/etc/group" };
            local::id_for(name, file).context("not found")
        }
        Backend::Remote(id) => {
            let names_cell = id_names(id).await.context("session is closed")?;
            let names = names_cell.get().context("id maps unavailable")?;
            let resolved = if is_user {
                names.uid_of(name)
            } else {
                names.gid_of(name)
            };
            resolved.context("not found")
        }
    }
}

/// Attribute changes applied by the Properties dialog.
#[derive(Debug, Clone, Copy, Default)]
pub struct AttrChanges {
    /// Permission bits to set (12 bits: rwx ×3 + setuid/setgid/sticky).
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Give directories execute wherever they get read (WinSCP's
    /// "Add X to directories").
    pub add_x_dirs: bool,
}

impl AttrChanges {
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.uid.is_none() && self.gid.is_none()
    }

    fn mode_for(&self, is_dir: bool) -> Option<u32> {
        self.mode.map(|mode| {
            if is_dir && self.add_x_dirs {
                mode | ((mode & 0o444) >> 2)
            } else {
                mode
            }
        })
    }
}

/// Apply owner/group/permission changes to `roots`, and — when `recursive`
/// — to everything below the selected directories. Symlinks are left
/// untouched (and never followed). Returns how many items were changed.
pub async fn apply_attrs(
    backend: Backend,
    roots: &[FsEntry],
    changes: &AttrChanges,
    recursive: bool,
) -> anyhow::Result<u64> {
    if changes.is_empty() {
        return Ok(0);
    }

    // Collect the full work list up front: applying a restrictive mode to a
    // directory first could make its children unlistable.
    let mut work: Vec<(String, bool)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for entry in roots {
        if entry.is_symlink {
            continue;
        }
        work.push((entry.path.clone(), entry.is_dir));
        if entry.is_dir && recursive {
            dirs.push(entry.path.clone());
        }
    }
    while let Some(dir) = dirs.pop() {
        let children = list_dir(backend, &dir)
            .await
            .with_context(|| format!("listing {dir}"))?;
        for child in children {
            if child.is_symlink {
                continue;
            }
            if child.is_dir {
                dirs.push(child.path.clone());
            }
            work.push((child.path, child.is_dir));
        }
    }

    let mut changed = 0u64;
    for (path, is_dir) in work {
        if let Some(mode) = changes.mode_for(is_dir) {
            chmod(backend, &path, mode)
                .await
                .with_context(|| format!("changing permissions of {path}"))?;
        }
        chown(backend, &path, changes.uid, changes.gid)
            .await
            .with_context(|| format!("changing owner of {path}"))?;
        changed += 1;
    }
    Ok(changed)
}

pub async fn read_link(backend: Backend, path: &str) -> anyhow::Result<String> {
    match backend {
        Backend::Local => Ok(tokio::fs::read_link(path)
            .await?
            .to_string_lossy()
            .into_owned()),
        Backend::Remote(id) => Ok(sftp(id)?.read_link(path.to_owned()).await?),
    }
}

/// Create a symbolic link at `link` pointing to `target`.
pub async fn symlink(backend: Backend, link: &str, target: &str) -> anyhow::Result<()> {
    match backend {
        Backend::Local => Ok(tokio::fs::symlink(target, link).await?),
        Backend::Remote(id) => {
            // OpenSSH's sftp-server historically swaps SSH_FXP_SYMLINK's
            // linkpath/targetpath relative to the draft standard, so the
            // first path on the wire must be the *target*. russh-sftp sends
            // its first argument first — hence target, then link.
            Ok(sftp(id)?
                .symlink(target.to_owned(), link.to_owned())
                .await?)
        }
    }
}

/// Totals for a set of entries: every contained file and directory,
/// recursively, without following directory symlinks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

pub async fn disk_usage(backend: Backend, roots: &[FsEntry]) -> anyhow::Result<DiskUsage> {
    let mut usage = DiskUsage::default();
    let mut stack: Vec<String> = Vec::new();
    for entry in roots {
        if entry.is_dir && !entry.is_symlink {
            usage.dirs += 1;
            stack.push(entry.path.clone());
        } else {
            usage.files += 1;
            usage.bytes += entry.size;
        }
    }
    while let Some(dir) = stack.pop() {
        let children = match list_dir(backend, &dir).await {
            Ok(children) => children,
            // Unreadable subdirectory: skip it rather than fail the total.
            Err(_) => continue,
        };
        for child in children {
            if child.is_dir && !child.is_symlink {
                usage.dirs += 1;
                stack.push(child.path);
            } else {
                usage.files += 1;
                usage.bytes += child.size;
            }
        }
    }
    Ok(usage)
}

/// Recursively delete a file or directory.
pub async fn delete(backend: Backend, entry: &FsEntry) -> anyhow::Result<()> {
    match backend {
        Backend::Local => {
            let path = entry.path.clone();
            let is_dir = entry.is_dir && !entry.is_symlink;
            tokio::task::spawn_blocking(move || {
                if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                }
            })
            .await??;
            Ok(())
        }
        Backend::Remote(id) => {
            let sftp = sftp(id)?;
            delete_remote(&sftp, &entry.path, entry.is_dir && !entry.is_symlink).await
        }
    }
}

async fn delete_remote(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    is_dir: bool,
) -> anyhow::Result<()> {
    if !is_dir {
        sftp.remove_file(path.to_owned())
            .await
            .with_context(|| format!("deleting {path}"))?;
        return Ok(());
    }
    // Iterative DFS to avoid async recursion boxing.
    let mut stack: Vec<(String, bool)> = vec![(path.to_owned(), false)];
    while let Some((dir, children_done)) = stack.pop() {
        if children_done {
            sftp.remove_dir(dir.clone())
                .await
                .with_context(|| format!("removing directory {dir}"))?;
            continue;
        }
        stack.push((dir.clone(), true));
        let entries = sftp.read_dir(dir.clone()).await?;
        for entry in entries {
            let child = join(&dir, &entry.file_name());
            let file_type = entry.file_type();
            if file_type.is_dir() {
                stack.push((child, false));
            } else {
                sftp.remove_file(child.clone())
                    .await
                    .with_context(|| format!("deleting {child}"))?;
            }
        }
    }
    Ok(())
}

pub mod local {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};

    pub fn list_dir(path: &str) -> anyhow::Result<Vec<FsEntry>> {
        let read = std::fs::read_dir(path).with_context(|| format!("reading {path}"))?;
        let mut out = Vec::new();
        for entry in read.flatten() {
            let child = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            out.push(entry_from_meta(&child.to_string_lossy(), &meta));
        }
        Ok(out)
    }

    pub fn entry_from_meta(path: &str, meta: &std::fs::Metadata) -> FsEntry {
        let is_symlink = meta.is_symlink();
        // For symlinks show the target's kind so double-click works.
        let target_meta = if is_symlink {
            std::fs::metadata(path).ok()
        } else {
            None
        };
        let effective = target_meta.as_ref().unwrap_or(meta);
        FsEntry {
            name: file_name(path).to_owned(),
            path: path.to_owned(),
            is_dir: effective.is_dir(),
            is_symlink,
            size: effective.len(),
            mtime: Some(meta.mtime()),
            mode: Some(meta.mode() & 0o7777),
            owner: Some(user_name(meta.uid())),
            group: Some(group_name(meta.gid())),
            link_target: is_symlink
                .then(|| std::fs::read_link(path).ok())
                .flatten()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }

    fn user_name(uid: u32) -> String {
        lookup(uid, "/etc/passwd")
    }

    fn group_name(gid: u32) -> String {
        lookup(gid, "/etc/group")
    }

    /// id → name via the given passwd/group-format file, loaded once per
    /// process; unknown ids fall back to the number.
    fn lookup(id: u32, file: &str) -> String {
        with_id_map(file, |map| map.get(&id).cloned()).unwrap_or_else(|| id.to_string())
    }

    /// name → id via the same cached passwd/group maps.
    pub fn id_for(name: &str, file: &str) -> Option<u32> {
        with_id_map(file, |map| {
            map.iter()
                .find(|(_, entry)| entry.as_str() == name)
                .map(|(id, _)| *id)
        })
    }

    fn with_id_map<T>(file: &str, f: impl FnOnce(&HashMap<u32, String>) -> T) -> T {
        static CACHE: OnceLock<Mutex<HashMap<String, HashMap<u32, String>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        let mut cache = cache.lock().unwrap();
        let map = cache.entry(file.to_owned()).or_insert_with(|| {
            std::fs::read_to_string(file)
                .map(|content| IdNames::parse_id_file(&content))
                .unwrap_or_default()
        });
        f(map)
    }
}

pub mod remote {
    use super::*;
    use russh_sftp::protocol::FileAttributes;

    pub async fn list_dir(
        sftp: &russh_sftp::client::SftpSession,
        path: &str,
        names: Option<&IdNames>,
    ) -> anyhow::Result<Vec<FsEntry>> {
        let entries = sftp.read_dir(path.to_owned()).await?;
        let mut out = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let full = join(path, &name);
            let attrs = entry.metadata();
            let mut fs_entry = entry_from_attrs(&full, &name, &attrs, names);
            // Show what symlinks point at (dir-ness drives navigation).
            if fs_entry.is_symlink {
                if let Ok(target_attrs) = sftp.metadata(full.clone()).await {
                    fs_entry.is_dir = target_attrs.file_type().is_dir();
                }
                fs_entry.link_target = sftp.read_link(full.clone()).await.ok();
            }
            out.push(fs_entry);
        }
        Ok(out)
    }

    pub fn entry_from_attrs(
        path: &str,
        name: &str,
        attrs: &FileAttributes,
        names: Option<&IdNames>,
    ) -> FsEntry {
        let file_type: FileType = attrs.file_type();
        // Prefer names the server sent, then the session's /etc/passwd and
        // /etc/group maps, then the raw numeric ids.
        let owner = attrs.user.clone().or_else(|| {
            attrs.uid.map(|uid| {
                names
                    .and_then(|n| n.users.get(&uid).cloned())
                    .unwrap_or_else(|| uid.to_string())
            })
        });
        let group = attrs.group.clone().or_else(|| {
            attrs.gid.map(|gid| {
                names
                    .and_then(|n| n.groups.get(&gid).cloned())
                    .unwrap_or_else(|| gid.to_string())
            })
        });
        FsEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
            size: attrs.size.unwrap_or(0),
            mtime: attrs.mtime.map(|t| t as i64),
            mode: attrs.permissions.map(|p| p & 0o7777),
            owner,
            group,
            link_target: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(join("/a/b", "c"), "/a/b/c");
        assert_eq!(join("/", "c"), "/c");
        assert_eq!(parent("/a/b/c"), "/a/b");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(file_name("/a/b/c.txt"), "c.txt");
        assert_eq!(file_name("/"), "");
    }

    #[test]
    fn local_listing_works() {
        let dir = std::env::temp_dir().join("linuxscp-test-local");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), b"hello").unwrap();
        let entries = local::list_dir(&dir.to_string_lossy()).unwrap();
        let file = entries.iter().find(|e| e.name == "file.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 5);
        assert!(entries.iter().any(|e| e.name == "sub" && e.is_dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
