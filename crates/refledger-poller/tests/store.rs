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
use serde_json::Value;
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
    UnhashedEntry::move_event(
        at,
        Classification::ContentChange,
        Severity::High,
        "acme/widgets",
        "refs/tags/v1",
        RefType::Lightweight,
        RefType::Lightweight,
        binding('a', at, true),
        binding('b', at, false),
        3600,
        vec![
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
        ],
        None,
    )
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
            Err(format!("r2 unavailable for {}", archive.day))
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
    store
        .append_observation(&obs_at(odt(2026, Month::January, 1, 12, 0, 0, 0)))
        .unwrap();
    let digest = store.seal_day(day(2026, Month::January, 1)).unwrap();
    let note = digest
        .observation_digest
        .as_ref()
        .and_then(|d| d.note.as_ref())
        .expect("warning must appear in digest note");
    assert!(
        note.contains("contact URL unreachable"),
        "note={note}"
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

fn diffs_from_cache(cache: &CompareCache) -> BTreeMap<(String, String), refledger_log::entry::Diff> {
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
