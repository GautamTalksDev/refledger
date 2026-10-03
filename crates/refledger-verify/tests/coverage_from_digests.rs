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

struct DigestCoverage {
    seq: u64,
    date: String,
    skipped: u64,
    failed: u64,
}

struct CoverageSums {
    skipped: u64,
    failed: u64,
    rows: Vec<DigestCoverage>,
}

/// Sum `skipped` / `failed` from every `observation_digest` entry in the
/// published chain. The test asserts the verifier reports those sums, not a
/// hardcoded snapshot that goes stale when the next day seals.
fn coverage_from_signed_digests(entries: &[serde_json::Value]) -> CoverageSums {
    let mut skipped = 0u64;
    let mut failed = 0u64;
    let mut rows = Vec::new();
    for entry in entries {
        if entry.get("event").and_then(|v| v.as_str()) != Some("observation_digest") {
            continue;
        }
        let digest = entry
            .get("observation_digest")
            .expect("observation_digest body");
        let s = digest.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);
        let f = digest.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
        let seq = entry.get("seq").and_then(|v| v.as_u64()).expect("seq");
        let date = digest
            .get("date")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        skipped += s;
        failed += f;
        rows.push(DigestCoverage {
            seq,
            date,
            skipped: s,
            failed: f,
        });
    }
    CoverageSums {
        skipped,
        failed,
        rows,
    }
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

    let sums = coverage_from_signed_digests(&loaded.entries);
    assert!(
        !sums.rows.is_empty(),
        "published data/log must contain at least one observation_digest"
    );
    // One digest per sealed calendar day. A second digest for the same date
    // would mean a double seal; fail loudly instead of summing quietly.
    let mut seen_dates = std::collections::BTreeSet::new();
    for row in &sums.rows {
        assert!(
            seen_dates.insert(row.date.clone()),
            "date {} has more than one observation_digest (seq {}); refuse to treat that as a simple sum",
            row.date,
            row.seq
        );
        eprintln!(
            "digest seq={} date={} skipped={} failed={}",
            row.seq, row.date, row.skipped, row.failed
        );
    }

    assert_eq!(
        verdict.coverage_skipped, sums.skipped,
        "verifier skipped sum must match signed digests"
    );
    assert_eq!(
        verdict.coverage_failed, sums.failed,
        "verifier failed sum must match signed digests"
    );

    // CLI line must match the signed digest counts, not a raw coverage_gap event tally.
    // Non-strict: --strict rejects this log once any entry is more than 48h
    // older than the latest head.
    let bin = env!("CARGO_BIN_EXE_refledger-verify");
    let out = Command::new(bin)
        .args([log_dir.to_str().unwrap()])
        .output()
        .expect("run verifier");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "verifier must pass on published data/log\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let expect_line = format!(
        "coverage gaps recorded: {} skipped, {} failed polls (from signed digests)",
        sums.skipped, sums.failed
    );
    assert!(
        stdout.contains(&expect_line),
        "unexpected coverage line (wanted {expect_line:?}):\n{stdout}"
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
