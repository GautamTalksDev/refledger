//! Entry-shape invariants the verifier enforces beyond hash chaining.
//!
//! - Correlation `member_seqs` must all be strictly less than the entry's seq.
//! - Published ObservationDigest hashes must match observation files when present.

use refledger_verify::{
    entry_hash, verify_chain, verify_observation_digests, Failure, VerifyError,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn genesis_prev() -> String {
    format!("sha256:{}", "0".repeat(64))
}

fn hashed(mut entry: serde_json::Value) -> serde_json::Value {
    let h = entry_hash(&entry).expect("hash");
    entry
        .as_object_mut()
        .unwrap()
        .insert("entry_hash".into(), serde_json::Value::String(h));
    entry
}

#[test]
fn correlation_member_seq_ge_own_seq_is_rejected() {
    let entry = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 0,
        "prev_hash": genesis_prev(),
        "recorded_at": "2026-01-01T00:00:00.000Z",
        "event": "correlation",
        "correlation": {
            "batch_id": "b",
            "member_seqs": [0],
            "refs_moved_together": ["refs/tags/v1.0.0"],
            "all_to_same_target": true
        }
    }));
    let err = verify_chain(&[entry]).expect_err("member_seq >= seq");
    match err {
        VerifyError::Failed {
            failure: Failure::CorrelationMemberSeq { seq: 0 },
        } => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn correlation_member_seqs_pointing_backwards_pass() {
    let e0 = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 0,
        "prev_hash": genesis_prev(),
        "recorded_at": "2026-01-01T00:00:00.000Z",
        "event": "correction",
        "corrects_seq": 0,
        "reason": "genesis"
    }));
    let tip = e0
        .get("entry_hash")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let e1 = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 1,
        "prev_hash": tip,
        "recorded_at": "2026-01-01T01:00:00.000Z",
        "event": "correlation",
        "correlation": {
            "batch_id": "b",
            "member_seqs": [0],
            "refs_moved_together": ["refs/tags/v1.0.0"],
            "all_to_same_target": true
        }
    }));
    verify_chain(&[e0, e1]).expect("valid correlation");
}

#[test]
fn observation_digest_matches_files_when_present() {
    let dir = TempDir::new().unwrap();
    let day = dir.path().join("2026/08/02");
    fs::create_dir_all(&day).unwrap();
    let file_a = day.join("acme--widgets.jsonl");
    let file_b = day.join("acme--gone.jsonl");
    fs::write(&file_a, b"obs-a\n").unwrap();
    fs::write(&file_b, b"obs-b\n").unwrap();
    let sha_a = hex::encode(Sha256::digest(b"obs-a\n"));
    let sha_b = hex::encode(Sha256::digest(b"obs-b\n"));

    let entry = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 0,
        "prev_hash": genesis_prev(),
        "recorded_at": "2026-08-03T00:00:00.000Z",
        "event": "observation_digest",
        "observation_digest": {
            "date": "2026-08-02",
            "repos_polled": 2,
            "ok": 2,
            "not_modified": 0,
            "failed": 0,
            "skipped": 0,
            "files": [
                {
                    "path": "2026/08/02/acme--gone.jsonl",
                    "sha256": sha_b
                },
                {
                    "path": "2026/08/02/acme--widgets.jsonl",
                    "sha256": sha_a
                }
            ]
        }
    }));
    verify_chain(std::slice::from_ref(&entry)).expect("chain ok");
    verify_observation_digests(std::slice::from_ref(&entry), dir.path())
        .expect("digest matches files");
}

#[test]
fn observation_digest_mismatch_is_rejected() {
    let dir = TempDir::new().unwrap();
    let day = dir.path().join("2026/08/02");
    fs::create_dir_all(&day).unwrap();
    let file = day.join("acme--widgets.jsonl");
    fs::write(&file, b"real-bytes\n").unwrap();

    let entry = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 0,
        "prev_hash": genesis_prev(),
        "recorded_at": "2026-08-03T00:00:00.000Z",
        "event": "observation_digest",
        "observation_digest": {
            "date": "2026-08-02",
            "repos_polled": 1,
            "ok": 1,
            "not_modified": 0,
            "failed": 0,
            "skipped": 0,
            "files": [
                {
                    "path": "2026/08/02/acme--widgets.jsonl",
                    "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            ]
        }
    }));
    let err = verify_observation_digests(&[entry], dir.path()).expect_err("mismatch");
    match err {
        VerifyError::Failed {
            failure: Failure::ObservationDigestMismatch { seq: 0, .. },
        } => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn missing_format_version_is_rejected() {
    let entry = hashed(serde_json::json!({
        "seq": 0,
        "prev_hash": genesis_prev(),
        "recorded_at": "2026-01-01T00:00:00.000Z",
        "event": "correction",
        "corrects_seq": 0,
        "reason": "genesis"
    }));
    let err = verify_chain(&[entry]).expect_err("no format_version");
    match err {
        VerifyError::Failed {
            failure: Failure::InvalidFormatVersion { seq: 0 },
        } => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn vectors_dir_exists() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors");
    assert!(dir.is_dir(), "{}", dir.display());
}
