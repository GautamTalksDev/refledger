//! Generate entry-level conformance vectors under tests/vectors/.
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use refledger_log::canonical_json;

fn sha40(digit: char) -> String {
    std::iter::repeat(digit).take(40).collect()
}

fn ulid_a() -> &'static str {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV"
}
fn ulid_b() -> &'static str {
    "01ARZ3NDEKTSV4RRFFQ69G5FAW"
}

fn hash_of(value: &serde_json::Value) -> (String, String) {
    let bytes = canonical_json(value).expect("canonical");
    let canonical = String::from_utf8(bytes.clone()).expect("utf8");
    let hash = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    (canonical, hash)
}

fn write_vector(name: &str, description: &str, input: serde_json::Value) {
    let (expected_canonical, expected_hash) = hash_of(&input);
    let doc = serde_json::json!({
        "description": description,
        "input": input,
        "expected_canonical": expected_canonical,
        "expected_hash": expected_hash,
    });
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors");
    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{name}.json"));
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    println!(
        "wrote {} ({} bytes canonical)",
        path.display(),
        expected_canonical.len()
    );
}

fn binding(
    target: char,
    commit: char,
    tree: char,
    first: &str,
    last: Option<&str>,
    count: u64,
    stable_days: u64,
    action_yml: Option<char>,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("target_sha".into(), sha40(target).into());
    m.insert("commit_sha".into(), sha40(commit).into());
    m.insert("tree_sha".into(), sha40(tree).into());
    m.insert("first_observed".into(), first.into());
    if let Some(l) = last {
        m.insert("last_observed".into(), l.into());
    }
    m.insert("observation_count".into(), count.into());
    m.insert("stable_days".into(), stable_days.into());
    if let Some(a) = action_yml {
        m.insert("action_yml_sha".into(), sha40(a).into());
    }
    serde_json::Value::Object(m)
}

fn sources() -> serde_json::Value {
    serde_json::json!([ulid_a(), ulid_b()])
}

fn main() {
    let genesis_prev = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    write_vector(
        "entry_move_minimal",
        "Minimal move entry (format_version 1): required move fields including source_observations.",
        serde_json::json!({
            "format_version": 1,
            "seq": 1,
            "prev_hash": format!("sha256:{}", "11".repeat(32)),
            "recorded_at": "2026-03-15T12:00:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "high",
            "ref_form": "exact",
            "ancestry": "ahead",
            "repo": "acme/widgets",
            "ref": "refs/tags/v1.0.0",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('a','b','c',"2026-01-01T00:00:00.000Z", Some("2026-01-11T00:00:00.000Z"), 4, 10, None),
            "to": binding('d','e','f',"2026-03-15T12:00:00.000Z", None, 1, 0, None),
            "source_observations": sources(),
            "observation_window_seconds": 3600
        }),
    );

    write_vector(
        "entry_move_full_diff",
        "Move with Diff including diff_possibly_truncated=false and a complete path list.",
        serde_json::json!({
            "format_version": 1,
            "seq": 2,
            "prev_hash": format!("sha256:{}", "22".repeat(32)),
            "recorded_at": "2026-03-16T08:30:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "high",
            "ref_form": "exact",
            "ancestry": "ahead",
            "repo": "acme/widgets",
            "ref": "refs/tags/v1.1.0",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('1','2','3',"2026-02-01T00:00:00.000Z", Some("2026-03-01T00:00:00.000Z"), 8, 28, None),
            "to": binding('4','5','6',"2026-03-16T08:30:00.000Z", None, 1, 0, None),
            "diff": {
                "files_added": 2,
                "files_removed": 1,
                "files_modified": 3,
                "files_renamed": 1,
                "paths": ["src/lib.rs", "src/main.rs", "README.md", "Cargo.toml", "old/name.rs", "new/name.rs"],
                "diff_possibly_truncated": false
            },
            "source_observations": sources(),
            "observation_window_seconds": 7200
        }),
    );

    // Correlation is a standalone event (not inlined on Move).
    let refs_346: Vec<String> = (0..346).map(|i| format!("refs/tags/comp-{i:03}")).collect();
    let member_seqs: Vec<u64> = (1..347).collect();
    write_vector(
        "entry_correlation_346",
        "Standalone Correlation event: member_seqs all strictly below own seq; Trivy/tj-actions shape.",
        serde_json::json!({
            "format_version": 1,
            "seq": 347,
            "prev_hash": format!("sha256:{}", "33".repeat(32)),
            "recorded_at": "2026-03-15T18:00:00.000Z",
            "event": "correlation",
            "repo": "aquasecurity/trivy",
            "correlation": {
                "batch_id": "trivy-tj-actions-2026-03-15",
                "member_seqs": member_seqs,
                "refs_moved_together": refs_346,
                "all_to_same_target": true,
                "note": "At least 3 pre-existing exact-version tags moved to the same target within the correlation window."
            }
        }),
    );

    write_vector(
        "entry_deletion",
        "Deletion event with source_observations.",
        serde_json::json!({
            "format_version": 1,
            "seq": 4,
            "prev_hash": format!("sha256:{}", "44".repeat(32)),
            "recorded_at": "2026-04-01T10:00:00.000Z",
            "event": "deletion",
            "classification": "content_change",
            "severity": "info",
            "ref_form": "exact",
            "repo": "acme/widgets",
            "ref": "refs/tags/v0.9.0",
            "ref_type_before": "lightweight",
            "ref_type_after": "lightweight",
            "from": binding('a','b','c',"2025-12-01T00:00:00.000Z", Some("2026-04-01T09:00:00.000Z"), 12, 121, None),
            "to": binding('a','b','c',"2026-04-01T10:00:00.000Z", None, 1, 0, None),
            "source_observations": sources(),
            "observation_window_seconds": 3600
        }),
    );

    write_vector(
        "entry_recreation",
        "Recreation with gap_seconds and both bindings.",
        serde_json::json!({
            "format_version": 1,
            "seq": 5,
            "prev_hash": format!("sha256:{}", "55".repeat(32)),
            "recorded_at": "2026-04-02T11:00:00.000Z",
            "event": "recreation",
            "classification": "release_level_only",
            "severity": "medium",
            "ref_form": "exact",
            "repo": "acme/widgets",
            "ref": "refs/tags/v0.9.0",
            "ref_type_before": "lightweight",
            "ref_type_after": "annotated",
            "from": binding('a','b','c',"2026-04-01T10:00:00.000Z", Some("2026-04-01T10:00:00.000Z"), 1, 0, None),
            "to": binding('f','0','1',"2026-04-02T11:00:00.000Z", None, 1, 0, None),
            "gap_seconds": 90000,
            "source_observations": sources(),
            "observation_window_seconds": 1800
        }),
    );

    write_vector(
        "entry_correction",
        "Correction entry: corrects_seq + reason only.",
        serde_json::json!({
            "format_version": 1,
            "seq": 6,
            "prev_hash": format!("sha256:{}", "66".repeat(32)),
            "recorded_at": "2026-04-03T00:00:00.000Z",
            "event": "correction",
            "corrects_seq": 2,
            "reason": "misclassified commit_metadata_only move as content_change"
        }),
    );

    write_vector(
        "entry_lightweight_to_annotated",
        "Release-level move: lightweight to annotated; severity info.",
        serde_json::json!({
            "format_version": 1,
            "seq": 7,
            "prev_hash": format!("sha256:{}", "77".repeat(32)),
            "recorded_at": "2026-05-01T09:15:00.000Z",
            "event": "move",
            "classification": "release_level_only",
            "severity": "info",
            "ref_form": "floating_major",
            "repo": "acme/widgets",
            "ref": "refs/tags/v2",
            "ref_type_before": "lightweight",
            "ref_type_after": "annotated",
            "from": binding('2','3','4',"2026-04-01T00:00:00.000Z", Some("2026-04-20T00:00:00.000Z"), 5, 19, None),
            "to": binding('5','3','4',"2026-05-01T09:15:00.000Z", None, 1, 0, None),
            "source_observations": sources(),
            "observation_window_seconds": 600
        }),
    );

    write_vector(
        "entry_optionals_absent",
        "Sparsest valid entry: correction with only required envelope fields.",
        serde_json::json!({
            "format_version": 1,
            "seq": 8,
            "prev_hash": format!("sha256:{}", "88".repeat(32)),
            "recorded_at": "2026-05-02T00:00:00.000Z",
            "event": "correction",
            "corrects_seq": 0,
            "reason": "fixture: optionals absent"
        }),
    );

    write_vector(
        "entry_optionals_present",
        "Move with every move-optional field present, including ancestry and ref_form.",
        serde_json::json!({
            "format_version": 1,
            "seq": 9,
            "prev_hash": format!("sha256:{}", "99".repeat(32)),
            "entry_hash": format!("sha256:{}", "ab".repeat(32)),
            "recorded_at": "2026-05-03T12:00:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "high",
            "ref_form": "exact",
            "ancestry": "diverged",
            "repo": "acme/widgets",
            "ref": "refs/tags/v3.0.0",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('a','b','c',"2026-01-01T00:00:00.000Z", Some("2026-05-01T00:00:00.000Z"), 30, 120, Some('d')),
            "to": binding('e','f','0',"2026-05-03T12:00:00.000Z", None, 1, 0, Some('1')),
            "diff": {
                "files_added": 1,
                "files_removed": 0,
                "files_modified": 1,
                "files_renamed": 0,
                "paths": ["action.yml", "README.md"],
                "diff_possibly_truncated": false
            },
            "source_observations": sources(),
            "observation_window_seconds": 120,
            "detection_latency_note": "observed within one poll interval"
        }),
    );

    write_vector(
        "entry_unicode_repo_ref",
        "Move with Unicode in repo and ref names.",
        serde_json::json!({
            "format_version": 1,
            "seq": 10,
            "prev_hash": format!("sha256:{}", "aa".repeat(32)),
            "recorded_at": "2026-06-01T00:00:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "medium",
            "ref_form": "exact",
            "ancestry": "ahead",
            "repo": "例/リポジトリ",
            "ref": "refs/tags/バージョン-1.0.0-😀",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('1','2','3',"2026-05-01T00:00:00.000Z", Some("2026-05-31T00:00:00.000Z"), 3, 30, None),
            "to": binding('4','5','6',"2026-06-01T00:00:00.000Z", None, 1, 0, None),
            "source_observations": sources(),
            "observation_window_seconds": 3600
        }),
    );

    let paths_5k: Vec<String> = (0..5000)
        .map(|i| format!("vendor/pkg-{i:04}/src/lib.rs"))
        .collect();
    write_vector(
        "entry_diff_5000_paths",
        "Move Diff.paths with 5,000 paths; diff_possibly_truncated=false.",
        serde_json::json!({
            "format_version": 1,
            "seq": 11,
            "prev_hash": format!("sha256:{}", "bb".repeat(32)),
            "recorded_at": "2026-07-01T00:00:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "high",
            "ref_form": "exact",
            "ancestry": "ahead",
            "repo": "acme/monorepo",
            "ref": "refs/tags/v10.0.0",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('7','8','9',"2026-06-01T00:00:00.000Z", Some("2026-06-30T00:00:00.000Z"), 10, 29, None),
            "to": binding('a','b','c',"2026-07-01T00:00:00.000Z", None, 1, 0, None),
            "diff": {
                "files_added": 5000,
                "files_removed": 0,
                "files_modified": 0,
                "files_renamed": 0,
                "paths": paths_5k,
                "diff_possibly_truncated": false
            },
            "source_observations": sources(),
            "observation_window_seconds": 86400
        }),
    );

    write_vector(
        "entry_genesis",
        "Genesis entry: seq 0, format_version 1, zero prev_hash.",
        serde_json::json!({
            "format_version": 1,
            "seq": 0,
            "prev_hash": genesis_prev,
            "recorded_at": "2026-01-01T00:00:00.000Z",
            "event": "correction",
            "corrects_seq": 0,
            "reason": "genesis placeholder — log opened"
        }),
    );

    write_vector(
        "entry_repo_unavailable",
        "RepoUnavailable: one event for a 404, not N tag deletions.",
        serde_json::json!({
            "format_version": 1,
            "seq": 12,
            "prev_hash": format!("sha256:{}", "cc".repeat(32)),
            "recorded_at": "2026-08-01T00:00:00.000Z",
            "event": "repo_unavailable",
            "repo": "acme/gone",
            "http_status": 404
        }),
    );

    write_vector(
        "entry_repo_redirected",
        "RepoRedirected: 301 recorded, not followed.",
        serde_json::json!({
            "format_version": 1,
            "seq": 13,
            "prev_hash": format!("sha256:{}", "dd".repeat(32)),
            "recorded_at": "2026-08-02T00:00:00.000Z",
            "event": "repo_redirected",
            "repo": "acme/old-name",
            "http_status": 301,
            "redirect_location": "https://api.github.com/repositories/999001"
        }),
    );

    write_vector(
        "entry_observation_digest",
        "Daily ObservationDigest committing to that day's observation JSONL files.",
        serde_json::json!({
            "format_version": 1,
            "seq": 14,
            "prev_hash": format!("sha256:{}", "ee".repeat(32)),
            "recorded_at": "2026-08-03T00:00:00.000Z",
            "event": "observation_digest",
            "observation_digest": {
                "date": "2026-08-02",
                "repos_polled": 3,
                "ok": 2,
                "not_modified": 10,
                "failed": 1,
                "skipped": 0,
                "files": [
                    {"path": "data/observations/2026/08/02/acme--gone.jsonl", "sha256": "aa".repeat(32)},
                    {"path": "data/observations/2026/08/02/acme--widgets.jsonl", "sha256": "bb".repeat(32)}
                ]
            }
        }),
    );

    write_vector(
        "entry_observation_digest_recovery_note",
        "ObservationDigest optional note: a torn write preserved beside the log. Absent note is omitted, never null.",
        serde_json::json!({
            "format_version": 1,
            "seq": 14,
            "prev_hash": format!("sha256:{}", "ee".repeat(32)),
            "recorded_at": "2026-08-03T00:00:00.000Z",
            "event": "observation_digest",
            "observation_digest": {
                "date": "2026-08-02",
                "repos_polled": 0,
                "ok": 0,
                "not_modified": 0,
                "failed": 0,
                "skipped": 0,
                "files": [],
                "note": "torn write preserved: log/2026/08/02.jsonl.torn.20260803T000000.000Z (17 bytes)"
            }
        }),
    );

    write_vector(
        "entry_diff_possibly_truncated",
        "Diff with diff_possibly_truncated=true and empty paths (cap reached).",
        serde_json::json!({
            "format_version": 1,
            "seq": 15,
            "prev_hash": format!("sha256:{}", "ff".repeat(32)),
            "recorded_at": "2026-08-04T00:00:00.000Z",
            "event": "move",
            "classification": "content_change",
            "severity": "high",
            "ref_form": "exact",
            "ancestry": "ahead",
            "repo": "torvalds/linux",
            "ref": "refs/tags/v6.0",
            "ref_type_before": "annotated",
            "ref_type_after": "annotated",
            "from": binding('1','2','3',"2026-01-01T00:00:00.000Z", Some("2026-08-01T00:00:00.000Z"), 5, 212, None),
            "to": binding('4','5','6',"2026-08-04T00:00:00.000Z", None, 1, 0, None),
            "diff": {
                "files_added": 100,
                "files_removed": 50,
                "files_modified": 150,
                "files_renamed": 0,
                "paths": [],
                "diff_possibly_truncated": true
            },
            "source_observations": sources(),
            "observation_window_seconds": 3600
        }),
    );

    write_vector(
        "entry_population_change_seed",
        "PopulationChange Added with Seed reason — seed set membership.",
        serde_json::json!({
            "format_version": 1,
            "seq": 16,
            "prev_hash": format!("sha256:{}", "a1".repeat(32)),
            "recorded_at": "2026-09-28T00:00:00.000Z",
            "event": "population_change",
            "repo": "actions/checkout",
            "population_change": {
                "change": "added",
                "reason": {"type": "seed"},
                "note": "official_org:actions/*"
            }
        }),
    );

    write_vector(
        "entry_population_change_transitive",
        "PopulationChange Added Transitive via parent action@commit (Trivy path).",
        serde_json::json!({
            "format_version": 1,
            "seq": 17,
            "prev_hash": format!("sha256:{}", "a2".repeat(32)),
            "recorded_at": "2026-09-28T00:01:00.000Z",
            "event": "population_change",
            "repo": "aquasecurity/setup-trivy",
            "population_change": {
                "change": "added",
                "reason": {
                    "type": "transitive",
                    "via": "aquasecurity/trivy-action@0123456789abcdef0123456789abcdef01234567"
                }
            }
        }),
    );

    write_vector(
        "entry_population_change_removed",
        "PopulationChange Removed — observation stops; history retained.",
        serde_json::json!({
            "format_version": 1,
            "seq": 18,
            "prev_hash": format!("sha256:{}", "a3".repeat(32)),
            "recorded_at": "2026-09-28T00:02:00.000Z",
            "event": "population_change",
            "repo": "acme/widgets",
            "population_change": {
                "path": "nested/action",
                "change": "removed",
                "reason": {"type": "manual"},
                "note": "maintainer request"
            }
        }),
    );

    // Remove obsolete vector if present
    let obsolete = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors/entry_move_correlation_346.json");
    if obsolete.exists() {
        fs::remove_file(&obsolete).unwrap();
        println!("removed obsolete {}", obsolete.display());
    }
}
