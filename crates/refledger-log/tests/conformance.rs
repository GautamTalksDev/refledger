//! Shared conformance vectors (`tests/vectors/*.json`).
//!
//! Same files consumed by `refledger-verify`. If this test and the verifier's
//! conformance test disagree, the canonicalisers have diverged.

use refledger_log::canonical_json;
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
fn all_conformance_vectors_match_refledger_log_canonicaliser() {
    let dir = vectors_dir();
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 20,
        "expected at least 20 vectors in {}, found {}",
        dir.display(),
        paths.len()
    );

    for path in &paths {
        let v: Vector = serde_json::from_str(&fs::read_to_string(path).unwrap())
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        assert!(
            !v.description.is_empty(),
            "{} missing description",
            path.display()
        );
        let bytes = canonical_json(&v.input)
            .unwrap_or_else(|e| panic!("canonicalise {} ({}): {e}", v.description, path.display()));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            v.expected_canonical.as_str(),
            "canonical bytes: {} ({})",
            v.description,
            path.display()
        );
        let hash = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        assert_eq!(
            hash,
            v.expected_hash,
            "hash: {} ({})",
            v.description,
            path.display()
        );
    }
}
