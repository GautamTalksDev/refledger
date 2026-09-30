//! Coverage line must reflect ObservationDigest skipped/failed sums.

use refledger_verify::{load_jsonl_dir, verify_chain};
use std::path::PathBuf;
use std::process::Command;

fn repo_data_log() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("../../data/log");
    if candidate.is_dir() && candidate.join("heads.jsonl").is_file() {
        return Some(candidate);
    }
    None
}

#[test]
fn published_data_log_coverage_from_signed_digests() {
    let Some(log_dir) = repo_data_log() else {
        // Workspace without a sealed main tip (for example a sparse checkout).
        eprintln!("skip: data/log not present beside the workspace");
        return;
    };

    let loaded = load_jsonl_dir(&log_dir).expect("load published data/log");
    let verdict = verify_chain(&loaded.entries).expect("verify published chain");
    assert!(verdict.ok);
    assert_eq!(
        verdict.coverage_skipped, 215,
        "2026-09-29 digest records skipped=215"
    );
    assert_eq!(
        verdict.coverage_failed, 1,
        "2026-09-29 digest records failed=1"
    );

    // CLI line must match the signed digest counts, not a raw coverage_gap event tally.
    let bin = env!("CARGO_BIN_EXE_refledger-verify");
    let out = Command::new(bin)
        .args([
            log_dir.to_str().unwrap(),
            "--strict",
            "--pubkey",
            "b3e7e795c35dee53731e039b76da930fc54e87e2edc632449a8a2e55252e276a",
        ])
        .output()
        .expect("run verifier");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "verifier must pass on published data/log\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout
            .contains("coverage gaps recorded: 215 skipped, 1 failed polls (from signed digests)"),
        "unexpected coverage line:\n{stdout}"
    );
}

#[test]
fn digest_skipped_and_failed_sum_across_entries() {
    use refledger_verify::entry_hash;

    fn hashed(mut entry: serde_json::Value) -> serde_json::Value {
        let h = entry_hash(&entry).unwrap();
        entry
            .as_object_mut()
            .unwrap()
            .insert("entry_hash".into(), serde_json::Value::String(h));
        entry
    }

    let prev0 = format!("sha256:{}", "0".repeat(64));
    let e0 = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 0,
        "prev_hash": prev0,
        "recorded_at": "2026-01-02T00:00:00.000Z",
        "event": "observation_digest",
        "observation_digest": {
            "date": "2026-01-01",
            "repos_polled": 1,
            "ok": 0,
            "not_modified": 0,
            "failed": 2,
            "skipped": 10,
            "files": []
        }
    }));
    let prev1 = e0.get("entry_hash").unwrap().clone();
    let e1 = hashed(serde_json::json!({
        "format_version": 1,
        "seq": 1,
        "prev_hash": prev1,
        "recorded_at": "2026-01-03T00:00:00.000Z",
        "event": "observation_digest",
        "observation_digest": {
            "date": "2026-01-02",
            "repos_polled": 1,
            "ok": 0,
            "not_modified": 0,
            "failed": 1,
            "skipped": 5,
            "files": []
        }
    }));
    let v = verify_chain(&[e0, e1]).unwrap();
    assert_eq!(v.coverage_skipped, 15);
    assert_eq!(v.coverage_failed, 3);
}
