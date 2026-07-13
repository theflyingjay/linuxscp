//! Minimal ~/.ssh/config reader.
//!
//! We never interpret options ourselves — the ssh binary does that — we only
//! collect concrete `Host` aliases (and a few display hints) so the connect
//! dialog can offer the user's existing hosts, exactly like Tabby does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
}

impl KnownHost {
    /// "user@hostname" style subtitle for the host list.
    pub fn subtitle(&self) -> String {
        let host = self.hostname.as_deref().unwrap_or(&self.alias);
        match &self.user {
            Some(user) => format!("{user}@{host}"),
            None => host.to_owned(),
        }
    }

    /// Case-insensitive match against the alias, hostname, and user, for
    /// the site-manager search box. `query` must already be lowercase.
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.alias.to_lowercase().contains(query)
            || self
                .hostname
                .as_deref()
                .is_some_and(|h| h.to_lowercase().contains(query))
            || self
                .user
                .as_deref()
                .is_some_and(|u| u.to_lowercase().contains(query))
    }
}

pub fn ssh_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
}

/// All concrete host aliases from ~/.ssh/config, following `Include`s.
pub fn known_hosts() -> Vec<KnownHost> {
    let mut hosts = Vec::new();
    let mut visited = HashSet::new();
    parse_file(&ssh_dir().join("config"), &mut hosts, &mut visited, 0);
    hosts
}

fn parse_file(path: &Path, hosts: &mut Vec<KnownHost>, visited: &mut HashSet<PathBuf>, depth: u32) {
    if depth > 16 || !visited.insert(path.to_path_buf()) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };

    // Indices into `hosts` for the aliases of the current Host block, so
    // HostName/User lines can be attached to them.
    let mut current: Vec<usize> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match split_keyword(line) {
            Some(kv) => kv,
            None => continue,
        };

        match key.to_ascii_lowercase().as_str() {
            "host" => {
                current.clear();
                for alias in tokenize(value) {
                    if is_concrete_alias(&alias) {
                        current.push(hosts.len());
                        hosts.push(KnownHost {
                            alias,
                            hostname: None,
                            user: None,
                        });
                    }
                }
            }
            "match" => current.clear(),
            "hostname" => {
                for &i in &current {
                    hosts[i].hostname.get_or_insert_with(|| value.to_owned());
                }
            }
            "user" => {
                for &i in &current {
                    hosts[i].user.get_or_insert_with(|| value.to_owned());
                }
            }
            "include" => {
                for pattern in tokenize(value) {
                    for included in resolve_include(&pattern, path) {
                        parse_file(&included, hosts, visited, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Key value`, `Key=value` and `Key = value` forms.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = &line[..idx];
    let rest = line[idx..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    Some((key, rest.trim()))
}

/// Split a value into whitespace-separated tokens honoring double quotes.
fn tokenize(value: &str) -> Vec<String> {
    shell_words::split(value).unwrap_or_else(|_| {
        value
            .split_whitespace()
            .map(|s| s.to_owned())
            .collect::<Vec<_>>()
    })
}

/// Patterns and negations are not connectable entries.
fn is_concrete_alias(alias: &str) -> bool {
    !alias.contains(['*', '?']) && !alias.starts_with('!')
}

fn resolve_include(pattern: &str, from_file: &Path) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = pattern.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        // Relative includes are relative to the including file's directory
        // per ssh_config(5) (in practice ~/.ssh).
        from_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(pattern)
    };

    // Cheap glob support: only expand a `*` in the file name component,
    // which covers the ubiquitous `Include config.d/*` case.
    let name = expanded
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains('*') {
        return vec![expanded];
    }
    let Some(parent) = expanded.parent() else {
        return Vec::new();
    };
    let (prefix, suffix) = name.split_once('*').unwrap();
    let mut out: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| {
                            let n = n.to_string_lossy();
                            n.starts_with(prefix)
                                && n.ends_with(suffix)
                                && n.len() >= prefix.len() + suffix.len()
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(content: &str, dir: &Path) -> Vec<KnownHost> {
        let file = dir.join("config");
        std::fs::write(&file, content).unwrap();
        let mut hosts = Vec::new();
        let mut visited = HashSet::new();
        parse_file(&file, &mut hosts, &mut visited, 0);
        hosts
    }

    #[test]
    fn parses_hosts_and_hints() {
        let dir = std::env::temp_dir().join("linuxscp-test-sshcfg");
        std::fs::create_dir_all(&dir).unwrap();
        let hosts = parse_str(
            "# comment\n\
             Host web db.example.com\n\
             \tHostName 10.0.0.5\n\
             \tUser deploy\n\
             Host *\n\
             \tUser nobody\n\
             Host jump\n\
             \tPort 2222\n",
            &dir,
        );
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "web");
        assert_eq!(hosts[0].hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[1].alias, "db.example.com");
        assert_eq!(hosts[2].alias, "jump");
        assert_eq!(hosts[2].user, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn host_search_matches_alias_hostname_and_user() {
        let host = KnownHost {
            alias: "Web".into(),
            hostname: Some("10.0.0.5".into()),
            user: Some("Deploy".into()),
        };
        assert!(host.matches(""));
        assert!(host.matches("web"));
        assert!(host.matches("0.0.5"));
        assert!(host.matches("deploy"));
        assert!(!host.matches("db"));
    }

    #[test]
    fn follows_includes() {
        let dir = std::env::temp_dir().join("linuxscp-test-sshcfg-inc");
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(dir.join("config.d/extra"), "Host extra1\n").unwrap();
        let hosts = parse_str("Include config.d/*\nHost main\n", &dir);
        let aliases: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, vec!["extra1", "main"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
