//! Turning a PuTTY `.ppk` into an OpenSSH key file the system `ssh` can use.
//!
//! OpenSSH will never read PPK, so "importing" means converting once: the
//! key is decoded ([`super::ppk`]), optionally re-encrypted under the same
//! passphrase (OpenSSH's own `aes256-ctr` + `bcrypt` scheme, what
//! `ssh-keygen` produces) and written to `~/.ssh/<name>` with a matching
//! `.pub` beside it. The original `.ppk` is left untouched. Files are
//! created with `O_EXCL` and mode 0600 / 0644, nothing is ever overwritten,
//! and the written key is read back and checked before success is reported.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use ssh_key::rand_core::OsRng;
use ssh_key::{LineEnding, PrivateKey};
use zeroize::Zeroizing;

use super::ppk::{self, MAX_FILE_LEN, PpkError};
use crate::tr;

/// Fallback name when the `.ppk` file name yields nothing usable.
const DEFAULT_NAME: &str = "putty-key";

pub struct ImportRequest {
    /// The `.ppk` file.
    pub source: PathBuf,
    /// Its passphrase, if encrypted.
    pub passphrase: Option<Zeroizing<String>>,
    /// File name (not a path) for the converted key under `~/.ssh`.
    pub dest_name: String,
    /// Encrypt the converted key with the same passphrase. Ignored when
    /// there is no passphrase.
    pub protect_with_passphrase: bool,
}

#[derive(Debug, Clone)]
pub struct Imported {
    pub private_path: PathBuf,
    pub public_path: PathBuf,
    /// The written key is encrypted with the PPK's passphrase.
    pub passphrase_kept: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Ppk(#[from] PpkError),
    #[error("{}", tr!("could not determine your home directory"))]
    NoHome,
    #[error("{}", tr!("the key file name is not valid"))]
    BadName,
    #[error("{}", tr!("a key named \u{201c}{}\u{201d} already exists in {}", .0, .1))]
    NameTaken(String, String),
    #[error("{}", tr!("the file is too large to be a key file"))]
    TooLarge,
    #[error("{}", tr!("could not read {}: {}", path.display(), source))]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}", tr!("could not write {}: {}", path.display(), source))]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}", tr!("could not encode the OpenSSH key: {}", .0))]
    Encode(ssh_key::Error),
    #[error("{}", tr!("the converted key did not read back correctly"))]
    Verify,
}

/// `~/.ssh`.
pub fn ssh_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh"))
}

/// A free file name under `~/.ssh` derived from the `.ppk`'s name:
/// `cluster.ppk` becomes `cluster`, or `cluster-2` if that is taken.
pub fn suggest_name(source: &Path) -> String {
    match ssh_dir() {
        Some(dir) => suggest_name_in(&dir, source),
        None => base_name(source),
    }
}

pub fn suggest_name_in(dir: &Path, source: &Path) -> String {
    let base = base_name(source);
    if !name_taken(dir, &base) {
        return base;
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !name_taken(dir, candidate))
        .expect("some suffix is free")
}

fn base_name(source: &Path) -> String {
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = match name.char_indices().rev().nth(3) {
        Some((i, _)) if name[i..].eq_ignore_ascii_case(".ppk") => &name[..i],
        _ => name,
    };
    let stem = stem.trim();
    if validate_name(stem).is_err() {
        DEFAULT_NAME.to_string()
    } else {
        stem.to_string()
    }
}

/// Either the key or its `.pub` exists (dangling symlinks count).
fn name_taken(dir: &Path, name: &str) -> bool {
    dir.join(name).symlink_metadata().is_ok()
        || dir.join(format!("{name}.pub")).symlink_metadata().is_ok()
}

/// A plain file name: no path separators, not `.` or `..`.
pub fn validate_name(name: &str) -> Result<(), ImportError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(ImportError::BadName);
    }
    Ok(())
}

/// Convert and write under `~/.ssh`. Blocking: run it off the GTK thread.
pub fn import(request: &ImportRequest) -> Result<Imported, ImportError> {
    let dir = ssh_dir().ok_or(ImportError::NoHome)?;
    import_into(request, &dir)
}

/// [`import`] with an explicit destination directory (created 0700 if
/// missing).
pub fn import_into(request: &ImportRequest, dir: &Path) -> Result<Imported, ImportError> {
    validate_name(&request.dest_name)?;
    let bytes = read_capped(&request.source)?;
    let passphrase: &[u8] = request
        .passphrase
        .as_deref()
        .map(|p| p.as_bytes())
        .unwrap_or(&[]);

    let key = ppk::decrypt(&bytes, passphrase)?;
    // The .pub line comes from the plaintext key so it keeps the comment
    // (ssh-key's encrypted form only carries the comment inside the
    // ciphertext).
    let public_line = key.public_key().to_openssh().map_err(ImportError::Encode)?;
    let keep = request.protect_with_passphrase && !passphrase.is_empty();
    let pem = if keep {
        key.encrypt(&mut OsRng, passphrase)
            .map_err(ImportError::Encode)?
            .to_openssh(LineEnding::LF)
    } else {
        key.to_openssh(LineEnding::LF)
    }
    .map_err(ImportError::Encode)?;

    ensure_dir(dir)?;
    let private_path = dir.join(&request.dest_name);
    let public_path = dir.join(format!("{}.pub", request.dest_name));
    let dir_label = dir.display().to_string();
    let taken_err = |err: ImportError| match err {
        ImportError::Write { source, .. } if source.kind() == io::ErrorKind::AlreadyExists => {
            ImportError::NameTaken(request.dest_name.clone(), dir_label.clone())
        }
        other => other,
    };

    // Only remove what this call created: a pre-existing file that made
    // create_new fail must stay.
    let mut created: Vec<&Path> = Vec::new();
    let mut write_both = || {
        write_new(&private_path, 0o600, pem.as_bytes()).map_err(taken_err)?;
        created.push(&private_path);
        write_new(&public_path, 0o644, format!("{public_line}\n").as_bytes()).map_err(taken_err)?;
        created.push(&public_path);
        verify_written(&private_path, &key, keep.then_some(passphrase))
    };
    let outcome = write_both();
    if let Err(err) = outcome {
        for path in created {
            let _ = fs::remove_file(path);
        }
        return Err(err);
    }

    Ok(Imported {
        private_path,
        public_path,
        passphrase_kept: keep,
    })
}

fn read_capped(path: &Path) -> Result<Vec<u8>, ImportError> {
    let read_err = |source| ImportError::Read {
        path: path.to_path_buf(),
        source,
    };
    let file = fs::File::open(path).map_err(read_err)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(read_err)?;
    if bytes.len() > MAX_FILE_LEN {
        return Err(ImportError::TooLarge);
    }
    Ok(bytes)
}

fn ensure_dir(dir: &Path) -> Result<(), ImportError> {
    if dir.is_dir() {
        return Ok(());
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(ImportError::Write {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

fn write_new(path: &Path, mode: u32, data: &[u8]) -> Result<(), ImportError> {
    let write_err = |source| ImportError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(write_err)?;
    file.write_all(data).map_err(write_err)?;
    file.sync_all().map_err(write_err)
}

/// Read the file back the way ssh will and make sure it is the same key.
fn verify_written(
    path: &Path,
    original: &PrivateKey,
    passphrase: Option<&[u8]>,
) -> Result<(), ImportError> {
    let text = Zeroizing::new(fs::read(path).map_err(|source| ImportError::Read {
        path: path.to_path_buf(),
        source,
    })?);
    let parsed = PrivateKey::from_openssh(text.as_slice()).map_err(|_| ImportError::Verify)?;
    let plain = match passphrase {
        Some(passphrase) if parsed.is_encrypted() => parsed
            .decrypt(passphrase)
            .map_err(|_| ImportError::Verify)?,
        None if !parsed.is_encrypted() => parsed,
        _ => return Err(ImportError::Verify),
    };
    if plain.key_data() != original.key_data() || plain.comment() != original.comment() {
        return Err(ImportError::Verify);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PASSPHRASE: &str = "linuxscp-test-passphrase";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/ssh/testdata/ppk")
            .join(name)
    }

    fn reference(kind: &str) -> PrivateKey {
        PrivateKey::from_openssh(fs::read(fixture(&format!("{kind}.openssh"))).unwrap()).unwrap()
    }

    /// A fresh scratch tree; `dir` is the would-be `~/.ssh`, not yet created.
    struct Scratch {
        root: PathBuf,
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("linuxscp-key-import-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let dir = root.join("ssh");
            Self { root, dir }
        }

        fn entries(&self) -> Vec<String> {
            let Ok(read) = fs::read_dir(&self.dir) else {
                return Vec::new();
            };
            let mut names: Vec<String> = read
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn request(source: &str, passphrase: Option<&str>, name: &str, protect: bool) -> ImportRequest {
        ImportRequest {
            source: fixture(source),
            passphrase: passphrase.map(|p| Zeroizing::new(p.to_string())),
            dest_name: name.into(),
            protect_with_passphrase: protect,
        }
    }

    #[test]
    fn imports_an_encrypted_key_and_keeps_its_passphrase() {
        let scratch = Scratch::new("encrypted");
        let imported = import_into(
            &request("ed25519-v3-aes.ppk", Some(PASSPHRASE), "cluster", true),
            &scratch.dir,
        )
        .unwrap();

        assert_eq!(imported.private_path, scratch.dir.join("cluster"));
        assert_eq!(imported.public_path, scratch.dir.join("cluster.pub"));
        assert!(imported.passphrase_kept);
        assert_eq!(mode(&scratch.dir), 0o700);
        assert_eq!(mode(&imported.private_path), 0o600);
        assert_eq!(mode(&imported.public_path), 0o644);
        assert_eq!(scratch.entries(), ["cluster", "cluster.pub"]);

        let written = PrivateKey::from_openssh(fs::read(&imported.private_path).unwrap()).unwrap();
        assert!(written.is_encrypted());
        assert!(written.decrypt("wrong").is_err());
        let plain = written.decrypt(PASSPHRASE).unwrap();
        let expected = reference("ed25519");
        assert_eq!(plain.key_data(), expected.key_data());
        assert_eq!(plain.comment(), "linuxscp test ed25519");

        // The .pub is byte-for-byte what ssh-keygen wrote for the original.
        assert_eq!(
            fs::read_to_string(&imported.public_path).unwrap(),
            fs::read_to_string(fixture("ed25519.openssh.pub")).unwrap()
        );
    }

    #[test]
    fn can_drop_the_passphrase_on_request() {
        let scratch = Scratch::new("plain-out");
        let imported = import_into(
            &request("rsa-v2-aes.ppk", Some(PASSPHRASE), "legacy", false),
            &scratch.dir,
        )
        .unwrap();
        assert!(!imported.passphrase_kept);
        let written = PrivateKey::from_openssh(fs::read(&imported.private_path).unwrap()).unwrap();
        assert!(!written.is_encrypted());
        assert_eq!(written.key_data(), reference("rsa").key_data());
        assert_eq!(mode(&imported.private_path), 0o600);
    }

    #[test]
    fn unencrypted_ppk_needs_no_passphrase() {
        let scratch = Scratch::new("plain-in");
        // "Protect" is moot without a passphrase.
        let imported = import_into(
            &request("ecdsa521-v3-plain.ppk", None, "ec", true),
            &scratch.dir,
        )
        .unwrap();
        assert!(!imported.passphrase_kept);
        let written = PrivateKey::from_openssh(fs::read(&imported.private_path).unwrap()).unwrap();
        assert!(!written.is_encrypted());
        assert_eq!(written.key_data(), reference("ecdsa521").key_data());
        assert_eq!(
            fs::read_to_string(&imported.public_path).unwrap(),
            fs::read_to_string(fixture("ecdsa521.openssh.pub")).unwrap()
        );
    }

    #[test]
    fn never_overwrites_and_cleans_up_after_itself() {
        let scratch = Scratch::new("overwrite");
        fs::create_dir(&scratch.dir).unwrap();
        fs::write(scratch.dir.join("taken"), "keep me").unwrap();
        let err = import_into(
            &request("ed25519-v3-plain.ppk", None, "taken", true),
            &scratch.dir,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ImportError::NameTaken(name, _) if name == "taken"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(scratch.dir.join("taken")).unwrap(),
            "keep me"
        );
        assert_eq!(scratch.entries(), ["taken"]);

        // Only the .pub exists: the private key written first must be
        // removed again, the stranger's .pub left alone.
        fs::write(scratch.dir.join("half.pub"), "someone else's").unwrap();
        let err = import_into(
            &request("ed25519-v3-plain.ppk", None, "half", true),
            &scratch.dir,
        )
        .unwrap_err();
        assert!(matches!(err, ImportError::NameTaken(..)), "{err}");
        assert_eq!(scratch.entries(), ["half.pub", "taken"]);
        assert_eq!(
            fs::read_to_string(scratch.dir.join("half.pub")).unwrap(),
            "someone else's"
        );
    }

    #[test]
    fn failures_leave_nothing_behind() {
        let scratch = Scratch::new("failures");
        let err = import_into(
            &request("ed25519-v3-aes.ppk", Some("nope"), "k", true),
            &scratch.dir,
        )
        .unwrap_err();
        assert!(
            matches!(err, ImportError::Ppk(PpkError::WrongPassphrase)),
            "{err}"
        );
        assert!(matches!(
            import_into(
                &request("ed25519-v3-aes.ppk", None, "k", true),
                &scratch.dir
            ),
            Err(ImportError::Ppk(PpkError::PassphraseRequired))
        ));
        assert!(matches!(
            import_into(&request("ed25519.openssh", None, "k", true), &scratch.dir),
            Err(ImportError::Ppk(PpkError::NotPpk))
        ));
        assert!(matches!(
            import_into(&request("dsa-v2-plain.ppk", None, "k", true), &scratch.dir),
            Err(ImportError::Ppk(PpkError::UnsupportedAlgorithm(_)))
        ));
        let missing = ImportRequest {
            source: scratch.root.join("missing.ppk"),
            ..request("ed25519-v3-plain.ppk", None, "k", true)
        };
        assert!(matches!(
            import_into(&missing, &scratch.dir),
            Err(ImportError::Read { .. })
        ));
        assert!(scratch.entries().is_empty());
    }

    #[test]
    fn rejects_names_that_are_not_plain_file_names() {
        let scratch = Scratch::new("names");
        for bad in ["", ".", "..", "a/b", "/etc/x", "nul\0"] {
            let err = import_into(
                &request("ed25519-v3-plain.ppk", None, bad, true),
                &scratch.dir,
            )
            .unwrap_err();
            assert!(matches!(err, ImportError::BadName), "{bad:?}: {err}");
        }
        assert!(!scratch.dir.exists());
        assert!(validate_name("id_ed25519").is_ok());
        assert!(validate_name("My Key").is_ok());
    }

    #[test]
    fn suggested_names_come_from_the_ppk_and_stay_unique() {
        let scratch = Scratch::new("suggest");
        let dir = &scratch.dir;
        let source = Path::new("/mnt/usb/cluster.ppk");
        assert_eq!(suggest_name_in(dir, source), "cluster");
        assert_eq!(suggest_name_in(dir, Path::new("My Key.PPK")), "My Key");
        assert_eq!(suggest_name_in(dir, Path::new("noext")), "noext");
        assert_eq!(suggest_name_in(dir, Path::new(".ppk")), DEFAULT_NAME);
        assert_eq!(suggest_name_in(dir, Path::new("")), DEFAULT_NAME);

        fs::create_dir(dir).unwrap();
        fs::write(dir.join("cluster"), "").unwrap();
        assert_eq!(suggest_name_in(dir, source), "cluster-2");
        fs::write(dir.join("cluster-2.pub"), "").unwrap();
        assert_eq!(suggest_name_in(dir, source), "cluster-3");
        // A dangling symlink still counts as taken.
        std::os::unix::fs::symlink("/nowhere", dir.join("cluster-3")).unwrap();
        assert_eq!(suggest_name_in(dir, source), "cluster-4");
    }
}
