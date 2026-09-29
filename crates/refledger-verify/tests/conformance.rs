//! Conformance: this crate's canonicaliser must match `tests/vectors/*.json`.
//!
//! These are the same vector files `refledger-log`'s tests consume. Divergence
//! between the two implementations is exactly what this test exists to catch —
//! and it is the single most valuable test in the repo.

use refledger_verify::canonical_json;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Vector {
    description: String,
    input: serde_json::Value,
    expected_canonical: String,
    expected_hash: String,
}

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors")
}

#[test]
fn all_conformance_vectors_match_this_crates_canonicaliser() {
    let dir = vectors_dir();
    assert!(
        dir.is_dir(),
        "missing vectors dir at {} — create tests/vectors/*.json",
        dir.display()
    );

    let mut paths: Vec<_> = fs::read_dir(&dir)
        .expect("read vectors")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 20,
        "expected at least 20 vectors, found {}",
        paths.len()
    );

    for path in &paths {
        let raw = fs::read_to_string(path).expect("read vector");
        let v: Vector = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("parse {}: {e}", path.display());
        });
        assert!(!v.description.is_empty());

        let bytes = canonical_json(&v.input).unwrap_or_else(|e| {
            panic!("canonicalise {} ({}): {e}", v.description, path.display());
        });
        assert_eq!(
            String::from_utf8_lossy(&bytes).as_ref(),
            v.expected_canonical.as_str(),
            "canonical bytes diverge for {} ({})",
            v.description,
            path.display()
        );
        let hash = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        assert_eq!(
            hash,
            v.expected_hash,
            "hash diverge for {} ({})",
            v.description,
            path.display()
        );
    }
}
