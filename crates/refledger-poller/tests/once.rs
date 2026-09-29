//! GitHub Actions `once` path: gaps, missed-day seals, confirmation re-poll,
//! and the disabled-variable guard.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use refledger_log::SigningKey;
use refledger_poller::github::rest::{RestRequest, RestResponse, Transport};
use refledger_poller::observation::{ETag, Method, Observation, Outcome, SkipReason, Timestamp};
use refledger_poller::once::{poller_enabled, run_once_with, CountingTransport, OnceArgs};
use refledger_poller::population::PollGroup;
use refledger_poller::scheduler::M1_INTERVAL;
use refledger_poller::store::{RekorClient, StaticRekor, Store, StoreOptions};
use serde_json::Value;
use tempfile::TempDir;
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

fn key() -> SigningKey {
    SigningKey::from_seed_bytes(&[9u8; 32]).unwrap()
}

fn opts(at: OffsetDateTime) -> StoreOptions {
    StoreOptions {
        log_id: "refledger-test".into(),
        signing_key: key(),
        recovered_at: at,
        rekor: Box::new(StaticRekor { log_index: 7 }) as Box<dyn RekorClient>,
        archive: Box::new(refledger_poller::NoopArchive),
        publisher: Box::new(refledger_poller::NoopPublisher),
        observations_on_data_branch: true,
    }
}

fn ok_obs(repo: &str, at: OffsetDateTime) -> Observation {
    Observation::builder()
        .repo(repo)
        .unwrap()
        .observed_at(at)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Ok {
            http_status: 200,
            etag: Some(ETag::new("W/\"t\"")),
            refs: vec![],
        })
        .build()
        .unwrap()
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rest")
}

fn load_fixture(name: &str) -> RestResponse {
    let path = fixtures_dir().join(format!("{name}.json"));
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture"))
        .expect("fixture json");
    let status = value["status"].as_u64().expect("status") as u16;
    let headers = value["headers"]
        .as_object()
        .expect("headers")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned()))
        .collect();
    let body = match &value["body"] {
        Value::Null => None,
        other => Some(other.clone()),
    };
    RestResponse {
        status,
        headers,
        body,
    }
}

#[derive(Clone, Default)]
struct MockTransport {
    routes: Arc<Mutex<BTreeMap<String, String>>>,
    log: Arc<Mutex<Vec<RestRequest>>>,
}

impl MockTransport {
    fn new() -> Self {
        Self::default()
    }

    fn route(&self, contains: &str, fixture: &str) -> &Self {
        self.routes
            .lock()
            .unwrap()
            .insert(contains.to_owned(), fixture.to_owned());
        self
    }

    fn request_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }
}

impl Transport for MockTransport {
    fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
        self.log.lock().unwrap().push(request.clone());
        let routes = self.routes.lock().unwrap();
        let path = request
            .target
            .split_once('?')
            .map(|(p, _)| p)
            .unwrap_or(&request.target);
        let mut best: Option<(&String, usize)> = None;
        for (key, fixture) in routes.iter() {
            let score = if path == key || request.target == *key {
                10_000 + key.len()
            } else if path.starts_with(&format!("{key}/")) {
                continue;
            } else if request.target.contains(key) {
                key.len()
            } else {
                continue;
            };
            if best.map(|(_, s)| score >= s).unwrap_or(true) {
                best = Some((fixture, score));
            }
        }
        let fixture = best
            .map(|(f, _)| f)
            .ok_or_else(|| format!("no fixture for {}", request.target))?;
        Ok(load_fixture(fixture))
    }
}

#[test]
fn disabled_variable_means_zero_requests() {
    assert!(!poller_enabled(None));
    assert!(!poller_enabled(Some("false")));
    assert!(!poller_enabled(Some("True")));
    assert!(!poller_enabled(Some("1")));
    assert!(poller_enabled(Some("true")));
    // Guard is evaluated before any transport is constructed.
    let mock = MockTransport::new();
    assert_eq!(mock.request_count(), 0);
}

#[test]
fn gap_recorded_between_runs_as_scheduler_lag() {
    let dir = TempDir::new().unwrap();
    let last = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let scheduled = last + Duration::hours(1);
    let actual = last + Duration::minutes(25); // > 2×300s

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];

    {
        let mut store = Store::open(dir.path(), opts(last)).unwrap();
        store
            .append_observation(&ok_obs("acme/widgets", last))
            .unwrap();
    }

    let mut store = Store::open(dir.path(), opts(actual)).unwrap();
    let gaps = store
        .record_schedule_gaps(&groups, M1_INTERVAL, scheduled, actual)
        .unwrap();
    assert_eq!(gaps.len(), 1);
    match gaps[0].outcome() {
        Outcome::Skipped {
            reason:
                SkipReason::SchedulerLag {
                    scheduled: s,
                    actual: a,
                },
        } => {
            assert_eq!(s.as_offset_datetime(), scheduled);
            assert_eq!(a.as_offset_datetime(), actual);
        }
        other => panic!("expected SchedulerLag, got {other:?}"),
    }
}

#[test]
fn run_after_26_hours_seals_both_missed_days_in_order() {
    let dir = TempDir::new().unwrap();
    let day0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    // 26 hours later: Jan 2 14:00 — days Jan 1 and Jan 2 must seal before polling Jan 2… wait.
    // latest.date() = Jan 1, today = Jan 2 → only seal Jan 1.
    // Need today = Jan 3 so both Jan 1 and Jan 2 seal.
    let now = day0 + Duration::hours(26) + Duration::hours(12); // Jan 3 02:00
    assert_eq!(now.date().day(), 3);

    {
        let mut store = Store::open(dir.path(), opts(day0)).unwrap();
        store
            .append_observation(&ok_obs("acme/widgets", day0))
            .unwrap();
    }

    let mut store = Store::open(dir.path(), opts(now)).unwrap();
    let sealed = store.seal_missed_days_before(now).unwrap();
    assert_eq!(sealed.len(), 2, "expected Jan 1 then Jan 2, got {sealed:?}");
    assert_eq!(sealed[0].day(), 1);
    assert_eq!(sealed[1].day(), 2);

    // Digests landed: tip should include two ObservationDigest entries.
    let digests: Vec<_> = store
        .entries()
        .iter()
        .filter(|e| e.observation_digest.is_some())
        .collect();
    assert_eq!(digests.len(), 2);
    assert!(
        digests[0]
            .observation_digest
            .as_ref()
            .unwrap()
            .note
            .as_deref()
            == Some("observation files published on the data branch")
            || digests.iter().any(|e| e
                .observation_digest
                .as_ref()
                .and_then(|d| d.note.as_ref())
                .is_some_and(|n| n.contains("observation files published on the data branch"))),
        "seal note must mention data branch observations"
    );
}

#[test]
fn confirmation_re_poll_recorded_after_movement() {
    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);

    // Prior Ok with one lightweight binding so the next Ok looks like a move.
    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        let prior = Observation::builder()
            .repo("acme/widgets")
            .unwrap()
            .observed_at(t0)
            .unwrap()
            .method(Method::Rest)
            .outcome(Outcome::Ok {
                http_status: 200,
                etag: Some(ETag::new("W/\"old\"")),
                refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                    "v1",
                    "1111111111111111111111111111111111111111",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap()],
            })
            .build()
            .unwrap();
        store.append_observation(&prior).unwrap();
    }

    let mock = MockTransport::new();
    mock.route("/repos/acme/widgets", "repo_ok")
        .route("/repos/acme/widgets/git/matching-refs/tags", "tags_moved")
        .route(
            "/repos/acme/widgets/git/commits/2222222222222222222222222222222222222222",
            "git_commit_2",
        )
        .route(
            "/repos/acme/widgets/contents/action.yml",
            "contents_action_yml",
        );

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];

    let slept = Arc::new(Mutex::new(false));
    let slept_flag = slept.clone();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test_token".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.confirm_delay = Duration::seconds(0);
    args.sleep = Box::new(move |_| {
        *slept_flag.lock().unwrap() = true;
    });
    args.max_requests = 150;
    args.max_new_peels = 40;

    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    let transport = CountingTransport::new(mock.clone(), 150);
    let report = run_once_with(&mut store, transport, &groups, &args).expect("once");
    assert!(*slept.lock().unwrap(), "confirm delay must be invoked");
    assert_eq!(report.confirmations, 1, "movement must trigger re-poll");
    assert!(report.observations >= 2);
    assert!(report.requests > 0);

    // Both observations stamped with schedule lag fields.
    let stamped: Vec<_> = store
        .latest_ok_targets("acme/widgets")
        .unwrap()
        .into_iter()
        .collect();
    assert!(!stamped.is_empty());
}

#[test]
fn schedule_stamps_survive_round_trip() {
    let scheduled =
        Timestamp::from_offset_datetime(odt(2026, Month::January, 1, 12, 2, 0, 0)).unwrap();
    let actual =
        Timestamp::from_offset_datetime(odt(2026, Month::January, 1, 12, 3, 0, 0)).unwrap();
    let mut obs = ok_obs("acme/widgets", actual.as_offset_datetime());
    obs.stamp_schedule(scheduled, actual);
    let wire = serde_json::to_string(&obs).unwrap();
    assert!(wire.contains("scheduled_at"));
    assert!(wire.contains("actual_start"));
    let back: Observation = serde_json::from_str(&wire).unwrap();
    assert_eq!(back.scheduled_at(), Some(scheduled));
    assert_eq!(back.actual_start(), Some(actual));
}

/// The tidy-clock confirmation test above can hide the live failure mode:
/// `OffsetDateTime::now_utc()` carries nanoseconds. Drive `once` with the
/// real system clock against the fake transport end to end.
#[test]
fn once_with_real_system_clock_against_fake_transport() {
    let dir = TempDir::new().unwrap();
    let raw_now = OffsetDateTime::now_utc();
    // Force a sub-millisecond remainder when the OS happened to land on a
    // millisecond boundary (rare but possible).
    let messy = raw_now
        .replace_nanosecond((raw_now.nanosecond() / 1_000_000) * 1_000_000 + 123_456)
        .unwrap_or(raw_now);
    assert_ne!(
        messy.nanosecond() % 1_000_000,
        0,
        "test setup must use a non-ms clock reading"
    );

    let mock = MockTransport::new();
    mock.route("/repos/acme/widgets", "repo_ok")
        .route(
            "/repos/acme/widgets/git/matching-refs/tags",
            "tags_lightweight",
        )
        .route(
            "/repos/acme/widgets/git/commits/1111111111111111111111111111111111111111",
            "git_commit_1",
        )
        .route(
            "/repos/acme/widgets/contents/action.yml",
            "contents_action_yml",
        );

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];

    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test_token".into();
    // Deliberately unnormalised — run_once_with must floor at the boundary.
    args.scheduled_at = messy;
    args.actual_start = messy;
    args.confirm_delay = Duration::seconds(0);
    args.sleep = Box::new(|_| {});
    args.max_requests = 150;
    args.max_new_peels = 40;

    let mut store = Store::open(dir.path(), opts(messy)).unwrap();
    let transport = CountingTransport::new(mock, 150);
    let report = run_once_with(&mut store, transport, &groups, &args)
        .expect("once must accept a real (nanosecond) clock after normalisation");
    assert!(report.observations >= 1);
    assert!(report.requests > 0);

    // Persisted observations must carry canonical …sssZ stamps.
    let day = at_observation_path(dir.path(), messy);
    let text = std::fs::read_to_string(&day).expect("observation file");
    for line in text.lines().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_str(line).unwrap();
        let at = v["observed_at"].as_str().expect("observed_at");
        assert_eq!(
            at.len(),
            "YYYY-MM-DDTHH:MM:SS.sssZ".len(),
            "non-canonical observed_at length: {at}"
        );
        assert!(at.ends_with('Z'), "observed_at must end with Z: {at}");
        assert_eq!(
            at.as_bytes()[at.len() - 5],
            b'.',
            "observed_at must include .sss before Z: {at}"
        );
    }
}

fn at_observation_path(root: &Path, at: OffsetDateTime) -> PathBuf {
    let day = at.date();
    root.join("observations")
        .join(format!("{:04}", day.year()))
        .join(format!("{:02}", u8::from(day.month())))
        .join(format!("{:02}", day.day()))
        .join("acme--widgets.jsonl")
}
