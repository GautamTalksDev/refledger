//! Entry schema tests (spec §9.2).

use refledger_log::entry::{
    Binding, Classification, Diff, Entry, EntryError, Event, HashRef, PopulationChange,
    PopulationChangeKind, PopulationReason, RefType, Severity, Sha40,
};
use serde_json::{json, Value};
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

fn odt(
    year: i32,
    month: Month,
    day: u8,
    hour: u8,
    min: u8,
    sec: u8,
    millisecond: u16,
) -> OffsetDateTime {
    let time = Time::from_hms_milli(hour, min, sec, millisecond).expect("valid time");
    let date = time::Date::from_calendar_date(year, month, day).expect("valid date");
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn sha(digit: char) -> String {
    std::iter::repeat(digit).take(40).collect()
}

fn sample_from() -> Binding {
    let first = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let last = first + Duration::days(10);
    Binding::builder()
        .target_sha(sha('1'))
        .commit_sha(sha('2'))
        .tree_sha(sha('3'))
        .first_observed(first)
        .last_observed(last)
        .observation_count(4)
        .build()
        .expect("consistent from binding")
}

fn sample_to() -> Binding {
    Binding::builder()
        .target_sha(sha('4'))
        .commit_sha(sha('5'))
        .tree_sha(sha('6'))
        .first_observed(odt(2026, Month::January, 11, 12, 0, 0, 0))
        .observation_count(1)
        .build()
        .expect("to binding")
}

fn sample_move(classification: Classification) -> Entry {
    Entry::builder()
        .seq(1)
        .prev_hash(HashRef::GENESIS)
        .recorded_at(odt(2026, Month::January, 11, 12, 0, 1, 0))
        .event(Event::Move)
        .classification(classification)
        .severity(Severity::High)
        .repo("acme/widgets")
        .ref_name("refs/tags/v1")
        .ref_type_before(RefType::Annotated)
        .ref_type_after(RefType::Annotated)
        .from(sample_from())
        .to(sample_to())
        .observation_window_seconds(3600)
        .source_observations(vec![
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
        ])
        .diff(Diff {
            files_added: 1,
            files_removed: 0,
            files_modified: 2,
            files_renamed: 0,
            paths: vec!["src/main.rs".into()],
            diff_possibly_truncated: false,
        })
        .build()
        .expect("move entry")
}

fn object_keys(value: &Value) -> Vec<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

#[test]
fn move_content_change_serialises_exact_key_set_no_extras() {
    let entry = sample_move(Classification::ContentChange);
    let value = serde_json::to_value(&entry).expect("serialise");

    let mut keys = object_keys(&value);
    keys.sort();
    assert_eq!(
        keys,
        [
            "classification",
            "diff",
            "event",
            "format_version",
            "from",
            "observation_window_seconds",
            "prev_hash",
            "recorded_at",
            "ref",
            "ref_type_after",
            "ref_type_before",
            "repo",
            "seq",
            "severity",
            "source_observations",
            "to",
        ]
    );
    assert_eq!(value.get("event").and_then(|v| v.as_str()), Some("move"));
    assert_eq!(
        value.get("classification").and_then(|v| v.as_str()),
        Some("content_change")
    );

    let mut from_keys = object_keys(value.get("from").expect("from"));
    from_keys.sort();
    assert_eq!(
        from_keys,
        [
            "commit_sha",
            "first_observed",
            "last_observed",
            "observation_count",
            "stable_days",
            "target_sha",
            "tree_sha",
        ]
    );

    let mut to_keys = object_keys(value.get("to").expect("to"));
    to_keys.sort();
    assert_eq!(
        to_keys,
        [
            "commit_sha",
            "first_observed",
            "observation_count",
            "stable_days",
            "target_sha",
            "tree_sha",
        ]
    );
    assert!(
        !to_keys.iter().any(|k| k == "last_observed"),
        "to block must not include last_observed at creation"
    );
}

#[test]
fn from_block_builder_requires_observation_count() {
    let first = odt(2026, Month::March, 1, 0, 0, 0, 0);
    let last = first + Duration::days(3);
    let from = Binding::builder()
        .target_sha(sha('1'))
        .commit_sha(sha('2'))
        .tree_sha(sha('3'))
        .first_observed(first)
        .last_observed(last)
        .observation_count(2)
        .build()
        .expect("complete builder");
    assert_eq!(from.observation_count(), 2);
}

#[test]
fn ui_from_block_without_observation_count_fails_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/entry_ui/from_missing_observation_count.rs");
}

#[test]
fn stable_days_is_derived_inconsistent_pair_rejected() {
    let first = odt(2026, Month::April, 1, 0, 0, 0, 0);
    let last = first + Duration::days(5);
    let from = Binding::builder()
        .target_sha(sha('1'))
        .commit_sha(sha('2'))
        .tree_sha(sha('3'))
        .first_observed(first)
        .last_observed(last)
        .observation_count(1)
        .build()
        .expect("derived stable_days");
    assert_eq!(
        from.stable_days(),
        5,
        "stable_days must come from the observation span"
    );

    let mut wire = serde_json::to_value(&from).expect("serialise");
    wire.as_object_mut()
        .expect("object")
        .insert("stable_days".into(), json!(999));
    let err = serde_json::from_value::<Binding>(wire).expect_err("inconsistent");
    let msg = err.to_string();
    assert!(
        msg.contains("stable_days") || msg.contains("inconsistent"),
        "rejection must cite the inconsistency, got {msg}"
    );
}

#[test]
fn to_block_has_first_observed_not_last_observed() {
    let to = sample_to();
    let value = serde_json::to_value(&to).expect("serialise");
    let keys = object_keys(&value);
    assert!(keys.iter().any(|k| k == "first_observed"));
    assert!(
        !keys.iter().any(|k| k == "last_observed"),
        "to must not carry last_observed at creation; got {keys:?}"
    );
}

#[test]
fn observation_window_seconds_required_and_must_be_positive() {
    let err = Entry::builder()
        .seq(1)
        .prev_hash(HashRef::GENESIS)
        .recorded_at(odt(2026, Month::January, 11, 12, 0, 1, 0))
        .event(Event::Move)
        .classification(Classification::ContentChange)
        .severity(Severity::High)
        .repo("acme/widgets")
        .ref_name("refs/tags/v1")
        .ref_type_before(RefType::Annotated)
        .ref_type_after(RefType::Annotated)
        .from(sample_from())
        .to(sample_to())
        .source_observations(vec![
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
        ])
        .observation_window_seconds(0)
        .build()
        .expect_err("zero window rejected");
    assert!(
        matches!(err, EntryError::NonPositiveObservationWindow),
        "got {err:?}"
    );

    let ok = sample_move(Classification::CommitMetadataOnly);
    assert_eq!(ok.observation_window_seconds(), Some(3600));
}

#[test]
fn correction_requires_corrects_seq_and_reason_forbids_diff() {
    let entry = Entry::builder()
        .seq(2)
        .prev_hash(HashRef::GENESIS)
        .recorded_at(odt(2026, Month::May, 1, 0, 0, 0, 0))
        .event(Event::Correction)
        .corrects_seq(1)
        .reason("misclassified tree-unchanged move as content-change")
        .build()
        .expect("correction");

    let value = serde_json::to_value(&entry).expect("serialise");
    assert_eq!(
        value.get("event").and_then(|v| v.as_str()),
        Some("correction")
    );
    assert_eq!(value.get("corrects_seq").and_then(|v| v.as_u64()), Some(1));
    assert!(value.get("reason").and_then(|v| v.as_str()).is_some());
    assert!(
        value.get("diff").is_none(),
        "correction must forbid a diff block"
    );

    let with_diff = json!({
        "format_version": 1,
        "seq": 2,
        "prev_hash": HashRef::GENESIS,
        "recorded_at": "2026-05-01T00:00:00.000Z",
        "event": "correction",
        "corrects_seq": 1,
        "reason": "x",
        "diff": {
            "files_added": 1,
            "files_removed": 0,
            "files_modified": 0,
            "files_renamed": 0,
            "paths": [],
            "diff_possibly_truncated": false
        }
    });
    let err = serde_json::from_value::<Entry>(with_diff).expect_err("diff forbidden");
    let msg = err.to_string();
    assert!(msg.contains("diff") || msg.contains("forbid"), "got {msg}");
}

#[test]
fn round_trip_every_event_type_and_classification() {
    let recorded = odt(2026, Month::June, 1, 0, 0, 0, 0);
    let cases = vec![
        sample_move(Classification::ContentChange),
        sample_move(Classification::CommitMetadataOnly),
        sample_move(Classification::ReleaseLevelOnly),
        Entry::builder()
            .seq(10)
            .prev_hash(HashRef::GENESIS)
            .recorded_at(recorded)
            .event(Event::Deletion)
            .classification(Classification::ContentChange)
            .severity(Severity::Medium)
            .repo("acme/widgets")
            .ref_name("refs/tags/v1")
            .ref_type_before(RefType::Lightweight)
            .ref_type_after(RefType::Lightweight)
            .from(sample_from())
            .to(sample_to())
            .source_observations(vec![
                "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
            ])
            .observation_window_seconds(60)
            .build()
            .expect("deletion"),
        Entry::builder()
            .seq(11)
            .prev_hash(HashRef::GENESIS)
            .recorded_at(recorded)
            .event(Event::Recreation)
            .classification(Classification::ReleaseLevelOnly)
            .severity(Severity::Low)
            .repo("acme/widgets")
            .ref_name("refs/tags/v2")
            .ref_type_before(RefType::Annotated)
            .ref_type_after(RefType::Annotated)
            .from(sample_from())
            .to(sample_to())
            .gap_seconds(86400)
            .source_observations(vec![
                "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
            ])
            .observation_window_seconds(120)
            .build()
            .expect("recreation"),
        Entry::builder()
            .seq(12)
            .prev_hash(HashRef::GENESIS)
            .recorded_at(recorded)
            .event(Event::Correction)
            .corrects_seq(10)
            .reason("fixture")
            .build()
            .expect("correction"),
        Entry::builder()
            .seq(13)
            .prev_hash(HashRef::GENESIS)
            .recorded_at(recorded)
            .event(Event::PopulationChange)
            .repo("aquasecurity/setup-trivy")
            .population_change(PopulationChange {
                path: None,
                change: PopulationChangeKind::Added,
                reason: PopulationReason::Transitive {
                    via: "aquasecurity/trivy-action@0123456789abcdef0123456789abcdef01234567"
                        .into(),
                },
                note: None,
            })
            .build()
            .expect("population_change"),
    ];

    for entry in cases {
        let bytes = serde_json::to_vec(&entry).expect("serialise");
        let back: Entry = serde_json::from_slice(&bytes).expect("deserialise");
        assert_eq!(back, entry);
    }
}

#[test]
fn unknown_field_in_incoming_json_is_rejected() {
    let entry = sample_move(Classification::ContentChange);
    let mut value = serde_json::to_value(&entry).expect("serialise");
    value
        .as_object_mut()
        .expect("object")
        .insert("surprise_future_field".into(), json!("nope"));

    let err = serde_json::from_value::<Entry>(value).expect_err("deny_unknown_fields");
    let msg = err.to_string();
    assert!(
        msg.contains("surprise_future_field") || msg.contains("unknown"),
        "unknown fields must be rejected so verifiers cannot silently drop them; got {msg}"
    );
}

#[test]
fn sha40_rejects_short_and_uppercase() {
    assert!(matches!(Sha40::parse("abc"), Err(EntryError::InvalidSha40)));
    assert!(matches!(
        Sha40::parse("A".repeat(40)),
        Err(EntryError::InvalidSha40)
    ));
    assert!(Sha40::parse(sha('a')).is_ok());
}

#[test]
fn action_yml_sha_round_trips_on_binding() {
    let first = odt(2026, Month::July, 1, 0, 0, 0, 0);
    let binding = Binding::builder()
        .target_sha(sha('a'))
        .commit_sha(sha('b'))
        .tree_sha(sha('c'))
        .first_observed(first)
        .last_observed(first + Duration::days(1))
        .action_yml_sha(sha('d'))
        .observation_count(2)
        .build()
        .expect("binding");
    let value = serde_json::to_value(&binding).expect("serialise");
    assert_eq!(
        value.get("action_yml_sha").and_then(|v| v.as_str()),
        Some(sha('d').as_str())
    );
    let back: Binding = serde_json::from_value(value).expect("deserialise");
    assert_eq!(back, binding);
}

#[test]
fn unhashed_view_omits_entry_hash() {
    let mut entry = sample_move(Classification::ContentChange);
    entry.entry_hash = Some(HashRef::parse(format!("sha256:{}", "ab".repeat(32))).unwrap());
    let with_hash = serde_json::to_value(&entry).expect("full");
    assert!(with_hash.get("entry_hash").is_some());
    let unhashed = serde_json::to_value(entry.unhashed()).expect("unhashed");
    assert!(
        unhashed.get("entry_hash").is_none(),
        "Unhashed must omit entry_hash"
    );
}

#[test]
fn correlation_rejects_member_seq_ge_own_seq() {
    let bad = json!({
        "format_version": 1,
        "seq": 5,
        "prev_hash": HashRef::GENESIS,
        "recorded_at": "2026-05-01T00:00:00.000Z",
        "event": "correlation",
        "correlation": {
            "batch_id": "b",
            "member_seqs": [1, 5],
            "refs_moved_together": ["refs/tags/v1.0.0"],
            "all_to_same_target": true
        }
    });
    let err = serde_json::from_value::<Entry>(bad).expect_err("member_seq >= seq");
    let msg = err.to_string();
    assert!(
        msg.contains("member_seq") || msg.contains("Correlation"),
        "got {msg}"
    );
}
