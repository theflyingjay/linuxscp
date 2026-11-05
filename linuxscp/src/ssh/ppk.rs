//! PuTTY private key (`.ppk`) files — what PuTTY, WinSCP and FileZilla on
//! Windows keep keys in. OpenSSH cannot read them, so LinuxSCP decodes the
//! container here and hands the key material to `ssh-key`, which writes it
//! back out as an OpenSSH private key (see [`super::key_import`]). The
//! format is documented in Appendix C of the PuTTY manual; versions 2
//! (PuTTY 0.52–0.74) and 3 (0.75 and later) are both common and both
//! handled. Nothing here touches the network or the user's `~/.ssh`.
//!
//! A PPK carries the public key in the clear, so [`inspect`] can show the
//! key type, comment and fingerprint before asking for a passphrase. The
//! private half is AES-256-CBC encrypted under a key derived from the
//! passphrase (two SHA-1 hashes in v2, Argon2 in v3) and the whole file is
//! covered by an HMAC, which is also how a wrong passphrase is detected.

use std::fmt;
use std::path::Path;

use base64ct::{Base64, Encoding as _};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use ssh_key::private::{Ed25519Keypair, KeypairData, RsaKeypair, RsaPrivateKey};
use ssh_key::public::KeyData;
use ssh_key::{EcdsaCurve, HashAlg, Mpint, PrivateKey, PublicKey};
use zeroize::{Zeroize, Zeroizing};

/// Every PPK file starts with this, followed by the format version.
pub const MAGIC: &[u8] = b"PuTTY-User-Key-File-";

/// Larger than any real key file by orders of magnitude.
pub const MAX_FILE_LEN: usize = 1 << 20;
/// Cap on `Public-Lines` / `Private-Lines`; a 16 kbit RSA key needs ~40.
const MAX_LINES: usize = 1024;
/// Argon2 memory in KiB (1 GiB). PuTTY's default is 8192; an unreasonable
/// value in a hostile file must not make the app allocate the moon.
const MAX_ARGON2_MEMORY_KIB: u32 = 1 << 20;
const MAX_ARGON2_PASSES: u32 = 1 << 16;
const MAX_ARGON2_PARALLELISM: u32 = 64;

const V2_MAC_KEY_PREFIX: &[u8] = b"putty-private-key-file-mac-key";

#[derive(Debug, thiserror::Error)]
pub enum PpkError {
    #[error("not a PuTTY private key file")]
    NotPpk,
    #[error("PuTTY key file format version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("{}", describe_unsupported(.0))]
    UnsupportedAlgorithm(String),
    #[error("unknown encryption type \u{201c}{0}\u{201d}")]
    UnsupportedEncryption(String),
    #[error("the key is encrypted; its passphrase is required")]
    PassphraseRequired,
    #[error("wrong passphrase (or the key file is damaged)")]
    WrongPassphrase,
    #[error("the key file is damaged: {0}")]
    Corrupt(&'static str),
    #[error("the private key in the file does not match its public key")]
    Inconsistent,
    #[error("the key file cannot be processed: {0}")]
    Limits(&'static str),
    /// ssh-key rejected the key material (malformed mpint, bad curve point).
    /// Its messages never include key bytes.
    #[error("the key data is invalid: {0}")]
    Key(ssh_key::Error),
}

fn describe_unsupported(algorithm: &str) -> String {
    match algorithm {
        "ssh-dss" => "this is a DSA key; OpenSSH dropped DSA support, so it cannot be used \
                      from Linux — generate a new key instead"
            .into(),
        "ssh-ed448" => "Ed448 keys are not supported by OpenSSH".into(),
        other if other.ends_with("-cert-v01@openssh.com") => {
            "certificate keys cannot be imported; convert the plain key instead".into()
        }
        other => format!("\u{201c}{other}\u{201d} keys cannot be converted"),
    }
}

/// How the private blob is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encryption {
    None,
    Aes256Cbc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Argon2Flavour {
    D,
    I,
    Id,
}

#[derive(Debug, Clone)]
struct Argon2Params {
    flavour: Argon2Flavour,
    memory_kib: u32,
    passes: u32,
    parallelism: u32,
    salt: Vec<u8>,
}

/// The parsed outer layer: headers and the (still encrypted) blobs.
struct Parsed<'a> {
    version: u32,
    algorithm: &'a [u8],
    encryption: Encryption,
    encryption_raw: &'a [u8],
    comment: &'a [u8],
    public_blob: Vec<u8>,
    argon2: Option<Argon2Params>,
    private_blob: Vec<u8>,
    mac: Vec<u8>,
}

/// What a PPK reveals without its passphrase.
#[derive(Debug, Clone)]
pub struct PpkInfo {
    pub version: u32,
    /// SSH algorithm name from the header, e.g. `ssh-ed25519`.
    pub algorithm: String,
    pub encrypted: bool,
    pub comment: String,
    /// The public half, with the PPK comment attached.
    pub public_key: PublicKey,
}

impl PpkInfo {
    /// Human label: "Ed25519", "RSA 3072", "ECDSA P-256".
    pub fn key_type(&self) -> String {
        match self.public_key.key_data() {
            KeyData::Rsa(rsa) => {
                let bytes = rsa.n.as_positive_bytes().unwrap_or(rsa.n.as_bytes());
                let bits =
                    bytes.len() * 8 - bytes.first().map_or(0, |b| b.leading_zeros() as usize);
                format!("RSA {bits}")
            }
            KeyData::Ecdsa(ec) => format!(
                "ECDSA {}",
                match ec.curve() {
                    EcdsaCurve::NistP256 => "P-256",
                    EcdsaCurve::NistP384 => "P-384",
                    EcdsaCurve::NistP521 => "P-521",
                }
            ),
            KeyData::Ed25519(_) => "Ed25519".into(),
            other => other.algorithm().to_string(),
        }
    }

    /// `SHA256:…` fingerprint, as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> String {
        self.public_key.fingerprint(HashAlg::Sha256).to_string()
    }
}

impl fmt::Display for PpkInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.key_type(), self.fingerprint())?;
        if !self.comment.is_empty() {
            write!(f, " ({})", self.comment)?;
        }
        Ok(())
    }
}

/// Does this look like the start of a PPK file?
pub fn is_ppk(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// Sniff a file on disk; any error (missing, unreadable) counts as "no".
pub fn is_ppk_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 32];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    is_ppk(&head[..filled])
}

/// Read the headers and the public key. Needs no passphrase.
pub fn inspect(bytes: &[u8]) -> Result<PpkInfo, PpkError> {
    check_public(&parse(bytes)?)
}

/// Decrypt and verify the file, returning the key in OpenSSH's model, ready
/// to be encoded. `passphrase` is ignored for unencrypted files.
pub fn decrypt(bytes: &[u8], passphrase: &[u8]) -> Result<PrivateKey, PpkError> {
    let parsed = parse(bytes)?;
    let info = check_public(&parsed)?;

    let encrypted = parsed.encryption != Encryption::None;
    if encrypted && passphrase.is_empty() {
        return Err(PpkError::PassphraseRequired);
    }

    // Key material: cipher key, IV and MAC key, derived per format version.
    let mut private = Zeroizing::new(parsed.private_blob.clone());
    let mac_key: Zeroizing<Vec<u8>> = match parsed.version {
        2 => {
            // Unencrypted v2 files still carry a MAC, keyed on the empty
            // passphrase.
            let pass = if encrypted { passphrase } else { b"" };
            if encrypted {
                let key = v2_cipher_key(pass);
                aes256_cbc_decrypt(&key, &[0u8; 16], &mut private)?;
            }
            let mut hasher = Sha1::new();
            hasher.update(V2_MAC_KEY_PREFIX);
            hasher.update(pass);
            Zeroizing::new(hasher.finalize().to_vec())
        }
        3 => {
            if encrypted {
                let params = parsed
                    .argon2
                    .as_ref()
                    .ok_or(PpkError::Corrupt("missing key derivation headers"))?;
                let material = argon2_derive(params, passphrase)?;
                let key: &[u8; 32] = material[..32].try_into().expect("32 bytes");
                let iv: &[u8; 16] = material[32..48].try_into().expect("16 bytes");
                aes256_cbc_decrypt(key, iv, &mut private)?;
                Zeroizing::new(material[48..80].to_vec())
            } else {
                Zeroizing::new(Vec::new())
            }
        }
        other => return Err(PpkError::UnsupportedVersion(other)),
    };

    // The MAC covers the headers, the public blob and the *decrypted*
    // private blob (padding included), so it doubles as the passphrase
    // check.
    let mut preimage = Zeroizing::new(Vec::with_capacity(
        parsed.public_blob.len() + private.len() + 128,
    ));
    put_string(&mut preimage, parsed.algorithm);
    put_string(&mut preimage, parsed.encryption_raw);
    put_string(&mut preimage, parsed.comment);
    put_string(&mut preimage, &parsed.public_blob);
    put_string(&mut preimage, &private);
    let mac_ok = match parsed.version {
        2 => verify_mac::<Hmac<Sha1>>(&mac_key, &preimage, &parsed.mac),
        _ => verify_mac::<Hmac<Sha256>>(&mac_key, &preimage, &parsed.mac),
    };
    if !mac_ok {
        return Err(if encrypted {
            PpkError::WrongPassphrase
        } else {
            PpkError::Corrupt("MAC mismatch")
        });
    }

    let keypair = keypair_from_blob(info.public_key.key_data(), &private)?;
    PrivateKey::new(keypair, info.comment).map_err(PpkError::Key)
}

// --- outer layer --------------------------------------------------------

/// Byte-oriented line reader. PuTTY writes LF but accepts CR LF and CR.
struct Lines<'a> {
    rest: &'a [u8],
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let end = self
            .rest
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .unwrap_or(self.rest.len());
        let line = &self.rest[..end];
        let mut after = end;
        if after < self.rest.len() {
            after += 1;
            if self.rest[end] == b'\r' && self.rest.get(after) == Some(&b'\n') {
                after += 1;
            }
        }
        self.rest = &self.rest[after..];
        Some(line)
    }

    /// Next line as `Name: value`; the header name must match exactly.
    fn header(&mut self, name: &str) -> Result<&'a [u8], PpkError> {
        let line = self
            .next_line()
            .ok_or(PpkError::Corrupt("file ends early"))?;
        let (found, value) =
            split_header(line).ok_or(PpkError::Corrupt("malformed header line"))?;
        if found != name.as_bytes() {
            return Err(PpkError::Corrupt("headers out of order"));
        }
        Ok(value)
    }

    fn uint_header(&mut self, name: &str) -> Result<u32, PpkError> {
        parse_uint(self.header(name)?).ok_or(PpkError::Corrupt("bad number in header"))
    }

    /// `count` lines of base64, decoded as one blob.
    fn blob(&mut self, count: usize) -> Result<Vec<u8>, PpkError> {
        if count > MAX_LINES {
            return Err(PpkError::Limits("too many key lines"));
        }
        let mut text = String::new();
        for _ in 0..count {
            let line = self
                .next_line()
                .ok_or(PpkError::Corrupt("file ends early"))?;
            let line = std::str::from_utf8(line).map_err(|_| PpkError::Corrupt("bad base64"))?;
            text.push_str(line.trim_end());
        }
        Base64::decode_vec(&text).map_err(|_| PpkError::Corrupt("bad base64"))
    }
}

/// PuTTY's own reader requires exactly ": " between name and value.
fn split_header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let (name, rest) = line.split_at(colon);
    let value = rest.strip_prefix(b": ")?;
    Some((name, value))
}

fn parse_uint(text: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(text).ok()?.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn hex_decode(text: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(text).ok()?.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, PpkError> {
    if !is_ppk(bytes) {
        return Err(PpkError::NotPpk);
    }
    if bytes.len() > MAX_FILE_LEN {
        return Err(PpkError::Limits("file too large"));
    }
    let mut lines = Lines { rest: bytes };

    let first = lines.next_line().ok_or(PpkError::NotPpk)?;
    let (name, algorithm) = split_header(first).ok_or(PpkError::NotPpk)?;
    let version = name
        .strip_prefix(MAGIC)
        .and_then(parse_uint)
        .ok_or(PpkError::NotPpk)?;
    if version != 2 && version != 3 {
        return Err(PpkError::UnsupportedVersion(version));
    }
    if algorithm.is_empty() || !algorithm.is_ascii() {
        return Err(PpkError::Corrupt("bad algorithm name"));
    }

    let encryption_raw = lines.header("Encryption")?;
    let encryption = match encryption_raw {
        b"none" => Encryption::None,
        b"aes256-cbc" => Encryption::Aes256Cbc,
        other => {
            return Err(PpkError::UnsupportedEncryption(
                String::from_utf8_lossy(other).into_owned(),
            ));
        }
    };
    let comment = lines.header("Comment")?;
    let public_lines = lines.uint_header("Public-Lines")? as usize;
    let public_blob = lines.blob(public_lines)?;

    let argon2 = if version == 3 && encryption != Encryption::None {
        let flavour = match lines.header("Key-Derivation")? {
            b"Argon2d" => Argon2Flavour::D,
            b"Argon2i" => Argon2Flavour::I,
            b"Argon2id" => Argon2Flavour::Id,
            _ => return Err(PpkError::Corrupt("unknown key derivation")),
        };
        let memory_kib = lines.uint_header("Argon2-Memory")?;
        let passes = lines.uint_header("Argon2-Passes")?;
        let parallelism = lines.uint_header("Argon2-Parallelism")?;
        let salt =
            hex_decode(lines.header("Argon2-Salt")?).ok_or(PpkError::Corrupt("bad Argon2 salt"))?;
        if memory_kib > MAX_ARGON2_MEMORY_KIB {
            return Err(PpkError::Limits(
                "Argon2 memory parameter is unreasonably large",
            ));
        }
        if passes == 0 || passes > MAX_ARGON2_PASSES {
            return Err(PpkError::Limits("Argon2 passes parameter is out of range"));
        }
        if parallelism == 0 || parallelism > MAX_ARGON2_PARALLELISM {
            return Err(PpkError::Limits(
                "Argon2 parallelism parameter is out of range",
            ));
        }
        Some(Argon2Params {
            flavour,
            memory_kib,
            passes,
            parallelism,
            salt,
        })
    } else {
        None
    };

    let private_lines = lines.uint_header("Private-Lines")? as usize;
    let private_blob = lines.blob(private_lines)?;
    if encryption != Encryption::None && (private_blob.is_empty() || private_blob.len() % 16 != 0) {
        return Err(PpkError::Corrupt(
            "encrypted data is not a whole number of blocks",
        ));
    }

    let mac = hex_decode(lines.header("Private-MAC")?).ok_or(PpkError::Corrupt("bad MAC"))?;
    let expected_mac_len = if version == 2 { 20 } else { 32 };
    if mac.len() != expected_mac_len {
        return Err(PpkError::Corrupt("bad MAC length"));
    }

    Ok(Parsed {
        version,
        algorithm,
        encryption,
        encryption_raw,
        comment,
        public_blob,
        argon2,
        private_blob,
        mac,
    })
}

/// Reject algorithms OpenSSH can't use and make sure the public blob agrees
/// with the header.
fn check_public(parsed: &Parsed<'_>) -> Result<PpkInfo, PpkError> {
    let algorithm = String::from_utf8_lossy(parsed.algorithm).into_owned();
    match algorithm.as_str() {
        "ssh-rsa"
        | "ssh-ed25519"
        | "ecdsa-sha2-nistp256"
        | "ecdsa-sha2-nistp384"
        | "ecdsa-sha2-nistp521" => {}
        _ => return Err(PpkError::UnsupportedAlgorithm(algorithm)),
    }
    let mut public_key = PublicKey::from_bytes(&parsed.public_blob).map_err(PpkError::Key)?;
    if public_key.algorithm().as_str() != algorithm {
        return Err(PpkError::Corrupt("public key does not match the header"));
    }
    let comment = String::from_utf8_lossy(parsed.comment).into_owned();
    public_key.set_comment(comment.clone());
    Ok(PpkInfo {
        version: parsed.version,
        algorithm,
        encrypted: parsed.encryption != Encryption::None,
        comment,
        public_key,
    })
}

// --- key derivation and crypto ----------------------------------------------

/// v2: AES key = SHA1(0 ‖ pass) ‖ SHA1(1 ‖ pass), truncated to 32 bytes.
fn v2_cipher_key(passphrase: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    for (i, chunk) in key.chunks_mut(20).enumerate() {
        let mut hasher = Sha1::new();
        hasher.update((i as u32).to_be_bytes());
        hasher.update(passphrase);
        let digest = hasher.finalize();
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
    key
}

/// v3: Argon2 with the file's parameters yields AES key ‖ IV ‖ MAC key.
fn argon2_derive(
    params: &Argon2Params,
    passphrase: &[u8],
) -> Result<Zeroizing<[u8; 80]>, PpkError> {
    use argon2::{Algorithm, Argon2, Block, Params, Version};

    let algorithm = match params.flavour {
        Argon2Flavour::D => Algorithm::Argon2d,
        Argon2Flavour::I => Algorithm::Argon2i,
        Argon2Flavour::Id => Algorithm::Argon2id,
    };
    let argon_params = Params::new(
        params.memory_kib,
        params.passes,
        params.parallelism,
        Some(80),
    )
    .map_err(|_| PpkError::Corrupt("invalid Argon2 parameters"))?;
    let block_count = argon_params.block_count();
    // Allocate explicitly so an oversized (but in-range) request fails
    // gracefully instead of aborting the process.
    let mut blocks: Vec<Block> = Vec::new();
    blocks
        .try_reserve_exact(block_count)
        .map_err(|_| PpkError::Limits("not enough memory for the key's Argon2 parameters"))?;
    blocks.resize_with(block_count, Block::default);

    let mut out = Zeroizing::new([0u8; 80]);
    let result = Argon2::new(algorithm, Version::V0x13, argon_params)
        .hash_password_into_with_memory(
            passphrase,
            &params.salt,
            &mut out[..],
            blocks.as_mut_slice(),
        );
    blocks.zeroize();
    result.map_err(|_| PpkError::Corrupt("Argon2 key derivation failed"))?;
    Ok(out)
}

fn aes256_cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) -> Result<(), PpkError> {
    use cbc::cipher::block_padding::NoPadding;
    use cbc::cipher::{BlockDecryptMut, KeyIvInit};

    let decryptor = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
        .map_err(|_| PpkError::Corrupt("cipher setup"))?;
    decryptor
        .decrypt_padded_mut::<NoPadding>(data)
        .map_err(|_| PpkError::Corrupt("encrypted data is not a whole number of blocks"))?;
    Ok(())
}

fn verify_mac<M: Mac + KeyInit>(key: &[u8], data: &[u8], expected: &[u8]) -> bool {
    let Ok(mut mac) = <M as KeyInit>::new_from_slice(key) else {
        return false;
    };
    mac.update(data);
    mac.verify_slice(expected).is_ok()
}

// --- SSH wire helpers -------------------------------------------------------

/// Append an SSH `string` (uint32 length prefix + bytes).
fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Reads SSH `string`s from the decrypted private blob. Whatever follows
/// the fields a key type needs is PuTTY's random padding and is ignored.
struct WireReader<'a>(&'a [u8]);

impl<'a> WireReader<'a> {
    fn string(&mut self) -> Result<&'a [u8], PpkError> {
        const TRUNCATED: PpkError = PpkError::Corrupt("private key data is truncated");
        let (len, rest) = self.0.split_at_checked(4).ok_or(TRUNCATED)?;
        let len = u32::from_be_bytes(len.try_into().expect("4 bytes")) as usize;
        let (value, rest) = rest.split_at_checked(len).ok_or(TRUNCATED)?;
        self.0 = rest;
        Ok(value)
    }

    fn mpint(&mut self) -> Result<Mpint, PpkError> {
        Mpint::from_positive_bytes(self.string()?).map_err(PpkError::Key)
    }
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let skip = bytes.iter().take_while(|&&b| b == 0).count();
    &bytes[skip..]
}

/// Combine the PPK's public key with the fields of its private blob into
/// ssh-key's keypair model. Field layouts per Appendix C.3.
fn keypair_from_blob(public: &KeyData, blob: &[u8]) -> Result<KeypairData, PpkError> {
    let mut reader = WireReader(blob);
    match public {
        // PPK order is d, p, q, iqmp; OpenSSH's is d, iqmp, p, q. Both
        // mean the same iqmp (q^-1 mod p).
        KeyData::Rsa(rsa) => {
            let d = reader.mpint()?;
            let p = reader.mpint()?;
            let q = reader.mpint()?;
            let iqmp = reader.mpint()?;
            Ok(KeypairData::Rsa(RsaKeypair {
                public: rsa.clone(),
                private: RsaPrivateKey { d, iqmp, p, q },
            }))
        }
        // One mpint: the private scalar. ssh-key wants it as a fixed-size
        // field element, so normalise the length and let it decode the
        // (curve, point, scalar) triple it expects.
        KeyData::Ecdsa(ec) => {
            let scalar = strip_leading_zeros(reader.string()?);
            let size = match ec.curve() {
                EcdsaCurve::NistP256 => 32,
                EcdsaCurve::NistP384 => 48,
                EcdsaCurve::NistP521 => 66,
            };
            if scalar.is_empty() || scalar.len() > size {
                return Err(PpkError::Corrupt("ECDSA private scalar has the wrong size"));
            }
            let mut padded = Zeroizing::new(vec![0u8; size]);
            padded[size - scalar.len()..].copy_from_slice(scalar);
            let mut encoded = Zeroizing::new(Vec::with_capacity(size + 160));
            put_string(&mut encoded, ec.curve().as_str().as_bytes());
            put_string(&mut encoded, ec.as_sec1_bytes());
            put_string(&mut encoded, &padded);
            KeypairData::decode_as(&mut encoded.as_slice(), ec.algorithm()).map_err(PpkError::Key)
        }
        // The 32-byte seed, stored little-endian as an unsigned integer —
        // which is simply the seed bytes in order. Old PuTTY versions
        // dropped high zero bytes, so it may come up short; pad on the
        // right. Deriving the public key from it proves the halves agree.
        KeyData::Ed25519(expected) => {
            let seed_bytes = reader.string()?;
            if seed_bytes.len() > 32 {
                return Err(PpkError::Corrupt("Ed25519 private key has the wrong size"));
            }
            let mut seed = Zeroizing::new([0u8; 32]);
            seed[..seed_bytes.len()].copy_from_slice(seed_bytes);
            let keypair = Ed25519Keypair::from_seed(&seed);
            if keypair.public != *expected {
                return Err(PpkError::Inconsistent);
            }
            Ok(KeypairData::Ed25519(keypair))
        }
        other => Err(PpkError::UnsupportedAlgorithm(
            other.algorithm().to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ssh_key::private::Ed25519Keypair;
    use ssh_key::public::KeyData;

    use super::*;

    /// Matches generate.sh.
    const PASSPHRASE: &[u8] = b"linuxscp-test-passphrase";

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ssh/testdata/ppk")
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn fixture_text(name: &str) -> String {
        String::from_utf8(fixture(name)).expect("fixtures are ASCII")
    }

    /// The ssh-keygen original each PPK was made from.
    fn reference(kind: &str) -> PrivateKey {
        PrivateKey::from_openssh(fixture(&format!("{kind}.openssh"))).expect("reference key")
    }

    /// An unencrypted v3 file assembled from parts, MACed the way PuTTY
    /// does (HMAC-SHA256 with an empty key), for cases puttygen won't
    /// produce.
    fn build_v3_plain(algorithm: &str, comment: &str, public: &[u8], private: &[u8]) -> Vec<u8> {
        let mut preimage = Vec::new();
        put_string(&mut preimage, algorithm.as_bytes());
        put_string(&mut preimage, b"none");
        put_string(&mut preimage, comment.as_bytes());
        put_string(&mut preimage, public);
        put_string(&mut preimage, private);
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&[]).unwrap();
        mac.update(&preimage);
        let tag: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        fn lines(blob: &[u8]) -> (usize, String) {
            let text = Base64::encode_string(blob);
            let chunks: Vec<&str> = text
                .as_bytes()
                .chunks(64)
                .map(|c| std::str::from_utf8(c).unwrap())
                .collect();
            (chunks.len(), chunks.join("\n") + "\n")
        }
        let (public_count, public_lines) = lines(public);
        let (private_count, private_lines) = lines(private);
        format!(
            "PuTTY-User-Key-File-3: {algorithm}\nEncryption: none\nComment: {comment}\n\
             Public-Lines: {public_count}\n{public_lines}Private-Lines: {private_count}\n\
             {private_lines}Private-MAC: {tag}\n"
        )
        .into_bytes()
    }

    fn ed25519_public_blob(keypair: &Ed25519Keypair) -> Vec<u8> {
        PublicKey::new(KeyData::Ed25519(keypair.public), "")
            .to_bytes()
            .unwrap()
    }

    fn string_blob(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_string(&mut out, bytes);
        out
    }

    #[test]
    fn sniffing() {
        assert!(is_ppk(b"PuTTY-User-Key-File-3: ssh-ed25519\n"));
        assert!(is_ppk(b"PuTTY-User-Key-File-2: ssh-rsa"));
        assert!(!is_ppk(b"-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(!is_ppk(b""));
        assert!(is_ppk_file(&fixture_dir().join("rsa-v3-plain.ppk")));
        assert!(!is_ppk_file(&fixture_dir().join("rsa.openssh")));
        assert!(!is_ppk_file(&fixture_dir().join("does-not-exist.ppk")));
    }

    #[test]
    fn inspect_needs_no_passphrase() {
        let info = inspect(&fixture("ed25519-v3-aes.ppk")).unwrap();
        assert_eq!(info.version, 3);
        assert!(info.encrypted);
        assert_eq!(info.algorithm, "ssh-ed25519");
        assert_eq!(info.comment, "linuxscp test ed25519");
        assert_eq!(info.key_type(), "Ed25519");
        assert_eq!(info.public_key.comment(), "linuxscp test ed25519");
        let expected = reference("ed25519")
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert_eq!(info.fingerprint(), expected);
        assert!(expected.starts_with("SHA256:"));
        assert_eq!(
            info.to_string(),
            format!("Ed25519 {expected} (linuxscp test ed25519)")
        );

        let rsa = inspect(&fixture("rsa-v2-aes.ppk")).unwrap();
        assert_eq!((rsa.version, rsa.encrypted), (2, true));
        assert_eq!(rsa.key_type(), "RSA 3072");
        assert_eq!(
            rsa.fingerprint(),
            reference("rsa")
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string()
        );

        let ecdsa = inspect(&fixture("ecdsa384-v3-plain.ppk")).unwrap();
        assert!(!ecdsa.encrypted);
        assert_eq!(ecdsa.key_type(), "ECDSA P-384");
    }

    #[test]
    fn every_fixture_decodes_to_its_reference_key() {
        let cases: &[(&str, &[&str])] = &[
            (
                "ed25519",
                &[
                    "ed25519-v3-plain.ppk",
                    "ed25519-v3-aes.ppk",
                    "ed25519-v2-aes.ppk",
                    "ed25519-v2-plain.ppk",
                    "ed25519-v3-argon2i.ppk",
                    "ed25519-v3-argon2d.ppk",
                ],
            ),
            (
                "rsa",
                &["rsa-v3-plain.ppk", "rsa-v3-aes.ppk", "rsa-v2-aes.ppk"],
            ),
            (
                "ecdsa256",
                &[
                    "ecdsa256-v3-plain.ppk",
                    "ecdsa256-v3-aes.ppk",
                    "ecdsa256-v2-aes.ppk",
                ],
            ),
            (
                "ecdsa384",
                &[
                    "ecdsa384-v3-plain.ppk",
                    "ecdsa384-v3-aes.ppk",
                    "ecdsa384-v2-aes.ppk",
                ],
            ),
            (
                "ecdsa521",
                &[
                    "ecdsa521-v3-plain.ppk",
                    "ecdsa521-v3-aes.ppk",
                    "ecdsa521-v2-aes.ppk",
                ],
            ),
        ];
        for (kind, files) in cases {
            let expected = reference(kind);
            for file in *files {
                let bytes = fixture(file);
                let key = decrypt(&bytes, PASSPHRASE).unwrap_or_else(|e| panic!("{file}: {e}"));
                assert_eq!(key.key_data(), expected.key_data(), "{file}");
                assert_eq!(key.comment(), format!("linuxscp test {kind}"), "{file}");
                assert!(!key.is_encrypted(), "{file}");
                assert_eq!(
                    key.public_key().key_data(),
                    expected.public_key().key_data()
                );
                // Unencrypted files need no passphrase; a stray one is ignored.
                if !inspect(&bytes).unwrap().encrypted {
                    assert!(decrypt(&bytes, b"").is_ok(), "{file}");
                    assert!(decrypt(&bytes, b"ignored").is_ok(), "{file}");
                }
            }
        }
    }

    #[test]
    fn wrong_or_missing_passphrase() {
        for file in ["ed25519-v3-aes.ppk", "ed25519-v2-aes.ppk", "rsa-v3-aes.ppk"] {
            let bytes = fixture(file);
            assert!(
                matches!(decrypt(&bytes, b"not it"), Err(PpkError::WrongPassphrase)),
                "{file}"
            );
            assert!(
                matches!(decrypt(&bytes, b""), Err(PpkError::PassphraseRequired)),
                "{file}"
            );
        }
    }

    #[test]
    fn tampering_with_the_comment_breaks_the_mac() {
        let plain = fixture_text("ed25519-v3-plain.ppk").replace(
            "Comment: linuxscp test ed25519",
            "Comment: linuxscp test ed25519!",
        );
        // The public half is still readable...
        assert_eq!(
            inspect(plain.as_bytes()).unwrap().comment,
            "linuxscp test ed25519!"
        );
        // ...but the key is refused.
        assert!(matches!(
            decrypt(plain.as_bytes(), b""),
            Err(PpkError::Corrupt("MAC mismatch"))
        ));
        let v2 = fixture_text("ed25519-v2-plain.ppk").replace(
            "Comment: linuxscp test ed25519",
            "Comment: linuxscp test ed25519!",
        );
        assert!(matches!(
            decrypt(v2.as_bytes(), b""),
            Err(PpkError::Corrupt("MAC mismatch"))
        ));
        // On an encrypted file the same failure reads as a bad passphrase.
        let aes = fixture_text("ed25519-v3-aes.ppk").replace(
            "Comment: linuxscp test ed25519",
            "Comment: linuxscp test ed25519!",
        );
        assert!(matches!(
            decrypt(aes.as_bytes(), PASSPHRASE),
            Err(PpkError::WrongPassphrase)
        ));
    }

    #[test]
    fn tolerates_crlf_and_cr_line_endings() {
        let expected = reference("ecdsa256");
        let lf = fixture_text("ecdsa256-v3-aes.ppk");
        assert!(lf.contains('\n') && !lf.contains('\r'));
        for text in [lf.replace('\n', "\r\n"), lf.replace('\n', "\r")] {
            let key = decrypt(text.as_bytes(), PASSPHRASE).unwrap();
            assert_eq!(key.key_data(), expected.key_data());
        }
    }

    #[test]
    fn rejects_other_versions_and_algorithms() {
        let v1 = fixture_text("ed25519-v3-plain.ppk")
            .replace("PuTTY-User-Key-File-3", "PuTTY-User-Key-File-1");
        assert!(matches!(
            inspect(v1.as_bytes()),
            Err(PpkError::UnsupportedVersion(1))
        ));
        let v4 = fixture_text("ed25519-v3-plain.ppk")
            .replace("PuTTY-User-Key-File-3", "PuTTY-User-Key-File-4");
        assert!(matches!(
            decrypt(v4.as_bytes(), b""),
            Err(PpkError::UnsupportedVersion(4))
        ));

        let dsa = fixture("dsa-v2-plain.ppk");
        match inspect(&dsa) {
            Err(PpkError::UnsupportedAlgorithm(name)) => assert_eq!(name, "ssh-dss"),
            other => panic!("{other:?}"),
        }
        let message = decrypt(&dsa, b"").unwrap_err().to_string();
        assert!(message.contains("DSA"), "{message}");

        let ed448 = fixture("ed448-v3-plain.ppk");
        match decrypt(&ed448, b"") {
            Err(PpkError::UnsupportedAlgorithm(name)) => assert_eq!(name, "ssh-ed448"),
            other => panic!("{other:?}"),
        }

        let cert = fixture_text("ed25519-v3-plain.ppk").replace(
            "PuTTY-User-Key-File-3: ssh-ed25519",
            "PuTTY-User-Key-File-3: ssh-ed25519-cert-v01@openssh.com",
        );
        assert!(matches!(
            inspect(cert.as_bytes()),
            Err(PpkError::UnsupportedAlgorithm(_))
        ));

        let cipher = fixture_text("ed25519-v3-aes.ppk")
            .replace("Encryption: aes256-cbc", "Encryption: aes128-cbc");
        assert!(matches!(
            inspect(cipher.as_bytes()),
            Err(PpkError::UnsupportedEncryption(_))
        ));
    }

    #[test]
    fn not_a_ppk() {
        assert!(matches!(
            inspect(&fixture("ed25519.openssh")),
            Err(PpkError::NotPpk)
        ));
        assert!(matches!(decrypt(b"", b""), Err(PpkError::NotPpk)));
        assert!(matches!(
            inspect(b"PuTTY-User-Key-File-x: ssh-rsa\n"),
            Err(PpkError::NotPpk)
        ));
        assert!(matches!(
            inspect(b"PuTTY-User-Key-File-3: ssh-rsa\nEncryption: none\n"),
            Err(PpkError::Corrupt(_))
        ));
        // Headers must come in PuTTY's order with ": " separators.
        let swapped =
            fixture_text("ed25519-v3-plain.ppk").replace("Encryption: none", "Encryption:none");
        assert!(matches!(
            inspect(swapped.as_bytes()),
            Err(PpkError::Corrupt(_))
        ));
    }

    #[test]
    fn hostile_argon2_parameters_are_refused_at_parse_time() {
        let text = fixture_text("ed25519-v3-aes.ppk");
        // puttygen tunes the pass count to the generating machine, so look
        // the headers up rather than assuming their values.
        let header = |name: &str| {
            text.lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("{name} header"))
                .to_string()
        };
        for (from, to) in [
            (header("Argon2-Memory: "), "Argon2-Memory: 4294967295"),
            (header("Argon2-Passes: "), "Argon2-Passes: 0"),
            (header("Argon2-Passes: "), "Argon2-Passes: 1000000"),
            (header("Argon2-Parallelism: "), "Argon2-Parallelism: 4096"),
        ] {
            let hostile = text.replace(&from, to);
            // Even inspect refuses, so the UI never gets as far as asking
            // for a passphrase.
            assert!(
                matches!(inspect(hostile.as_bytes()), Err(PpkError::Limits(_))),
                "{to}"
            );
            assert!(matches!(
                decrypt(hostile.as_bytes(), PASSPHRASE),
                Err(PpkError::Limits(_))
            ));
        }
        let lines = text.replace("Public-Lines: 2", "Public-Lines: 2000000");
        assert!(matches!(
            inspect(lines.as_bytes()),
            Err(PpkError::Limits(_))
        ));
    }

    #[test]
    fn short_ed25519_seed_is_padded_on_the_right() {
        // Old PuTTY wrote the seed as a minimal little-endian integer, so a
        // seed whose high bytes are zero came out short.
        let mut seed = [0x5au8; 32];
        seed[30] = 0;
        seed[31] = 0;
        let keypair = Ed25519Keypair::from_seed(&seed);
        let file = build_v3_plain(
            "ssh-ed25519",
            "short seed",
            &ed25519_public_blob(&keypair),
            &string_blob(&seed[..30]),
        );
        let key = decrypt(&file, b"").unwrap();
        assert_eq!(key.key_data().ed25519().unwrap().private.to_bytes(), seed);
        assert_eq!(key.comment(), "short seed");

        // Padding bytes after the fields are ignored, as in real files.
        let mut padded = string_blob(&seed);
        padded.extend_from_slice(&[7u8; 13]);
        let file = build_v3_plain(
            "ssh-ed25519",
            "padded",
            &ed25519_public_blob(&keypair),
            &padded,
        );
        assert_eq!(
            decrypt(&file, b"")
                .unwrap()
                .key_data()
                .ed25519()
                .unwrap()
                .private
                .to_bytes(),
            seed
        );
    }

    #[test]
    fn mismatched_or_malformed_private_halves_are_rejected() {
        let public = ed25519_public_blob(&Ed25519Keypair::from_seed(&[1u8; 32]));
        // Private half belongs to another key: valid MAC, inconsistent key.
        let swapped = build_v3_plain("ssh-ed25519", "", &public, &string_blob(&[2u8; 32]));
        assert!(matches!(
            decrypt(&swapped, b""),
            Err(PpkError::Inconsistent)
        ));
        // Too long for a seed.
        let long = build_v3_plain("ssh-ed25519", "", &public, &string_blob(&[1u8; 33]));
        assert!(matches!(decrypt(&long, b""), Err(PpkError::Corrupt(_))));
        // Length prefix promises more than is there.
        let truncated = build_v3_plain("ssh-ed25519", "", &public, &[0, 0, 0, 40, 1, 2, 3]);
        assert!(matches!(
            decrypt(&truncated, b""),
            Err(PpkError::Corrupt(_))
        ));
        // Public blob algorithm disagrees with the header.
        let rsa_public = reference("rsa").public_key().to_bytes().unwrap();
        let mixed = build_v3_plain("ssh-ed25519", "", &rsa_public, &string_blob(&[1u8; 32]));
        assert!(matches!(inspect(&mixed), Err(PpkError::Corrupt(_))));
    }

    #[test]
    fn mac_preimage_matches_puttys_layout() {
        // A v2 unencrypted file has a MAC keyed on the empty passphrase;
        // flipping one base64 character of the private blob must be
        // caught even though nothing is encrypted.
        let text = fixture_text("ed25519-v2-plain.ppk");
        let mac_line = text
            .lines()
            .find(|l| l.starts_with("Private-MAC: "))
            .unwrap();
        assert_eq!(mac_line.len(), "Private-MAC: ".len() + 40);
        let private_start = text.find("Private-Lines: 1\n").unwrap() + "Private-Lines: 1\n".len();
        let mut bytes = text.into_bytes();
        bytes[private_start] = if bytes[private_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(matches!(
            decrypt(&bytes, b""),
            Err(PpkError::Corrupt("MAC mismatch"))
        ));
    }
}
