//! Ed25519 head signing (LOG-FORMAT.md §4).
//!
//! The signing key is loaded from an env var or file path at runtime — never
//! from a constant, never committed. Secrets are zeroized on drop.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use ed25519_dalek::{SigningKey as DalekSigningKey, SECRET_KEY_LENGTH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::canonical::canonical_json;
use crate::entry::{HashRef, Timestamp};

/// Unsigned log head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub seq: u64,
    pub entry_hash: HashRef,
    pub recorded_at: Timestamp,
    pub log_id: String,
}

/// Published signed head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedHead {
    pub head: Head,
    /// Ed25519 signature over `canonical_json(head)`, lowercase hex (128 chars).
    pub signature: String,
    /// Ed25519 public key, lowercase hex (64 chars).
    pub public_key: String,
}

#[derive(Debug, Error)]
pub enum SignError {
    #[error("invalid signing key: {0}")]
    InvalidKey(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("environment variable {0} is not set")]
    MissingEnv(String),
    #[error("canonicalisation failed: {0}")]
    Canonical(String),
    #[error("serde error: {0}")]
    Serde(String),
    #[error("invalid signature encoding")]
    InvalidSignature,
    #[error("invalid public key encoding")]
    InvalidPublicKey,
    #[error("signature verification failed")]
    VerificationFailed,
    #[error("refusing to overwrite existing key file: {0}")]
    AlreadyExists(String),
    #[error("failed to draw OS entropy: {0}")]
    Entropy(String),
}

/// Where to load a signing secret from. Never a compiled-in constant.
#[derive(Debug, Clone)]
pub enum KeySource {
    /// 64-char lowercase hex seed (32 bytes).
    HexSeed(String),
    /// File whose entire contents are a 64-char hex seed (optional trailing newline).
    File(PathBuf),
    /// Environment variable holding a 64-char hex seed.
    Env(String),
}

/// Ed25519 signing key. Secret material is zeroized on drop and omitted from Debug.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SigningKey {
    secret: [u8; SECRET_KEY_LENGTH],
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey { secret: REDACTED }")
    }
}

impl SigningKey {
    pub fn from_seed_bytes(seed: &[u8; SECRET_KEY_LENGTH]) -> Result<Self, SignError> {
        // Construct via dalek to validate; store seed for zeroization on drop.
        let _ = DalekSigningKey::from_bytes(seed);
        Ok(Self { secret: *seed })
    }

    fn dalek(&self) -> DalekSigningKey {
        DalekSigningKey::from_bytes(&self.secret)
    }

    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.dalek().verifying_key().to_bytes()
    }
}

/// Load a signing key from an env var or file path given at runtime.
pub fn load_signing_key(source: KeySource) -> Result<SigningKey, SignError> {
    let hex_seed = match source {
        KeySource::HexSeed(s) => s,
        KeySource::Env(name) => std::env::var(&name).map_err(|_| SignError::MissingEnv(name))?,
        KeySource::File(path) => {
            fs::read_to_string(&path).map_err(|e| SignError::Io(e.to_string()))?
        }
    };
    parse_hex_seed(&hex_seed)
}

fn parse_hex_seed(raw: &str) -> Result<SigningKey, SignError> {
    let s = raw.trim();
    if s.len() != SECRET_KEY_LENGTH * 2 {
        return Err(SignError::InvalidKey(format!(
            "expected {} hex chars, got {}",
            SECRET_KEY_LENGTH * 2,
            s.len()
        )));
    }
    if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(SignError::InvalidKey(
            "seed must be lowercase hexadecimal".into(),
        ));
    }
    let mut seed = [0u8; SECRET_KEY_LENGTH];
    hex::decode_to_slice(s, &mut seed).map_err(|e| SignError::InvalidKey(e.to_string()))?;
    SigningKey::from_seed_bytes(&seed)
}

/// `sha256:` + hex(SHA-256 of the raw 32-byte Ed25519 public key).
///
/// This is not a hash of the hex encoding or of the PEM form.
pub fn key_id(public_key: &[u8; 32]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(public_key)))
}

/// Public half printed by keygen. Never contains secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedPublicKey {
    /// 64 lowercase hex characters (raw 32-byte Ed25519 public key).
    pub public_key_hex: String,
    /// `sha256:` + SHA-256 of the raw public key bytes.
    pub key_id: String,
}

/// Generate an Ed25519 seed from the OS random source, write it to `path`
/// (64 lowercase hex chars + newline, mode `0600`), and return only the public
/// half. Refuses to overwrite an existing file.
pub fn generate_signing_key_file(path: &Path) -> Result<GeneratedPublicKey, SignError> {
    if path.exists() {
        return Err(SignError::AlreadyExists(path.display().to_string()));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| SignError::Io(e.to_string()))?;
        }
    }

    let mut seed = [0u8; SECRET_KEY_LENGTH];
    getrandom::fill(&mut seed).map_err(|e| SignError::Entropy(e.to_string()))?;
    let key = SigningKey::from_seed_bytes(&seed)?;
    let public = key.verifying_key_bytes();
    let public_key_hex = hex::encode(public);
    let kid = key_id(&public);

    let mut hex_seed = hex::encode(seed);
    seed.zeroize();

    write_seed_file_0600(path, &hex_seed)?;
    hex_seed.zeroize();

    Ok(GeneratedPublicKey {
        public_key_hex,
        key_id: kid,
    })
}

fn write_seed_file_0600(path: &Path, hex_seed: &str) -> Result<(), SignError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    SignError::AlreadyExists(path.display().to_string())
                } else {
                    SignError::Io(e.to_string())
                }
            })?;
        writeln!(file, "{hex_seed}").map_err(|e| SignError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| SignError::Io(e.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = hex_seed;
        return Err(SignError::Io(
            "signing key generation requires a Unix filesystem for mode 0600".into(),
        ));
    }
    Ok(())
}

/// PKIX `SubjectPublicKeyInfo` PEM for an Ed25519 public key.
///
/// Rekor's hashedrekord verifier parses this with `x509.ParsePKIXPublicKey`
/// (`BEGIN PUBLIC KEY`), not a raw 32-byte blob.
pub fn public_key_pkix_pem(public_key: &[u8; 32]) -> String {
    // SEQUENCE { SEQUENCE { OID 1.3.101.112 } BIT STRING { 0x00 || key } }
    let mut body = Vec::with_capacity(44);
    body.extend_from_slice(&[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]);
    body.extend_from_slice(public_key);
    let mut der = Vec::with_capacity(46);
    der.extend_from_slice(&[0x30, body.len() as u8]);
    der.extend_from_slice(&body);
    let b64 = base64_encode(&der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem
}

/// Ed25519ph (RFC 8032, empty context) over `message`.
///
/// Rekor hashedrekord v0.0.1 loads Ed25519 signatures with `WithED25519ph`
/// and verifies `ed25519.VerifyWithOptions(..., Hash: SHA-512)` against the
/// prehash it was given. Pure Ed25519 over the log head is a different
/// signature and is what [`sign_head`] produces for `heads.jsonl`.
pub fn sign_ed25519ph(key: &SigningKey, message: &[u8]) -> Result<[u8; 64], SignError> {
    let mut prehash = Sha512::new();
    prehash.update(message);
    let sig = key
        .dalek()
        .sign_prehashed(prehash, None)
        .map_err(|e| SignError::InvalidKey(e.to_string()))?;
    Ok(sig.to_bytes())
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    let rest = bytes.len() - i;
    if rest == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

/// Sign `head` with Ed25519 over `canonical_json(head)`.
pub fn sign_head(head: &Head, key: &SigningKey) -> Result<SignedHead, SignError> {
    let message = head_message(head)?;
    let dalek = key.dalek();
    let signature = dalek.sign(&message);
    Ok(SignedHead {
        head: head.clone(),
        signature: hex::encode(signature.to_bytes()),
        public_key: hex::encode(dalek.verifying_key().to_bytes()),
    })
}

/// Verify `signed` against the public key it carries.
pub fn verify_head(signed: &SignedHead) -> Result<(), SignError> {
    let message = head_message(&signed.head)?;
    let sig_bytes = decode_hex_exact(&signed.signature, Signature::BYTE_SIZE)
        .map_err(|_| SignError::InvalidSignature)?;
    let pk_bytes =
        decode_hex_exact(&signed.public_key, 32).map_err(|_| SignError::InvalidPublicKey)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| SignError::InvalidSignature)?;
    let pk_array: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| SignError::InvalidPublicKey)?;
    let verifying_key =
        VerifyingKey::from_bytes(&pk_array).map_err(|_| SignError::InvalidPublicKey)?;
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| SignError::VerificationFailed)
}

fn head_message(head: &Head) -> Result<Vec<u8>, SignError> {
    let value = serde_json::to_value(head).map_err(|e| SignError::Serde(e.to_string()))?;
    canonical_json(&value).map_err(|e| SignError::Canonical(e.to_string()))
}

fn decode_hex_exact(s: &str, nbytes: usize) -> Result<Vec<u8>, SignError> {
    if s.len() != nbytes * 2 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(SignError::InvalidSignature);
    }
    hex::decode(s).map_err(|_| SignError::InvalidSignature)
}
