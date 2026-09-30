//! Eligibility: only score what the poller could have seen.

use serde_json::json;
use tempfile::TempDir;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use refledger_canary_score::{
    canary_added_at, classify_eligibility, load_gaps, score, Eligibility, GapKind, LedgerLine,
    RecordedGap,
};

fn odt(
    year: i32,
    month: Month,
    day: u8,
    hour: u8,
    min: u8,
    sec: u8,
    milli: u16,
) -> OffsetDateTime {
    let time = Time::from_hms_milli(hour, min, sec, milli).unwrap();
    let date = Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn ts(t: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 1_000_000
    )
}

fn line_at(performed: OffsetDateTime) -> LedgerLine {
    LedgerLine {
        pattern: "exact_content_change".into(),
        tag: "v1.0.0".into(),
        from: "aaa".into(),
        to: "bbb".into(),
        performed_at: ts(performed),
    }
}

fn added_entry(repo: &str, at: OffsetDateTime) -> serde_json::Value {
    json!({
        "format_version": 1,
        "seq": 1,
        "prev_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "recorded_at": ts(at),
        "event": "population_change",
        "repo": repo,
        "population_change": {
            "change": "added",
            "reason": { "type": "manual" },
            "note": "canary"
        }
    })
}

fn move_entry(repo: &str, tag: &str, to: &str, at: OffsetDateTime) -> serde_json::Value {
    json!({
        "format_version": 1,
        "seq": 2,
        "prev_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        "recorded_at": ts(at),
        "event": "move",
        "repo": repo,
        "ref": format!("refs/tags/{tag}"),
        "to": {
            "commit_sha": to,
            "first_observed": ts(at)
        },
        "classification": "content_change"
    })
}

#[test]
fn pre_genesis_move_is_not_scored() {
    let repo = "GautamTalksDev/canary";
    let added = odt(2026, Month::October, 1, 0, 0, 0, 0);
    let early = odt(2026, Month::September, 28, 12, 0, 0, 0);
    let entries = vec![added_entry(repo, added)];
    let ledger = vec![line_at(early)];
    assert_eq!(
        classify_eligibility(&ledger[0], canary_added_at(&entries, repo), &[]),
        Eligibility::PreGenesis
    );
    let report = score(
        &ledger,
        &entries,
        &[],
        repo,
        odt(2026, Month::October, 2, 0, 0, 0, 0),
    );
    assert_eq!(report.scored.len(), 0);
    assert_eq!(report.pre_genesis.len(), 1);
    assert_eq!(report.detected, 0);
}

#[test]
fn move_during_poller_down_is_reported_in_gap_section() {
    let repo = "GautamTalksDev/canary";
    let added = odt(2026, Month::October, 1, 0, 0, 0, 0);
    let gap_from = odt(2026, Month::October, 1, 12, 0, 0, 0);
    let gap_to = odt(2026, Month::October, 1, 12, 20, 0, 0);
    let during = odt(2026, Month::October, 1, 12, 10, 0, 0);
    let entries = vec![added_entry(repo, added)];
    let gaps = vec![RecordedGap {
        kind: GapKind::PollerDown,
        from: gap_from,
        to: gap_to,
    }];
    let ledger = vec![line_at(during)];
    assert!(matches!(
        classify_eligibility(&ledger[0], Some(added), &gaps),
        Eligibility::DuringGap {
            kind: GapKind::PollerDown
        }
    ));
    let report = score(
        &ledger,
        &entries,
        &gaps,
        repo,
        odt(2026, Month::October, 2, 0, 0, 0, 0),
    );
    assert_eq!(report.scored.len(), 0);
    assert_eq!(report.during_gap.len(), 1);
    assert_eq!(report.during_gap[0].1, GapKind::PollerDown);
}

#[test]
fn normal_move_after_genesis_is_scored() {
    let repo = "GautamTalksDev/canary";
    let added = odt(2026, Month::October, 1, 0, 0, 0, 0);
    let performed = odt(2026, Month::October, 1, 1, 0, 0, 0);
    let detected_at = odt(2026, Month::October, 1, 1, 1, 0, 0);
    let entries = vec![
        added_entry(repo, added),
        move_entry(repo, "v1.0.0", "bbb", detected_at),
    ];
    let ledger = vec![line_at(performed)];
    assert_eq!(
        classify_eligibility(&ledger[0], Some(added), &[]),
        Eligibility::Scorable
    );
    let report = score(
        &ledger,
        &entries,
        &[],
        repo,
        odt(2026, Month::October, 2, 0, 0, 0, 0),
    );
    assert_eq!(report.scored.len(), 1);
    assert!(report.scored[0].detected);
    assert_eq!(report.pre_genesis.len(), 0);
    assert_eq!(report.during_gap.len(), 0);
    assert_eq!(report.detected, 1);
}

#[test]
fn load_gaps_reads_poller_down_from_observations() {
    let dir = TempDir::new().unwrap();
    let path = dir
        .path()
        .join("2026/10/01/GautamTalksDev--canary.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let from = odt(2026, Month::October, 1, 12, 0, 0, 0);
    let to = odt(2026, Month::October, 1, 12, 20, 0, 0);
    let obs = json!({
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "repo": "GautamTalksDev/canary",
        "observed_at": ts(to),
        "poller_version": "0.1.0",
        "method": "rest",
        "outcome": {
            "type": "skipped",
            "reason": {
                "poller_down": {
                    "from": ts(from),
                    "to": ts(to)
                }
            }
        }
    });
    std::fs::write(&path, format!("{obs}\n")).unwrap();
    let gaps = load_gaps(dir.path(), "GautamTalksDev/canary").unwrap();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].kind, GapKind::PollerDown);
    assert_eq!(gaps[0].from, from);
    assert_eq!(gaps[0].to, to);
}

#[test]
fn retired_patterns_are_not_scored() {
    use refledger_canary_score::score_with_corrections;
    let repo = "GautamTalksDev/canary";
    let added = odt(2026, Month::October, 1, 0, 0, 0, 0);
    let performed = odt(2026, Month::October, 1, 1, 0, 0, 0);
    let entries = vec![added_entry(repo, added)];
    let ledger = vec![
        LedgerLine {
            pattern: "lightweight_annotated_roundtrip".into(),
            tag: "v2".into(),
            from: "aaa".into(),
            to: "bbb".into(),
            performed_at: ts(performed),
        },
        LedgerLine {
            pattern: "delete_recreate".into(),
            tag: "v3.0.0".into(),
            from: "ccc".into(),
            to: "".into(),
            performed_at: ts(performed),
        },
    ];
    let report = score_with_corrections(
        &ledger,
        &[],
        &entries,
        &[],
        repo,
        odt(2026, Month::October, 2, 0, 0, 0, 0),
    );
    assert_eq!(report.scored.len(), 0);
    assert_eq!(report.retired_pattern.len(), 2);
    assert!(report.latencies.is_empty());
}

#[test]
fn manual_intervention_correction_excludes_from_latency() {
    use refledger_canary_score::{score_with_corrections, LedgerCorrection};
    let repo = "GautamTalksDev/canary";
    let added = odt(2026, Month::September, 29, 0, 0, 0, 0);
    let performed = odt(2026, Month::September, 30, 4, 51, 14, 0);
    let detected_at = odt(2026, Month::September, 30, 5, 7, 44, 0);
    let entries = vec![
        added_entry(repo, added),
        json!({
            "format_version": 1,
            "seq": 40,
            "prev_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "recorded_at": ts(detected_at),
            "event": "deletion",
            "repo": repo,
            "ref": "refs/tags/v3.0.0",
            "to": {
                "commit_sha": "0000000000000000000000000000000000000000",
                "first_observed": ts(detected_at)
            },
            "classification": "content_change"
        }),
    ];
    let line = LedgerLine {
        pattern: "delete".into(),
        tag: "v3.0.0".into(),
        from: "ca73a908125d15c79265e65fcb08c989849d547a".into(),
        to: "".into(),
        performed_at: ts(performed),
    };
    let corrections = vec![LedgerCorrection {
        kind: "correction".into(),
        corrects_performed_at: ts(performed),
        pattern: "delete".into(),
        tag: "v3.0.0".into(),
        correction_kind: "manual_intervention".into(),
        reason: "remote delete happened later by hand".into(),
    }];
    let report = score_with_corrections(
        &[line],
        &corrections,
        &entries,
        &[],
        repo,
        odt(2026, Month::October, 2, 0, 0, 0, 0),
    );
    assert_eq!(report.scored.len(), 0);
    assert_eq!(report.manual_intervention.len(), 1);
    assert!(report.latencies.is_empty());
    assert_eq!(report.detected, 0);
}
