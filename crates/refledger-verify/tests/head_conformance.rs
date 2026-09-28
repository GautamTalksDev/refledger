//! Head conformance: this crate's canonicaliser + pure Ed25519 verify must
//! match `tests/vectors/heads/*.json` independently of `refledger-log`.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::fs;
use std::path::PathBuf;
use refledger_verify::canonical_json;

#[derive(Debug, Deserialize)]
struct HeadVector {
    description: String,
    head: Value,
    expected_canonical: String,
    expected_signature_hex: String,
    expected_public_key_hex: String,
    expected_rekor_prehash_sha512_hex: String,
}

fn heads_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors/heads")
}

fn decode_hex(s: &str, nbytes: usize) -> [u8; 64] {
    assert_eq!(s.len(), nbytes * 2, "hex length");
    let v = hex::decode(s).expect("hex");
    let mut out = [0u8; 64];
    out[..nbytes].copy_from_slice(&v);
    out
}

#[test]
fn all_head_conformance_vectors_match_this_crates_canonicaliser() {
    let dir = heads_dir();
    assert!(
        dir.is_dir(),
        "missing heads vectors dir at {}",
        dir.display()
    );

    let mut paths: Vec<_> = fs::read_dir(&dir)
        .expect("read heads")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 4,
        "expected at least 4 head vectors, found {}",
        paths.len()
    );

    for path in &paths {
        let v: HeadVector = serde_json::from_str(&fs::read_to_string(path).unwrap())
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        assert!(!v.description.is_empty());

        let bytes = canonical_json(&v.head).unwrap_or_else(|e| {
            panic!("canonicalise {} ({}): {e}", v.description, path.display());
        });
        assert_eq!(
            String::from_utf8_lossy(&bytes).as_ref(),
            v.expected_canonical.as_str(),
            "canonical bytes diverge for {} ({})",
            v.description,
            path.display()
        );

        let prehash = hex::encode(Sha512::digest(&bytes));
        assert_eq!(
            prehash, v.expected_rekor_prehash_sha512_hex,
            "rekor prehash diverge for {} ({})",
            v.description,
            path.display()
        );

        let pk_raw = hex::decode(&v.expected_public_key_hex).expect("pk hex");
        let pk: [u8; 32] = pk_raw.try_into().expect("32-byte pk");
        let verifying_key = VerifyingKey::from_bytes(&pk).expect("verifying key");
        let sig_raw = decode_hex(&v.expected_signature_hex, 64);
        let signature = Signature::from_slice(&sig_raw).expect("signature");
        verifying_key
            .verify(&bytes, &signature)
            .unwrap_or_else(|e| {
                panic!(
                    "Ed25519 verify failed for {} ({}): {e}",
                    v.description,
                    path.display()
                )
            });
    }
}
