//! Lightweight JSON settings persisted under the XDG config dir. We avoid
//! GSettings/gschema so the app runs from a plain `cargo run` with no
//! install step.
//!
//! The site manager stores an in-app tree of folders and saved connections
//! (WinSCP-style). Passwords and key passphrases are never written here;
//! they live in the OS keyring keyed by the site id (see [`crate::secrets`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{AuthMethod, ConnectSpec, Elevation};

/// A saved connection in the site manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub auth: AuthMethod,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub elevation: Elevation,
    #[serde(default)]
    pub remote_dir: Option<String>,
    /// True if a secret (password/passphrase) is stored in the keyring.
    #[serde(default)]
    pub has_secret: bool,
}

impl Site {
    pub fn new(name: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            host: host.into(),
            port: None,
            user: None,
            auth: AuthMethod::Agent,
            identity_file: None,
            elevation: Elevation::None,
            remote_dir: None,
            has_secret: false,
        }
    }

    /// Human-readable "user@host:port" summary for list subtitles.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if let Some(user) = self.user.as_deref().filter(|u| !u.is_empty()) {
            s.push_str(user);
            s.push('@');
        }
        s.push_str(&self.host);
        if let Some(port) = self.port {
            s.push_str(&format!(":{port}"));
        }
        s
    }

    /// Case-insensitive match against the name, host, and user, for the
    /// site-manager search box. `query` must already be lowercase.
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.name.to_lowercase().contains(query)
            || self.host.to_lowercase().contains(query)
            || self
                .user
                .as_deref()
                .is_some_and(|u| u.to_lowercase().contains(query))
    }

    /// Build a connection spec from this site. The secret (if any) is loaded
    /// separately by the caller and assigned to `spec.secret`.
    pub fn to_spec(&self) -> ConnectSpec {
        ConnectSpec {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone().filter(|u| !u.is_empty()),
            auth: self.auth,
            identity_file: self.identity_file.clone().filter(|p| !p.is_empty()),
            elevation: self.elevation,
            remote_dir: self.remote_dir.clone().filter(|d| !d.is_empty()),
            sftp_server_path: None,
            extra_ssh_args: Vec::new(),
            secret: None,
        }
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A folder in the site tree. Folders nest arbitrarily and hold sites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub sites: Vec<Site>,
}

impl Default for Folder {
    fn default() -> Self {
        // A fresh, unique id so programmatically-created folders never
        // collide (an empty id would match the root and every sibling).
        Self {
            id: new_id(),
            name: String::new(),
            folders: Vec::new(),
            sites: Vec::new(),
        }
    }
}

impl Folder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Depth-first iterator over every site in this subtree.
    pub fn all_sites(&self) -> Vec<&Site> {
        let mut out: Vec<&Site> = self.sites.iter().collect();
        for folder in &self.folders {
            out.extend(folder.all_sites());
        }
        out
    }

    /// Total number of sites in this subtree.
    pub fn site_count(&self) -> usize {
        self.sites.len() + self.folders.iter().map(|f| f.site_count()).sum::<usize>()
    }

    /// Mutable ref to the folder identified by `id`, falling back to self
    /// when the id is empty or not found. Handy for "add under parent (or
    /// root)" without tripping the borrow checker at the call site.
    pub fn folder_or_root_mut(&mut self, id: &str) -> &mut Folder {
        if id.is_empty() || self.find_folder_mut(id).is_none() {
            return self;
        }
        self.find_folder_mut(id).unwrap()
    }

    /// Find a folder by id anywhere in the subtree (including self).
    pub fn find_folder_mut(&mut self, id: &str) -> Option<&mut Folder> {
        if self.id == id {
            return Some(self);
        }
        for folder in &mut self.folders {
            if let Some(found) = folder.find_folder_mut(id) {
                return Some(found);
            }
        }
        None
    }

    /// Immutable [`Self::find_folder_mut`].
    pub fn find_folder(&self, id: &str) -> Option<&Folder> {
        if self.id == id {
            return Some(self);
        }
        self.folders.iter().find_map(|f| f.find_folder(id))
    }

    /// Remove a site by id from anywhere in the subtree; returns it.
    pub fn remove_site(&mut self, id: &str) -> Option<Site> {
        if let Some(pos) = self.sites.iter().position(|s| s.id == id) {
            return Some(self.sites.remove(pos));
        }
        for folder in &mut self.folders {
            if let Some(site) = folder.remove_site(id) {
                return Some(site);
            }
        }
        None
    }

    /// Update a site in place by id; returns true if found.
    pub fn update_site(&mut self, site: Site) -> bool {
        if let Some(existing) = self.sites.iter_mut().find(|s| s.id == site.id) {
            *existing = site;
            return true;
        }
        for folder in &mut self.folders {
            if folder.update_site(site.clone()) {
                return true;
            }
        }
        false
    }

    /// Id of the folder directly containing `child_id` (a site or subfolder).
    pub fn parent_id_of(&self, child_id: &str) -> Option<String> {
        if self.sites.iter().any(|s| s.id == child_id)
            || self.folders.iter().any(|f| f.id == child_id)
        {
            return Some(self.id.clone());
        }
        self.folders.iter().find_map(|f| f.parent_id_of(child_id))
    }

    /// Copy of this subtree keeping only what matches `query` (lowercase):
    /// matching sites, plus folders that match by name (kept whole) or that
    /// contain a match somewhere below. An empty query returns everything.
    pub fn filtered(&self, query: &str) -> Folder {
        if query.is_empty() {
            return self.clone();
        }
        let mut out = Folder {
            id: self.id.clone(),
            name: self.name.clone(),
            folders: Vec::new(),
            sites: Vec::new(),
        };
        for sub in &self.folders {
            if sub.name.to_lowercase().contains(query) {
                out.folders.push(sub.clone());
                continue;
            }
            let pruned = sub.filtered(query);
            if !pruned.folders.is_empty() || !pruned.sites.is_empty() {
                out.folders.push(pruned);
            }
        }
        for site in &self.sites {
            if site.matches(query) {
                out.sites.push(site.clone());
            }
        }
        out
    }

    /// Move a site into the folder `dest_id` (the root when empty or
    /// unknown). Returns true when the site actually moved.
    pub fn move_site_to(&mut self, site_id: &str, dest_id: &str) -> bool {
        let dest = self.folder_or_root_mut(dest_id).id.clone();
        if self.parent_id_of(site_id) == Some(dest.clone()) {
            return false;
        }
        let Some(site) = self.remove_site(site_id) else {
            return false;
        };
        self.folder_or_root_mut(&dest).sites.push(site);
        true
    }

    /// Move a folder (subtree and all, sites included) into the folder
    /// `dest_id` (the root when empty or unknown). Rejects moves into the
    /// folder itself or anywhere below it. Returns true when it moved.
    pub fn move_folder_to(&mut self, folder_id: &str, dest_id: &str) -> bool {
        if folder_id == self.id {
            return false; // the root itself never moves
        }
        let dest = self.folder_or_root_mut(dest_id).id.clone();
        // No-ops and cycles: onto itself, into its own subtree, or into the
        // folder it is already in.
        let Some(moving) = self.find_folder(folder_id) else {
            return false;
        };
        if moving.find_folder(&dest).is_some() {
            return false;
        }
        if self.parent_id_of(folder_id) == Some(dest.clone()) {
            return false;
        }
        let Some(folder) = self.remove_folder(folder_id) else {
            return false;
        };
        self.folder_or_root_mut(&dest).folders.push(folder);
        true
    }

    /// Remove a (non-root) folder by id from the subtree; returns it.
    pub fn remove_folder(&mut self, id: &str) -> Option<Folder> {
        if let Some(pos) = self.folders.iter().position(|f| f.id == id) {
            return Some(self.folders.remove(pos));
        }
        for folder in &mut self.folders {
            if let Some(removed) = folder.remove_folder(id) {
                return Some(removed);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub show_hidden: bool,
    /// Root of the site-manager tree; its own name is unused.
    #[serde(default)]
    pub sites: Folder,
    /// Legacy flat bookmarks, migrated into `sites` on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<LegacyBookmark>,
    /// Preferred terminal command; `None` = auto-detect.
    #[serde(default)]
    pub terminal: Option<String>,
    #[serde(default)]
    pub left_width: Option<i32>,
    /// Play a sound when a transfer completes.
    #[serde(default = "enabled")]
    pub notify_sound: bool,
    /// Send a desktop notification when a transfer completes.
    #[serde(default = "enabled")]
    pub notify_desktop: bool,
}

fn enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_hidden: false,
            sites: Folder::default(),
            bookmarks: Vec::new(),
            terminal: None,
            left_width: None,
            notify_sound: true,
            notify_desktop: true,
        }
    }
}

/// Old bookmark format (pre-site-manager). Kept only for migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyBookmark {
    pub label: String,
    pub host: String,
    #[serde(default)]
    pub elevation: Elevation,
    #[serde(default)]
    pub remote_dir: Option<String>,
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("linuxscp");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let mut settings: Settings = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        settings.migrate_bookmarks();
        settings
    }

    /// Fold any legacy flat bookmarks into the site tree root, once.
    fn migrate_bookmarks(&mut self) {
        if self.bookmarks.is_empty() {
            return;
        }
        for bookmark in std::mem::take(&mut self.bookmarks) {
            let mut site = Site::new(bookmark.label, bookmark.host);
            site.elevation = bookmark.elevation;
            site.remote_dir = bookmark.remote_dir;
            self.sites.sites.push(site);
        }
        self.save();
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(config_path(), json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_tree_operations() {
        let mut root = Folder::default();
        let mut prod = Folder {
            name: "Production".into(),
            ..Default::default()
        };
        let prod_id = prod.id.clone();
        prod.sites.push(Site::new("web", "web.example.com"));
        root.folders.push(prod);
        root.sites.push(Site::new("laptop", "laptop.local"));

        assert_eq!(root.site_count(), 2);
        assert_eq!(root.all_sites().len(), 2);

        // Add a site into the nested folder.
        let target = root.find_folder_mut(&prod_id).unwrap();
        target.sites.push(Site::new("db", "db.example.com"));
        assert_eq!(root.site_count(), 3);

        // Remove a nested site.
        let web_id = root.folders[0].sites[0].id.clone();
        let removed = root.remove_site(&web_id).unwrap();
        assert_eq!(removed.name, "web");
        assert_eq!(root.site_count(), 2);

        // Remove the folder (and its remaining site).
        root.remove_folder(&prod_id).unwrap();
        assert_eq!(root.site_count(), 1);
    }

    #[test]
    fn move_site_between_folders() {
        let mut root = Folder::default();
        let prod = Folder::new("Production");
        let prod_id = prod.id.clone();
        root.folders.push(prod);
        root.sites.push(Site::new("web", "web.example.com"));
        let site_id = root.sites[0].id.clone();

        // Root -> folder.
        assert!(root.move_site_to(&site_id, &prod_id));
        assert!(root.sites.is_empty());
        assert_eq!(root.folders[0].sites.len(), 1);

        // Already there: no-op.
        assert!(!root.move_site_to(&site_id, &prod_id));

        // Folder -> root ("" and the root id both mean root).
        assert!(root.move_site_to(&site_id, ""));
        assert_eq!(root.sites.len(), 1);
        assert!(root.folders[0].sites.is_empty());

        // Unknown site id: no-op.
        assert!(!root.move_site_to("missing", &prod_id));
    }

    #[test]
    fn move_folder_carries_subtree_and_rejects_cycles() {
        let mut root = Folder::default();
        let mut prod = Folder::new("Production");
        let prod_id = prod.id.clone();
        let mut backups = Folder::new("Backups");
        let backups_id = backups.id.clone();
        backups.sites.push(Site::new("vault", "vault.internal"));
        prod.folders.push(backups);
        root.folders.push(prod);
        let home = Folder::new("Homelab");
        let home_id = home.id.clone();
        root.folders.push(home);

        // Move Backups (with its site) from Production into Homelab.
        assert!(root.move_folder_to(&backups_id, &home_id));
        assert!(root.find_folder(&prod_id).unwrap().folders.is_empty());
        let moved = &root.find_folder(&home_id).unwrap().folders[0];
        assert_eq!(moved.name, "Backups");
        assert_eq!(moved.sites.len(), 1, "sites must travel with the folder");

        // Cycles: into itself, or into its own subtree.
        assert!(!root.move_folder_to(&home_id, &home_id));
        assert!(!root.move_folder_to(&home_id, &backups_id));

        // Already in the destination: no-op.
        assert!(!root.move_folder_to(&backups_id, &home_id));

        // Folder -> root, and the root itself never moves.
        assert!(root.move_folder_to(&backups_id, ""));
        assert!(root.folders.iter().any(|f| f.id == backups_id));
        let root_id = root.id.clone();
        assert!(!root.move_folder_to(&root_id, &home_id));
    }

    #[test]
    fn search_filters_sites_and_folders() {
        let mut root = Folder::default();
        let mut prod = Folder::new("Production");
        prod.sites.push(Site::new("web", "web.example.com"));
        prod.sites.push(Site::new("db", "db.internal"));
        let mut nested = Folder::new("Backups");
        nested.sites.push(Site::new("vault", "vault.internal"));
        prod.folders.push(nested);
        root.folders.push(prod);
        let mut laptop = Site::new("laptop", "laptop.local");
        laptop.user = Some("Jacob".into());
        root.sites.push(laptop);

        // Empty query returns everything.
        assert_eq!(root.filtered("").site_count(), 4);

        // Site match by name keeps only the matching branch.
        let hit = root.filtered("web");
        assert_eq!(hit.site_count(), 1);
        assert_eq!(hit.folders[0].sites[0].name, "web");

        // Match by host and by user (case-insensitive).
        assert_eq!(root.filtered("internal").site_count(), 2);
        assert_eq!(root.filtered("jacob").site_count(), 1);

        // Folder-name match keeps the whole subtree.
        let by_folder = root.filtered("backup");
        assert_eq!(by_folder.site_count(), 1);
        assert_eq!(by_folder.folders[0].folders[0].name, "Backups");

        // No match prunes everything.
        let none = root.filtered("nomatch");
        assert_eq!(none.site_count(), 0);
        assert!(none.folders.is_empty());
    }

    #[test]
    fn site_summary_and_spec() {
        let mut site = Site::new("prod", "example.com");
        site.user = Some("deploy".into());
        site.port = Some(2222);
        assert_eq!(site.summary(), "deploy@example.com:2222");
        let spec = site.to_spec();
        assert_eq!(spec.user.as_deref(), Some("deploy"));
        assert_eq!(spec.port, Some(2222));
    }
}
