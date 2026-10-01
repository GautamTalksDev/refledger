//! --strict: multiple heads.jsonl lines for one seq must agree on head+signature.

use refledger_verify::{verify_heads_witness_identity, verify_strict, Failure, VerifyError};
use serde_json::json;

fn heads_two_agree() -> String {
    let head = json!({
        "seq": 42,
        "entry_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "recorded_at": "2026-10-01T00:00:00.000Z",
        "log_id": "refledger"
    });
    let line1 = json!({
        "head": head,
        "signature": "aa".repeat(32),
        "public_key": "bb".repeat(32),
        "key_id": "sha256:cc",
        "rekor": {"error": "409", "attempts": 1}
    });
    let mut line2 = line1.clone();
    line2["rekor"] = json!({"log_index": 1, "attempts": 2, "uuid": "u"});
    format!("{}\n{}\n", line1, line2)
}

fn heads_two_disagree() -> String {
    let mut lines: Vec<serde_json::Value> = heads_two_agree()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[1]["signature"] = json!("dd".repeat(32));
    format!("{}\n{}\n", lines[0], lines[1])
}

#[test]
fn strict_accepts_matching_witness_attempt_lines() {
    let heads = heads_two_agree();
    verify_heads_witness_identity(&heads).unwrap();
    let entries = vec![json!({
        "seq": 0,
        "recorded_at": "2026-10-01T00:00:00.000Z",
        "event": "correction"
    })];
    // verify_strict also needs tip time; backlog check may pass if within 48h of tip.
    verify_strict(&entries, &heads).unwrap();
}

#[test]
fn strict_rejects_disagreeing_head_or_signature_for_same_seq() {
    let heads = heads_two_disagree();
    let err = verify_heads_witness_identity(&heads).unwrap_err();
    match err {
        VerifyError::Failed {
            failure: Failure::HeadWitnessIdentityMismatch { seq, first, second },
        } => {
            assert_eq!(seq, 42);
            assert_eq!(first, 1);
            assert_eq!(second, 2);
        }
        other => panic!("expected identity mismatch, got {other}"),
    }
}
