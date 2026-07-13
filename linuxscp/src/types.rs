use serde::{Deserialize, Serialize};

pub type SessionId = u64;
pub type TransferId = u64;
pub type Reply<T> = tokio::sync::oneshot::Sender<T>;

/// A file or directory entry, unified across the local and remote backends.
#[derive(Debug, Clone, PartialEq)]
pub struct FsEntry {
    pub name: String,
    /// Full path (native for local, absolute remote path for SFTP).
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Unix timestamp of last modification.
    pub mtime: Option<i64>,
    /// Unix permission bits (the low 12 bits of st_mode).
    pub mode: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Symlink target, if known.
    pub link_target: Option<String>,
}

impl FsEntry {
    pub fn permissions_string(&self) -> String {
        match self.mode {
            Some(mode) => format_permissions(self.is_dir, self.is_symlink, mode),
            None => "----------".into(),
        }
    }
}

pub fn format_permissions(is_dir: bool, is_symlink: bool, mode: u32) -> String {
    let kind = if is_symlink {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        let exec = bits & 0o1 != 0;
        let special = match shift {
            6 => mode & 0o4000 != 0, // setuid
            3 => mode & 0o2000 != 0, // setgid
            _ => mode & 0o1000 != 0, // sticky
        };
        s.push(match (special, exec) {
            (true, true) if shift == 0 => 't',
            (true, false) if shift == 0 => 'T',
            (true, true) => 's',
            (true, false) => 'S',
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Which side of a pane talks to which filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Local,
    Remote(SessionId),
}

impl Backend {
    pub fn is_remote(&self) -> bool {
        matches!(self, Backend::Remote(_))
    }
}

/// How to become root (or another user) after connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Elevation {
    #[default]
    None,
    /// `sudo -S` fed over stdin; works with and without NOPASSWD.
    Sudo,
    /// `su -` over a forced remote pty (su requires a tty).
    Su,
}

impl Elevation {
    pub const ALL: [Elevation; 3] = [Elevation::None, Elevation::Sudo, Elevation::Su];

    pub fn label(&self) -> &'static str {
        match self {
            Elevation::None => "Connect as yourself",
            Elevation::Sudo => "Root via sudo",
            Elevation::Su => "Root via su -",
        }
    }
}

/// How to authenticate to the SSH server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Whatever ssh would do on its own: agent, ssh_config `IdentityFile`, etc.
    #[default]
    Agent,
    /// Interactive password (optionally remembered in the keyring).
    Password,
    /// A specific private key / PEM file (optionally with a saved passphrase).
    Key,
}

impl AuthMethod {
    pub const ALL: [AuthMethod; 3] = [AuthMethod::Agent, AuthMethod::Password, AuthMethod::Key];

    pub fn label(&self) -> &'static str {
        match self {
            AuthMethod::Agent => "SSH agent / default keys",
            AuthMethod::Password => "Password",
            AuthMethod::Key => "Key file (PEM)",
        }
    }
}

/// Everything needed to open a connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectSpec {
    /// ssh_config alias or hostname, handed to the ssh binary.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// Login user (`-l`); when unset ssh uses its own default / config.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub auth: AuthMethod,
    /// Private key / PEM path for [`AuthMethod::Key`].
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub elevation: Elevation,
    /// Directory to open after connecting; defaults to the remote home.
    #[serde(default)]
    pub remote_dir: Option<String>,
    /// Override for the remote sftp-server path (elevated modes only).
    #[serde(default)]
    pub sftp_server_path: Option<String>,
    /// Extra arguments passed to the ssh binary verbatim (e.g. -J jump).
    #[serde(default)]
    pub extra_ssh_args: Vec<String>,
    /// Resolved password / key passphrase to auto-answer ssh's prompt with.
    /// Never serialized; secrets live in the OS keyring.
    #[serde(skip)]
    pub secret: Option<String>,
}

impl ConnectSpec {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            user: None,
            auth: AuthMethod::Agent,
            identity_file: None,
            elevation: Elevation::None,
            remote_dir: None,
            sftp_server_path: None,
            extra_ssh_args: Vec::new(),
            secret: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectedInfo {
    pub id: SessionId,
    pub spec: ConnectSpec,
    /// Directory the remote pane should open at.
    pub initial_dir: String,
}

/// One queued transfer operation (possibly many files).
#[derive(Debug, Clone)]
pub struct TransferRequest {
    pub src_backend: Backend,
    pub dst_backend: Backend,
    pub items: Vec<FsEntry>,
    /// Destination directory the items land in.
    pub dst_dir: String,
    /// Delete sources after a fully successful copy (F6 move).
    pub move_src: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Scanning,
    Running,
    Paused,
    WaitingConflict,
    Done,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TransferState::Done | TransferState::Failed | TransferState::Cancelled
        )
    }
}

/// What a queue job is doing — copies/moves transfer bytes, while deletes
/// and attribute changes are remote mutations that also want progress,
/// cancel and a pane refresh when they complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Copy,
    Move,
    Delete,
    Attributes,
}

#[derive(Debug, Clone)]
pub struct TransferSnapshot {
    pub id: TransferId,
    pub title: String,
    pub kind: TransferKind,
    pub state: TransferState,
    pub current_file: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub files_done: u32,
    pub files_total: u32,
    pub speed_bps: f64,
    /// True while the scanner is still discovering files, so `total_bytes`
    /// and `files_total` are lower bounds (still climbing). The UI shows an
    /// indeterminate bar and withholds the ETA until this clears.
    pub scanning: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferAction {
    Pause,
    Resume,
    Cancel,
    /// Remove a terminal entry from the queue view.
    Dismiss,
}

/// User's answer to a destination-exists conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    Overwrite,
    OverwriteAll,
    /// Continue writing the existing destination from its current length.
    Resume,
    Skip,
    SkipAll,
    CancelJob,
}

#[derive(Debug)]
pub struct ConflictRequest {
    pub transfer_id: TransferId,
    pub file_name: String,
    pub src_size: u64,
    pub dst_size: u64,
    pub reply: Reply<ConflictDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Hidden-input password/passphrase entry.
    Secret,
    /// A yes/no question (host key confirmation).
    YesNo,
}

/// A prompt that must be answered by the user (from ssh askpass or elevation).
#[derive(Debug)]
pub struct PromptRequest {
    pub prompt: String,
    pub kind: PromptKind,
    /// `None` = cancelled.
    pub reply: Reply<Option<String>>,
}

/// Events flowing from the tokio side to the GTK main loop.
#[derive(Debug)]
pub enum Event {
    Prompt(PromptRequest),
    TransferUpdate(TransferSnapshot),
    Conflict(ConflictRequest),
    SessionClosed { id: SessionId, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_strings() {
        assert_eq!(format_permissions(false, false, 0o644), "-rw-r--r--");
        assert_eq!(format_permissions(true, false, 0o755), "drwxr-xr-x");
        assert_eq!(format_permissions(false, true, 0o777), "lrwxrwxrwx");
        assert_eq!(format_permissions(false, false, 0o4755), "-rwsr-xr-x");
        assert_eq!(format_permissions(false, false, 0o2644), "-rw-r-Sr--");
        assert_eq!(format_permissions(true, false, 0o1777), "drwxrwxrwt");
    }
}
