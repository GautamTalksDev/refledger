//! Head conformance vectors (`tests/vectors/heads/*.json`).
//!
//! Covers the object that is signed and Rekor-witnessed. Both this crate and
//! `refledger-verify` must reproduce canonical bytes, pure Ed25519 signature,
//! and SHA-512 Rekor prehash for every vector.

use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::fs;
use std::path::PathBuf;
use refledger_log::canonical_json;
use refledger_log::sign::{sign_head, verify_head, Head, SigningKey};

#[derive(Debug, Deserialize)]
struct HeadVector {
    description: String,
    test_key_seed_hex: String,
    head: Head,
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

#[test]
fn all_head_conformance_vectors_match_refledger_log() {
    let dir = heads_dir();
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 4,
        "expected at least 4 head vectors in {}, found {}",
        dir.display(),
        paths.len()
    );

    for path in &paths {
        let v: HeadVector = serde_json::from_str(&fs::read_to_string(path).unwrap())
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        assert!(!v.description.is_empty(), "{} missing description", path.display());

        let seed = hex::decode(&v.test_key_seed_hex).expect("seed hex");
        let seed: [u8; 32] = seed.try_into().expect("32-byte seed");
        let key = SigningKey::from_seed_bytes(&seed).expect("key");

        let value = serde_json::to_value(&v.head).expect("head value");
        let bytes = canonical_json(&value)
            .unwrap_or_else(|e| panic!("canonicalise {} ({}): {e}", v.description, path.display()));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            v.expected_canonical.as_str(),
            "canonical bytes: {} ({})",
            v.description,
            path.display()
        );

        let signed = sign_head(&v.head, &key).expect("sign");
        assert_eq!(
            signed.signature, v.expected_signature_hex,
            "signature: {} ({})",
            v.description,
            path.display()
        );
        assert_eq!(
            signed.public_key, v.expected_public_key_hex,
            "public key: {} ({})",
            v.description,
            path.display()
        );
        verify_head(&signed).expect("verify");

        let prehash = hex::encode(Sha512::digest(&bytes));
        assert_eq!(
            prehash, v.expected_rekor_prehash_sha512_hex,
            "rekor prehash: {} ({})",
            v.description,
            path.display()
        );
    }
}
