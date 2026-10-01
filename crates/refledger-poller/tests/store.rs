//! Store: single writer, durable day files, seal-at-the-boundary, signed heads.
//!
//! Day D's ObservationDigest is the first entry of day D+1, stamped
//! D+1 00:00:00.000Z. Replay must not consult the wall clock.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use base64::Engine;
use refledger_log::chain::{verify, UnhashedEntry};
use refledger_log::entry::{Binding, Classification, Event, RefType, Severity};
use refledger_log::key_id;
use refledger_log::SigningKey;
use refledger_poller::classify::{classify, Ancestry, Enrichment, RepoState};
use refledger_poller::derive::{derive, ChainTip};
use refledger_poller::enrich::CompareCache;
use refledger_poller::observation::{ETag, Method, Observation, ObservedRef, Outcome};
use refledger_poller::store::{
    Appended, FailingRekor, FaultVolume, RekorAcceptance, RekorClient, StaticRekor, Store,
    StoreError, StoreOptions,
};
use serde_json::Value;
use tempfile::TempDir;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

fn odt(
    year: i32,
    month: Month,
    day: u8,
    hour: u8,
    min: u8,
    sec: u8,
    millisecond: u16,
) -> OffsetDateTime {
    let time = Time::from_hms_milli(hour, min, sec, millisecond).unwrap();
    let date = Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn day(year: i32, month: Month, d: u8) -> Date {
    Date::from_calendar_date(year, month, d).unwrap()
}

fn key() -> SigningKey {
    SigningKey::from_seed_bytes(&[9u8; 32]).unwrap()
}

fn opts(rekor: Box<dyn RekorClient>) -> StoreOptions {
    StoreOptions {
        log_id: "refledger-test".into(),
        signing_key: key(),
        recovered_at: odt(2026, Month::January, 2, 0, 0, 0, 0),
        rekor,
        archive: Box::new(refledger_poller::NoopArchive),
        publisher: Box::new(refledger_poller::NoopPublisher),
        observations_on_data_branch: false,
        skip_lock: false,
    }
}

fn static_rekor() -> Box<dyn RekorClient> {
    Box::new(StaticRekor { log_index: 7 })
}

fn sha(digit: char) -> String {
    std::iter::repeat(digit).take(40).collect()
}

fn correction(at: OffsetDateTime, seq: u64) -> UnhashedEntry {
    UnhashedEntry::correction(at, seq, "test")
}

fn binding(digit: char, at: OffsetDateTime, with_last: bool) -> Binding {
    let mut b = Binding::builder()
        .target_sha(sha(digit))
        .commit_sha(sha(digit))
        .tree_sha(sha(digit))
        .first_observed(at)
        .observation_count(1);
    if with_last {
        b = b.last_observed(at);
    }
    b.build().unwrap()
}

fn move_at(at: OffsetDateTime) -> UnhashedEntry {
    UnhashedEntry::move_event(refledger_log::MoveDraft {
        recorded_at: at,
        classification: Classification::ContentChange,
        severity: Severity::High,
        repo: "acme/widgets".into(),
        ref_name: "refs/tags/v1".into(),
        ref_type_before: RefType::Lightweight,
        ref_type_after: RefType::Lightweight,
        from: binding('a', at, true),
        to: binding('b', at, false),
        observation_window_seconds: 3600,
        source_observations: vec![
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
        ],
        diff: None,
    })
}

fn obs_at(at: OffsetDateTime) -> Observation {
    Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(at)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::NotModified {
            http_status: 304,
            etag: ETag::new("W/\"e\""),
        })
        .build()
        .unwrap()
}

struct CaptureRekor {
    got: Arc<Mutex<Option<Value>>>,
}

impl RekorClient for CaptureRekor {
    fn submit(&self, proposed: &Value) -> Result<RekorAcceptance, String> {
        *self.got.lock().unwrap() = Some(proposed.clone());
        Ok(RekorAcceptance {
            log_index: 7,
            uuid: "test-uuid".into(),
            log_id: Some("rekor-log".into()),
            integrated_time: Some(1_700_000_000),
        })
    }
}

#[test]
fn second_open_is_already_locked() {
    let dir = TempDir::new().unwrap();
    let first = Store::open(dir.path(), opts(static_rekor())).unwrap();
    match Store::open(dir.path(), opts(static_rekor())) {
        Err(StoreError::AlreadyLocked) => {}
        Err(err) => panic!("expected AlreadyLocked, got {err}"),
        Ok(_) => panic!("second open must fail"),
    }
    drop(first);
    Store::open(dir.path(), opts(static_rekor())).expect("lock released on drop");
}

#[test]
fn new_day_file_fsyncs_file_and_directory_or_the_append_is_not_committed() {
    let mut store = Store::open_with(FaultVolume::new(), opts(static_rekor())).unwrap();
    store.fault_mut().fail_next_dir_fsync();
    let err = store
        .append_entry(correction(odt(2026, Month::January, 1, 12, 0, 0, 0), 0))
        .unwrap_err();
    assert!(matches!(err, StoreError::Durability(_)));
    assert!(store.entries().is_empty());

    store
        .append_entry(correction(odt(2026, Month::January, 1, 12, 0, 0, 0), 0))
        .unwrap();
    let after_create = (store.fault_mut().file_fsyncs, store.fault_mut().dir_fsyncs);
    assert_eq!(
        after_create.1, 1,
        "creating the day file fsyncs its directory"
    );
    assert!(after_create.0 >= 1, "the file itself is fsynced");

    store
        .append_entry(correction(odt(2026, Month::January, 1, 13, 0, 0, 0), 0))
        .unwrap();
    assert_eq!(
        store.fault_mut().dir_fsyncs,
        1,
        "an existing day file does not fsync the directory again"
    );
    assert!(store.fault_mut().file_fsyncs >= 2);
}

#[test]
fn kill_mid_write_reopen_verifies_and_keeps_torn_bytes() {
    let mut store = Store::open_with(FaultVolume::new(), opts(static_rekor())).unwrap();
    store
        .append_entry(correction(odt(2026, Month::January, 1, 12, 0, 0, 0), 0))
        .unwrap();
    store.fault_mut().kill_next_append();
    let err = store
        .append_entry(correction(odt(2026, Month::January, 1, 13, 0, 0, 0), 0))
        .unwrap_err();
    assert!(matches!(err, StoreError::SimulatedCrash));

    let vol = store.into_volume();
    let store = Store::open_with(vol, opts(static_rekor())).unwrap();
    verify(store.entries()).unwrap();
    assert_eq!(store.entries().len(), 1);
    let log = store
        .day_log(day(2026, Month::January, 1))
        .unwrap()
        .unwrap();
    assert!(log.ends_with(b"\n"));
    let torn = store.torn_files().unwrap();
    assert_eq!(torn.len(), 1);
    assert!(!torn[0].1.is_empty());
    assert!(
        !log.ends_with(&torn[0].1),
        "torn bytes must leave the live file"
    );

    let jan1 = day(2026, Month::January, 1);
    store_seal(store, jan1);
}

fn store_seal(mut store: Store<FaultVolume>, jan1: Date) {
    store.stop_dispatch(jan1);
    let entry = store.seal_day(jan1).unwrap();
    let note = entry.observation_digest.unwrap().note.unwrap();
    assert!(note.contains("torn write preserved"), "{note}");
    assert!(note.contains(&store.torn_files().unwrap()[0].0));
}

#[test]
fn real_filesystem_torn_tail_is_preserved_and_chain_verifies() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    store
        .append_entry(correction(odt(2026, Month::January, 1, 12, 0, 0, 0), 0))
        .unwrap();
    drop(store);

    let path = dir.path().join("log/2026/01/01.jsonl");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"NOT-A-COMPLETE-LINE").unwrap();
    drop(file);

    let store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    verify(store.entries()).unwrap();
    assert_eq!(store.entries().len(), 1);
    let torn = store.torn_files().unwrap();
    assert_eq!(torn.len(), 1);
    assert_eq!(torn[0].1, b"NOT-A-COMPLETE-LINE");
}

#[test]
fn corrupt_complete_line_refuses_to_start() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    store
        .append_entry(correction(odt(2026, Month::January, 1, 12, 0, 0, 0), 0))
        .unwrap();
    drop(store);

    let path = dir.path().join("log/2026/01/01.jsonl");
    let bytes = fs::read(&path).unwrap();
    let mut doubled = bytes.clone();
    doubled.extend_from_slice(&bytes);
    fs::write(&path, doubled).unwrap();

    match Store::open(dir.path(), opts(static_rekor())) {
        Err(StoreError::Corrupt(_)) => {}
        Err(err) => panic!("expected corrupt, got {err}"),
        Ok(_) => panic!("corrupt tail must refuse to start"),
    }
}

#[test]
fn request_that_completes_after_midnight_lands_on_the_next_day() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    let start = odt(2026, Month::January, 1, 23, 59, 59, 900);
    let end = odt(2026, Month::January, 2, 0, 0, 0, 300);
    let ticket = store.begin_request(start).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    assert!(!store.dispatch_open(jan1));
    assert!(store.begin_request(start).is_err());

    store.complete_observation(ticket, &obs_at(end)).unwrap();
    assert!(store
        .read_rel("observations/2026/01/02/acme--widgets.jsonl")
        .unwrap()
        .is_some());
    assert!(store
        .read_rel("observations/2026/01/01/acme--widgets.jsonl")
        .unwrap()
        .is_none());

    let digest = store.seal_day(jan1).unwrap();
    let body = digest.observation_digest.unwrap();
    assert_eq!(body.date, "2026-01-01");
    assert_eq!(body.repos_polled, 0);
    assert!(body.files.is_empty());
    assert_eq!(
        digest.recorded_at.as_offset_datetime(),
        odt(2026, Month::January, 2, 0, 0, 0, 0)
    );
}

#[test]
fn move_detected_before_the_seal_is_appended_after_the_digest() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store
        .append_observation(&obs_at(odt(2026, Month::January, 1, 12, 0, 0, 0)))
        .unwrap();
    store.stop_dispatch(jan1);

    let detected = odt(2026, Month::January, 2, 0, 0, 2, 0);
    let appended = store.append_entry(move_at(detected)).unwrap();
    assert!(matches!(appended, Appended::Buffered));

    let digest = store.seal_day(jan1).unwrap();
    let entries = store.entries();
    let mv = entries.last().unwrap();
    assert_eq!(mv.event, Event::Move);
    assert!(mv.seq > digest.seq);
    assert!(mv.recorded_at.as_offset_datetime() > digest.recorded_at.as_offset_datetime());
    assert_eq!(
        digest.recorded_at.as_offset_datetime(),
        odt(2026, Month::January, 2, 0, 0, 0, 0)
    );

    let file = store
        .day_log(day(2026, Month::January, 2))
        .unwrap()
        .unwrap();
    let first = std::str::from_utf8(&file).unwrap().lines().next().unwrap();
    let value: Value = serde_json::from_str(first).unwrap();
    assert_eq!(value["event"], "observation_digest");
    assert_eq!(value["seq"], digest.seq);
}

#[test]
fn a_quiet_day_still_gets_a_zero_digest() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    let digest = store.seal_day(jan1).unwrap();
    let body = digest.observation_digest.unwrap();
    assert_eq!(body.repos_polled, 0);
    assert_eq!(body.ok, 0);
    assert_eq!(body.not_modified, 0);
    assert_eq!(body.failed, 0);
    assert_eq!(body.skipped, 0);
    assert!(body.files.is_empty());
    assert!(body.note.is_none());
    let file = store
        .day_log(day(2026, Month::January, 2))
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(&file)
        .unwrap()
        .contains("observation_digest"));
}

#[test]
fn head_commits_to_the_digest_and_verify_accepts_every_day() {
    let got = Arc::new(Mutex::new(None));
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(
        dir.path(),
        opts(Box::new(CaptureRekor { got: got.clone() })),
    )
    .unwrap();
    let jan1 = day(2026, Month::January, 1);
    let jan2 = day(2026, Month::January, 2);
    store
        .append_observation(&obs_at(odt(2026, Month::January, 1, 8, 0, 0, 0)))
        .unwrap();
    store.stop_dispatch(jan1);
    let digest = store.seal_day(jan1).unwrap();
    // A second day so --head must accept a head that is no longer the tip.
    store.stop_dispatch(jan2);
    let second = store.seal_day(jan2).unwrap();
    assert!(second.seq > digest.seq);

    let heads = store.read_rel("log/heads.jsonl").unwrap().unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&heads)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0]["head"]["entry_hash"],
        digest.entry_hash.unwrap().to_string()
    );
    assert_eq!(lines[0]["head"]["seq"], digest.seq);
    assert_eq!(
        lines[1]["head"]["entry_hash"],
        second.entry_hash.unwrap().to_string()
    );
    let pk = key().verifying_key_bytes();
    assert_eq!(lines[0]["key_id"], key_id(&pk));
    assert_eq!(lines[1]["key_id"], key_id(&pk));
    assert_eq!(lines[0]["rekor"]["log_index"], 7);
    assert_eq!(lines[1]["rekor"]["log_index"], 7);

    let proposed = got.lock().unwrap().clone().unwrap();
    assert_eq!(proposed["kind"], "hashedrekord");
    assert_eq!(proposed["apiVersion"], "0.0.1");
    assert_eq!(proposed["spec"]["data"]["hash"]["algorithm"], "sha512");
    let sig = base64::engine::general_purpose::STANDARD
        .decode(proposed["spec"]["signature"]["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(sig.len(), 64);
    let pem = base64::engine::general_purpose::STANDARD
        .decode(
            proposed["spec"]["signature"]["publicKey"]["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    let pem = String::from_utf8(pem).unwrap();
    assert!(pem.contains("BEGIN PUBLIC KEY"));

    let log_dir = dir.path().join("log");
    let head = log_dir.join("heads.jsonl");
    let output = Command::new(verify_bin())
        .arg(&log_dir)
        .arg("--head")
        .arg(&head)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("head: signed, valid"), "{text}");
}

#[test]
fn archive_upload_failure_appears_in_the_next_digest_note() {
    use refledger_poller::{DayArchive, ObservationArchive};

    struct Boom;
    impl ObservationArchive for Boom {
        fn upload_day(&self, archive: &DayArchive) -> Result<(), String> {
            Err(format!("mirror unavailable for {}", archive.day))
        }
    }

    let dir = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.archive = Box::new(Boom);
    let mut store = Store::open(dir.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    let first = store.seal_day(jan1).unwrap();
    assert!(
        first
            .observation_digest
            .as_ref()
            .and_then(|d| d.note.as_ref())
            .is_none(),
        "failure of day D must not land in D's own digest"
    );

    let jan2 = day(2026, Month::January, 2);
    store.stop_dispatch(jan2);
    let second = store.seal_day(jan2).unwrap();
    let note = second.observation_digest.unwrap().note.unwrap();
    assert!(
        note.contains("observation archive upload failed for 2026-01-01"),
        "expected next-digest archive note, got {note}"
    );
}

#[test]
fn witness_backlog_over_48h_lands_in_digest_note_and_fails_strict() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(
        dir.path(),
        opts(Box::new(FailingRekor {
            message: "rekor down".into(),
        })),
    )
    .unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();
    // Advance "as_of" by sealing a day 3 days later without fixing the witness.
    let jan3 = day(2026, Month::January, 3);
    store.stop_dispatch(day(2026, Month::January, 2));
    store.seal_day(day(2026, Month::January, 2)).unwrap();
    store.stop_dispatch(jan3);
    let digest = store.seal_day(jan3).unwrap();
    let note = digest.observation_digest.unwrap().note.unwrap();
    assert!(
        note.contains("witness backlog"),
        "expected backlog note, got {note}"
    );

    let log_dir = dir.path().join("log");
    let output = Command::new(verify_bin())
        .arg(&log_dir)
        .arg("--strict")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "strict must fail on backlog; stdout=\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("witness backlog") || text.contains("FAIL"),
        "{text}"
    );
}

#[test]
fn failed_rekor_submission_is_recorded_and_retried() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(
        dir.path(),
        opts(Box::new(FailingRekor {
            message: "rekor down".into(),
        })),
    )
    .unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store
        .seal_day(jan1)
        .expect("witness failure must not fail the seal");
    let first = heads(&store);
    assert!(first[0]["rekor"].get("log_index").is_none());
    assert_eq!(first[0]["rekor"]["error"], "rekor down");

    store.set_rekor(Box::new(StaticRekor { log_index: 9 }));
    assert_eq!(store.retry_witnesses().unwrap(), 1);
    let lines = heads(&store);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["rekor"]["log_index"], 9);
    assert_eq!(lines[1]["rekor"]["attempts"], 2);
    assert_eq!(
        lines[0]["head"]["entry_hash"],
        lines[1]["head"]["entry_hash"]
    );
}

fn heads(store: &Store<refledger_poller::store::OsVolume>) -> Vec<Value> {
    let bytes = store.read_rel("log/heads.jsonl").unwrap().unwrap();
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn replay_including_digests_is_byte_for_byte_without_a_wall_clock() {
    let archive = sample_archive();
    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().join("compare.jsonl");
    seed_cache(&cache_path);

    let once = replay(&archive, &cache_path);
    let twice = replay(&archive, &cache_path);
    assert_eq!(once, twice, "replay must regenerate the log BYTE FOR BYTE");
    let text = String::from_utf8(once).unwrap();
    assert!(text.contains("\"event\":\"observation_digest\""));
    assert!(text.contains("\"date\":\"2026-01-01\""));
    assert!(text.contains("\"date\":\"2026-01-02\""));
    assert!(text.contains("2026-01-02T00:00:00.000Z"));
    assert!(text.contains("2026-01-03T00:00:00.000Z"));
}

#[test]
fn startup_downtime_records_poller_down_gap_per_group_in_digest() {
    use refledger_poller::observation::SkipReason;
    use refledger_poller::population::PollGroup;
    use refledger_poller::scheduler::M1_INTERVAL;
    use time::Duration;

    let dir = TempDir::new().unwrap();
    let last_poll = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let restart = last_poll + Duration::minutes(20);

    let groups = vec![
        PollGroup {
            repo: "acme/widgets".into(),
            paths: vec![None],
        },
        PollGroup {
            repo: "acme/gadgets".into(),
            paths: vec![None],
        },
    ];

    {
        let mut store = Store::open(
            dir.path(),
            StoreOptions {
                recovered_at: last_poll,
                ..opts(static_rekor())
            },
        )
        .unwrap();
        store
            .append_observation(&ok_obs(last_poll, vec![lw("v1", &sha('a'), &sha('a'))]))
            .unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/gadgets")
                    .unwrap()
                    .observed_at(last_poll)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"g\"")),
                        refs: vec![],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    let mut store = Store::open(
        dir.path(),
        StoreOptions {
            recovered_at: restart,
            ..opts(static_rekor())
        },
    )
    .unwrap();
    let gaps = store
        .record_startup_downtime(&groups, M1_INTERVAL, restart)
        .unwrap();
    assert_eq!(gaps.len(), 2, "one PollerDown per group");
    for obs in &gaps {
        match obs.outcome() {
            Outcome::Skipped {
                reason: SkipReason::PollerDown { from, to },
            } => {
                assert_eq!(from.as_offset_datetime(), last_poll);
                assert_eq!(to.as_offset_datetime(), restart);
            }
            other => panic!("expected PollerDown, got {other:?}"),
        }
    }

    // No gap when restart is within 2× interval.
    let near = store
        .record_startup_downtime(&groups, M1_INTERVAL, restart + Duration::seconds(30))
        .unwrap();
    assert!(near.is_empty(), "sub-threshold gap must not double-record");

    store
        .append_entry(UnhashedEntry::correction(
            last_poll,
            0,
            "genesis placeholder — log opened",
        ))
        .unwrap();
    let digest = store.seal_day(day(2026, Month::January, 1)).unwrap();
    let stats = digest.observation_digest.as_ref().expect("digest");
    assert!(
        stats.skipped >= 2,
        "digest must count PollerDown gaps, skipped={}",
        stats.skipped
    );

    // Wire form visible in observation files.
    let widgets = fs::read_to_string(
        dir.path()
            .join("observations/2026/01/01/acme--widgets.jsonl"),
    )
    .unwrap();
    assert!(
        widgets.contains("poller_down"),
        "gap must be on the wire: {widgets}"
    );
}

#[test]
fn post_genesis_identity_warning_lands_in_next_digest_note() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    store
        .append_entry(correction(odt(2026, Month::January, 1, 0, 0, 0, 0), 0))
        .unwrap();
    store
        .record_identity_warning("contact URL unreachable: https://example.test: HTTP 503")
        .unwrap();
    assert!(
        dir.path().join("state/identity_warnings.jsonl").is_file(),
        "warnings live under state/, not log/"
    );
    assert!(
        !dir.path().join("log/identity_warnings.jsonl").exists(),
        "log/ must not hold warning sidecars"
    );
    store
        .append_observation(&obs_at(odt(2026, Month::January, 1, 12, 0, 0, 0)))
        .unwrap();
    let digest = store.seal_day(day(2026, Month::January, 1)).unwrap();
    let note = digest
        .observation_digest
        .as_ref()
        .and_then(|d| d.note.as_ref())
        .expect("warning must appear in digest note");
    assert!(note.contains("contact URL unreachable"), "note={note}");
}

#[test]
fn legacy_log_identity_warnings_migrate_into_digest_note() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("log")).unwrap();
    // Simulate pre-fix data branch: warning still under log/.
    fs::write(
        dir.path().join("log/identity_warnings.jsonl"),
        br#"{"warning":"observation 01M3Q896RH64XBABMKK8AXKNJ1 (tj-actions/changed-files at 2026-09-29T18:53:44.590Z): recorded http_status 422 was fabricated by a poller bug (budget exhaustion misreported as network/422); fixed in e30f6c7"}
"#,
    )
    .unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    assert!(
        !dir.path().join("log/identity_warnings.jsonl").exists(),
        "legacy sidecar must be removed from log/"
    );
    assert!(
        dir.path().join("state/identity_warnings.jsonl").is_file(),
        "legacy sidecar must land under state/"
    );
    store
        .append_entry(correction(odt(2026, Month::January, 1, 0, 0, 0, 0), 0))
        .unwrap();
    store
        .append_observation(&obs_at(odt(2026, Month::January, 1, 12, 0, 0, 0)))
        .unwrap();
    let digest = store.seal_day(day(2026, Month::January, 1)).unwrap();
    let note = digest
        .observation_digest
        .as_ref()
        .and_then(|d| d.note.as_ref())
        .expect("migrated warning must appear in digest note");
    assert!(
        note.contains("fabricated by a poller bug"),
        "false-422 note must survive migration: {note}"
    );
}

/// Build a bare remote + publishing clone under `root` for FF-only publish tests.
/// When `seed_data_log` is false, the clone has no `data/` yet (fresh main).
fn setup_publish_clone_opts(
    root: &std::path::Path,
    seed_data_log: bool,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let bare = root.join("remote.git");
    let clone = root.join("publish-clone");
    assert!(Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&bare)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["clone", bare.to_str().unwrap(), clone.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    if seed_data_log {
        let data_log = clone.join("data/log");
        fs::create_dir_all(&data_log).unwrap();
        fs::write(data_log.join(".gitkeep"), b"").unwrap();
        assert!(Command::new("git")
            .args(["-C", clone.to_str().unwrap(), "add", "data/log/.gitkeep"])
            .status()
            .unwrap()
            .success());
    } else {
        fs::write(clone.join("README.md"), b"refledger\n").unwrap();
        assert!(Command::new("git")
            .args(["-C", clone.to_str().unwrap(), "add", "README.md"])
            .status()
            .unwrap()
            .success());
    }
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "commit",
            "-m",
            "seed",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "push",
            "-u",
            "origin",
            "main"
        ])
        .status()
        .unwrap()
        .success());
    (bare, clone)
}

fn setup_publish_clone(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    setup_publish_clone_opts(root, true)
}

#[test]
fn seal_publishes_data_log_to_dedicated_clone() {
    use refledger_poller::GitLedgerPublisher;

    let root = TempDir::new().unwrap();
    let (_bare, clone) = setup_publish_clone(root.path());
    let data = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    let entry = store.seal_day(jan1).unwrap();

    let log = Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "log", "-1", "--pretty=%s"])
        .output()
        .unwrap();
    let subject = String::from_utf8_lossy(&log.stdout);
    assert_eq!(
        subject.trim(),
        format!("ledger: seal 2026-01-01 seq {}", entry.seq)
    );
    let files = Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ])
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&files.stdout);
    assert!(tree.contains("data/log/heads.jsonl"), "{tree}");
    assert!(
        tree.contains("data/log/2026/01/02.jsonl"),
        "digest lands on D+1: {tree}"
    );
    assert!(
        !tree.contains("identity_warnings"),
        "sidecars must never publish: {tree}"
    );
    // Live data dir is not the clone.
    assert_ne!(data.path(), clone.as_path());
}

#[test]
fn first_seal_publishes_when_main_has_no_data_dir_yet() {
    use refledger_poller::GitLedgerPublisher;

    // Fresh main (no data/): git status reports `?? data/` before add.
    let root = TempDir::new().unwrap();
    let (_bare, clone) = setup_publish_clone_opts(root.path(), false);
    let data = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    let entry = store.seal_day(jan1).unwrap();

    assert!(
        store
            .read_rel("state/publish_failures.jsonl")
            .unwrap()
            .map(|b| b.is_empty())
            .unwrap_or(true),
        "first seal onto empty main must not record a publish failure"
    );
    let log = Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "log", "-1", "--pretty=%s"])
        .output()
        .unwrap();
    let subject = String::from_utf8_lossy(&log.stdout);
    assert_eq!(
        subject.trim(),
        format!("ledger: seal 2026-01-01 seq {}", entry.seq)
    );
}

#[test]
fn readme_verify_command_passes_on_published_layout_with_stray_sidecar() {
    use refledger_poller::GitLedgerPublisher;

    let root = TempDir::new().unwrap();
    let (_bare, clone) = setup_publish_clone(root.path());
    let data = TempDir::new().unwrap();
    // Leave a legacy warning under log/ so migration + publish filtering are both exercised.
    fs::create_dir_all(data.path().join("log")).unwrap();
    fs::write(
        data.path().join("log/identity_warnings.jsonl"),
        br#"{"warning":"observation 01M3Q896RH64XBABMKK8AXKNJ1: fabricated 422; fixed in e30f6c7"}
"#,
    )
    .unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    assert!(!data.path().join("log/identity_warnings.jsonl").exists());
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();

    // Reproduce main's published tree: data/log from the publish clone.
    let published = root.path().join("main-tree");
    fs::create_dir_all(published.join("data/log")).unwrap();
    for ent in fs::read_dir(clone.join("data/log")).unwrap() {
        let ent = ent.unwrap();
        let dest = published.join("data/log").join(ent.file_name());
        if ent.path().is_dir() {
            copy_dir(&ent.path(), &dest);
        } else {
            fs::copy(ent.path(), &dest).unwrap();
        }
    }
    // A skeptic's clone should still pass even if someone drops a sidecar next to the chain.
    fs::write(
        published.join("data/log/identity_warnings.jsonl"),
        b"{\"warning\":\"should be ignored\"}\n",
    )
    .unwrap();

    let pk = hex::encode(key().verifying_key_bytes());
    let output = Command::new(verify_bin())
        .current_dir(&published)
        .args(["data/log", "--strict", "--pubkey", &pk])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "README verify must pass on published layout\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ignored non-chain file: identity_warnings.jsonl"),
        "sidecar must warn, not fail: {stderr}"
    );
    assert!(stdout.contains("chain: OK"), "{stdout}");
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    fs::create_dir_all(dst).unwrap();
    for ent in fs::read_dir(src).unwrap() {
        let ent = ent.unwrap();
        let to = dst.join(ent.file_name());
        if ent.path().is_dir() {
            copy_dir(&ent.path(), &to);
        } else {
            fs::copy(ent.path(), to).unwrap();
        }
    }
}

#[test]
fn rejected_publish_push_is_recorded_and_retried_next_seal() {
    use refledger_poller::GitLedgerPublisher;

    let root = TempDir::new().unwrap();
    let (bare, clone) = setup_publish_clone(root.path());
    let data = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();

    // Divergent tip on the bare remote so the next FF push fails.
    let other = root.path().join("other");
    assert!(Command::new("git")
        .args(["clone", bare.to_str().unwrap(), other.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    fs::write(other.join("data/log/divergent.txt"), b"other tip").unwrap();
    assert!(Command::new("git")
        .args([
            "-C",
            other.to_str().unwrap(),
            "add",
            "data/log/divergent.txt"
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-C",
            other.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "commit",
            "-m",
            "divergent",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["-C", other.to_str().unwrap(), "push", "origin", "main"])
        .status()
        .unwrap()
        .success());

    let jan2 = day(2026, Month::January, 2);
    store.stop_dispatch(jan2);
    let failed = store.seal_day(jan2).unwrap();
    assert!(
        failed
            .observation_digest
            .as_ref()
            .and_then(|d| d.note.as_ref())
            .is_none(),
        "publish failure of D must not land in D's own digest"
    );
    let failures = store
        .read_rel("state/publish_failures.jsonl")
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&failures).contains("2026-01-02"),
        "pending failure must persist for retry"
    );

    // Heal the publish clone so the next poll can FF-push again (retry path).
    assert!(Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "fetch", "origin"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "reset",
            "--hard",
            "origin/main"
        ])
        .status()
        .unwrap()
        .success());

    // Mid-day retry (every poll), without waiting for the next seal.
    assert!(
        store.retry_pending_publishes().unwrap(),
        "healed clone must publish on retry"
    );
    assert!(
        store
            .read_rel("state/publish_failures.jsonl")
            .unwrap()
            .map(|b| b.is_empty())
            .unwrap_or(true),
        "pending failures cleared on success"
    );
    let log = Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "log", "-1", "--pretty=%s"])
        .output()
        .unwrap();
    let subject = String::from_utf8_lossy(&log.stdout);
    assert!(
        subject.trim().starts_with("ledger: seal 2026-01-02"),
        "retry must publish failed day: {subject}"
    );

    let jan3 = day(2026, Month::January, 3);
    store.stop_dispatch(jan3);
    let next = store.seal_day(jan3).unwrap();
    let note = next.observation_digest.unwrap().note.unwrap();
    assert!(
        note.contains("ledger publish succeeded for 2026-01-02")
            && note.contains("retry after earlier failure"),
        "expected publish recovery in next digest, got {note}"
    );
    assert!(
        !note.contains("ledger publish failed"),
        "recovered publish must not keep the failure note: {note}"
    );
}

#[test]
fn gitignore_blocking_data_log_is_recorded_then_retry_publishes() {
    use refledger_poller::GitLedgerPublisher;

    let root = TempDir::new().unwrap();
    let (_bare, clone) = setup_publish_clone(root.path());
    // Reproduce the production bug: top-level /data/ ignores data/log.
    fs::write(clone.join(".gitignore"), "/data/\n").unwrap();
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "add",
            ".gitignore",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "commit",
            "-m",
            "ignore data",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "push", "origin", "main"])
        .status()
        .unwrap()
        .success());

    let data = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();

    let failures = store
        .read_rel("state/publish_failures.jsonl")
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&failures);
    assert!(
        text.contains("ignored by one of your .gitignore files") || text.contains("git add"),
        "gitignore block must be recorded: {text}"
    );

    // Fix .gitignore the way main will after this change.
    fs::write(
        clone.join(".gitignore"),
        "/data/**\n!/data/log/\n!/data/log/**\n",
    )
    .unwrap();
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "add",
            ".gitignore",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.email=test@refledger.invalid",
            "-c",
            "user.name=refledger-test",
            "commit",
            "-m",
            "allow data/log",
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "push", "origin", "main"])
        .status()
        .unwrap()
        .success());

    assert!(store.retry_pending_publishes().unwrap());
    let log = Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "log", "-1", "--pretty=%s"])
        .output()
        .unwrap();
    let subject = String::from_utf8_lossy(&log.stdout);
    assert!(
        subject.trim().starts_with("ledger: seal 2026-01-01"),
        "retry after gitignore fix must publish: {subject}"
    );
}

#[test]
fn dirty_publish_clone_outside_data_log_refuses() {
    use refledger_poller::GitLedgerPublisher;

    let root = TempDir::new().unwrap();
    let (_bare, clone) = setup_publish_clone(root.path());
    fs::write(clone.join("README.md"), b"do not commit me\n").unwrap();

    let data = TempDir::new().unwrap();
    let mut options = opts(static_rekor());
    options.publisher = Box::new(GitLedgerPublisher::new(&clone, None));
    let mut store = Store::open(data.path(), options).unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();

    let failures = store
        .read_rel("state/publish_failures.jsonl")
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&failures);
    assert!(
        text.contains("outside data/log"),
        "dirty tree must refuse: {text}"
    );
}

fn replay(archive: &[Observation], cache_path: &std::path::Path) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    let cache = CompareCache::open(cache_path).unwrap();
    let mut state = RepoState::default();
    let first_at = archive[0].observed_at().as_offset_datetime();
    store
        .append_entry(UnhashedEntry::correction(
            first_at,
            0,
            "genesis placeholder — log opened",
        ))
        .unwrap();

    let mut cursor = first_at.date();
    for obs in archive {
        let obs_day = obs.observed_at().as_offset_datetime().date();
        while cursor < obs_day {
            store.stop_dispatch(cursor);
            store.seal_day(cursor).unwrap();
            cursor = cursor.next_day().unwrap();
        }
        store.append_observation(obs).unwrap();
        let enrich = enrichment_from_cache(&state, obs, &cache);
        let (next, events) = classify(&state, obs, &enrich).unwrap();
        let mut tip = ChainTip::from_entries(
            store.entries(),
            obs.repo().as_str(),
            obs.observed_at().as_offset_datetime(),
            BTreeMap::new(),
        );
        tip.diffs = diffs_from_cache(&cache);
        for entry in derive(&tip, &events).unwrap() {
            store.append_entry(entry).unwrap();
        }
        state = next;
    }
    let last = archive
        .last()
        .unwrap()
        .observed_at()
        .as_offset_datetime()
        .date();
    while cursor <= last {
        store.stop_dispatch(cursor);
        store.seal_day(cursor).unwrap();
        cursor = cursor.next_day().unwrap();
    }
    verify(store.entries()).unwrap();
    store.chain_bytes().unwrap()
}

fn sample_archive() -> Vec<Observation> {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t2 = odt(2026, Month::January, 3, 12, 15, 0, 0);
    vec![
        ok_obs(t0, vec![lw("refs/tags/v4", &sha('a'), &sha('1'))]),
        ok_obs(t1, vec![lw("refs/tags/v4", &sha('b'), &sha('2'))]),
        ok_obs(t2, vec![lw("refs/tags/v4", &sha('b'), &sha('2'))]),
    ]
}

fn seed_cache(path: &std::path::Path) {
    let mut cache = CompareCache::open(path).unwrap();
    cache
        .put(
            &sha('a'),
            &sha('b'),
            refledger_poller::enrich::CompareResult {
                ancestry: Ancestry::Ahead,
                files_added: 1,
                files_removed: 0,
                files_modified: 2,
                files_renamed: 0,
                paths: vec!["action.yml".into()],
                diff_possibly_truncated: false,
            },
        )
        .unwrap();
}

fn lw(name: &str, commit: &str, tree: &str) -> ObservedRef {
    ObservedRef::new_lightweight(name, commit, tree).unwrap()
}

fn ok_obs(at: OffsetDateTime, refs: Vec<ObservedRef>) -> Observation {
    Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(at)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Ok {
            http_status: 200,
            etag: Some(ETag::new("W/\"e\"")),
            refs,
        })
        .build()
        .unwrap()
}

fn enrichment_from_cache(state: &RepoState, obs: &Observation, cache: &CompareCache) -> Enrichment {
    let mut e = Enrichment::empty();
    let Outcome::Ok { refs, .. } = obs.outcome() else {
        return e;
    };
    for r in refs {
        let (Some(new_commit), Some(_)) = (r.commit_sha(), r.tree_sha()) else {
            continue;
        };
        let Some(prev) = state.binding(r.name()) else {
            continue;
        };
        if prev.commit_sha() == new_commit {
            continue;
        }
        if let Some(c) = cache.get(prev.commit_sha(), new_commit) {
            e = e.with_ancestry(prev.commit_sha(), new_commit, c.ancestry);
        }
    }
    e
}

fn diffs_from_cache(
    cache: &CompareCache,
) -> BTreeMap<(String, String), refledger_log::entry::Diff> {
    let mut m = BTreeMap::new();
    for ((old, new), c) in cache.iter() {
        m.insert(
            (old.clone(), new.clone()),
            refledger_log::entry::Diff {
                files_added: c.files_added,
                files_removed: c.files_removed,
                files_modified: c.files_modified,
                files_renamed: c.files_renamed,
                paths: c.paths.clone(),
                diff_possibly_truncated: c.diff_possibly_truncated,
            },
        );
    }
    m
}

#[test]
fn entry_buffer_survives_store_reopen() {
    let dir = TempDir::new().unwrap();
    let jan1 = day(2026, Month::January, 1);
    {
        let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
        store
            .append_observation(&obs_at(odt(2026, Month::January, 1, 12, 0, 0, 0)))
            .unwrap();
        store.stop_dispatch(jan1);
        let detected = odt(2026, Month::January, 2, 0, 0, 2, 0);
        let appended = store.append_entry(move_at(detected)).unwrap();
        assert!(matches!(appended, Appended::Buffered));
        assert_eq!(store.buffered_entry_count(), 1);
        assert!(dir.path().join("state/entry_buffer.jsonl").exists());
    }
    let mut store = Store::open(dir.path(), opts(static_rekor())).unwrap();
    assert_eq!(store.buffered_entry_count(), 1);
    let digest = store.seal_day(jan1).unwrap();
    assert_eq!(store.buffered_entry_count(), 0);
    let mv = store.entries().last().unwrap();
    assert_eq!(mv.event, Event::Move);
    assert!(mv.seq > digest.seq);
}

struct ConflictLookupRekor {
    acceptance: RekorAcceptance,
}

impl RekorClient for ConflictLookupRekor {
    fn submit(&self, _proposed: &Value) -> Result<RekorAcceptance, String> {
        Err("https://rekor.sigstore.dev/api/v1/log/entries: status code 409".into())
    }

    fn lookup_by_hash(&self, artifact_hash: &str) -> Result<Option<RekorAcceptance>, String> {
        assert!(artifact_hash.starts_with("sha512:"));
        Ok(Some(self.acceptance.clone()))
    }
}

#[test]
fn rekor_409_records_existing_log_index() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(
        dir.path(),
        opts(Box::new(ConflictLookupRekor {
            acceptance: RekorAcceptance {
                log_index: 3027764712,
                uuid: "deadbeef".into(),
                log_id: Some("logid".into()),
                integrated_time: Some(1790812975),
            },
        })),
    )
    .unwrap();
    let jan1 = day(2026, Month::January, 1);
    store.stop_dispatch(jan1);
    store.seal_day(jan1).unwrap();
    let heads = store.read_rel("log/heads.jsonl").unwrap().unwrap();
    let first = heads.split(|b| *b == b'\n').next().unwrap();
    let line: Value = serde_json::from_slice(first).unwrap();
    assert_eq!(line["rekor"]["log_index"], 3027764712u64);
    assert_eq!(line["rekor"]["uuid"], "deadbeef");
    assert!(line["rekor"].get("error").is_none());
}

fn verify_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let bin = target.join(profile).join("refledger-verify");
    // Always rebuild. A pre-existing binary can predate heads.jsonl handling.
    let status = Command::new("cargo")
        .args(["build", "-p", "refledger-verify", "--offline"])
        .status()
        .expect("build refledger-verify");
    assert!(status.success(), "refledger-verify failed to build");
    bin
}
