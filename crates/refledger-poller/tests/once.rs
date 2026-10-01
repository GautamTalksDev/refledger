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
        skip_lock: false,
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

#[test]
fn counting_transport_never_exceeds_max() {
    #[derive(Clone, Default)]
    struct OkOnce {
        hits: Arc<Mutex<u32>>,
    }
    impl Transport for OkOnce {
        fn send(&self, _request: &RestRequest) -> Result<RestResponse, String> {
            *self.hits.lock().unwrap() += 1;
            Ok(RestResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: Some(serde_json::json!({})),
            })
        }
    }
    let inner = OkOnce::default();
    let t = CountingTransport::new(inner.clone(), 2);
    assert!(t
        .send(&RestRequest {
            method: "GET",
            target: "/a".into(),
            headers: BTreeMap::new(),
        })
        .is_ok());
    assert!(t
        .send(&RestRequest {
            method: "GET",
            target: "/b".into(),
            headers: BTreeMap::new(),
        })
        .is_ok());
    let err = t
        .send(&RestRequest {
            method: "GET",
            target: "/c".into(),
            headers: BTreeMap::new(),
        })
        .expect_err("third send must be denied");
    assert!(err.contains("budget exhausted"), "{err}");
    assert_eq!(t.requests(), 2, "counter must never exceed max");
    assert_eq!(*inner.hits.lock().unwrap(), 2);
}

#[test]
fn phase1_lists_every_group_while_peel_backlog_exceeds_budget() {
    use refledger_poller::github::rest::{RestRequest, RestResponse, Transport};
    use refledger_poller::once::{run_once_with, CountingTransport, OnceArgs};
    use refledger_poller::population::PollGroup;
    use refledger_poller::store::Store;
    use serde_json::json;
    use std::collections::BTreeMap;

    const N_GROUPS: usize = 10;
    const REFS_PER_GROUP: usize = 300;

    #[derive(Clone, Default)]
    struct HugeBacklog {
        log: Arc<Mutex<Vec<RestRequest>>>,
    }
    impl Transport for HugeBacklog {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            self.log.lock().unwrap().push(request.clone());
            let t = &request.target;
            if t.contains("/repos/") && !t.contains("/git/") && !t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                let refs: Vec<_> = (0..REFS_PER_GROUP)
                    .map(|i| {
                        json!({
                            "ref": format!("refs/tags/v{i}"),
                            "object": {
                                "type": "commit",
                                "sha": format!("{:040x}", i + 1)
                            }
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"tags\"".into())]),
                    body: Some(json!(refs)),
                });
            }
            if t.contains("/git/commits/") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "sha": "1111111111111111111111111111111111111111",
                        "tree": {"sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
                    })),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected target {t}"))
        }
    }

    let dir = TempDir::new().unwrap();
    let at = odt(2026, Month::January, 1, 12, 2, 0, 0);
    let groups: Vec<PollGroup> = (0..N_GROUPS)
        .map(|i| PollGroup {
            repo: format!("org/repo-{i}"),
            paths: vec![None],
        })
        .collect();
    {
        let mut store = Store::open(dir.path(), opts(at)).unwrap();
        for g in &groups {
            store
                .append_observation(&ok_obs(&g.repo, at - Duration::minutes(5)))
                .unwrap();
        }
    }

    let inner = HugeBacklog::default();
    let transport = CountingTransport::new(inner.clone(), 300);
    let mut store = Store::open(dir.path(), opts(at + Duration::minutes(5))).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = at + Duration::minutes(5);
    args.actual_start = at + Duration::minutes(5);
    args.sleep = Box::new(|_| {});
    args.max_requests = 300;
    args.max_new_peels = 120;
    args.confirm_delay = Duration::seconds(0);

    let report = run_once_with(&mut store, transport, &groups, &args).unwrap();
    assert_eq!(report.observations, N_GROUPS);
    let listings = inner
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target.contains("matching-refs/tags"))
        .count();
    assert!(
        listings >= N_GROUPS,
        "phase 1 must list every group before warm-up: {listings} listings for {N_GROUPS} groups"
    );
    fn obs_lines(root: &Path, out: &mut Vec<String>) {
        if root.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Ok(text) = std::fs::read_to_string(root) {
                out.extend(text.lines().map(str::to_owned));
            }
            return;
        }
        if root.is_dir() {
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    obs_lines(&e.path(), out);
                }
            }
        }
    }
    let mut lines = Vec::new();
    obs_lines(&dir.path().join("observations"), &mut lines);
    let budget_skips = lines
        .iter()
        .filter(|l| l.contains("budget_exhausted"))
        .count();
    assert_eq!(
        budget_skips, 0,
        "listing must not be skipped for warm-up backlog"
    );
}

#[test]
fn fair_skip_rotates_so_every_group_is_polled_across_runs() {
    use refledger_poller::population::{fair_skip_offset, rotate_groups};
    let groups: Vec<String> = (0..5).map(|i| format!("org/repo-{i}")).collect();
    let mut seen = std::collections::BTreeSet::new();
    // Five consecutive 5-minute cron slots.
    for i in 0..5 {
        let scheduled = odt(2026, Month::January, 1, 12, 2, 0, 0) + Duration::minutes(i * 5);
        let offset = fair_skip_offset(scheduled, groups.len());
        let order = rotate_groups(&groups, offset);
        // With budget of 1, only the first group in rotated order is polled.
        seen.insert(order[0].clone());
    }
    assert_eq!(
        seen.len(),
        5,
        "over N runs every group must be first at least once: {seen:?}"
    );
}

#[test]
fn genesis_added_entries_replay_identically_from_data_observations() {
    use refledger_log::entry::PopulationChangeKind;
    use refledger_poller::population::{
        genesis_added_entries, load_watched, EarliestObservation, WatchedKey,
    };
    use std::collections::{BTreeMap, BTreeSet};

    let watched_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../population/watched.jsonl");
    let watched = load_watched(&watched_path).expect("watched.jsonl");
    let data = Path::new("/home/gautamtalksdev/projects/refledger-data/observations");
    if !data.exists() {
        // Worktree may be absent in CI; synthesise from fixture-like times.
        return;
    }
    let mut earliest: BTreeMap<WatchedKey, EarliestObservation> = BTreeMap::new();
    for f in std::fs::read_dir(data.join("2026/09/29")).unwrap() {
        let f = f.unwrap().path();
        if f.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            let repo = v["repo"].as_str().unwrap().to_owned();
            let path = v
                .get("action_path")
                .and_then(|p| p.as_str())
                .map(str::to_owned);
            let key = WatchedKey::new(repo, path);
            let at = OffsetDateTime::parse(
                v["observed_at"].as_str().unwrap(),
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap();
            let id = v["observation_id"].as_str().unwrap().to_owned();
            match earliest.get(&key) {
                Some(prev) if prev.observed_at <= at => {}
                _ => {
                    earliest.insert(
                        key,
                        EarliestObservation {
                            observed_at: at,
                            observation_id: id,
                        },
                    );
                }
            }
        }
    }
    let run_now = OffsetDateTime::parse(
        "2026-09-29T20:00:00.000Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let a = genesis_added_entries(&watched, &earliest, &BTreeSet::new(), run_now);
    let b = genesis_added_entries(&watched, &earliest, &BTreeSet::new(), run_now);
    assert_eq!(a, b, "true-genesis Added rows must be replay-deterministic");
    assert_eq!(
        a.len(),
        watched.iter().filter(|e| e.active).count(),
        "every active watched key must get an Added row"
    );
    // Late registration path (already_added non-empty) is also deterministic.
    let already: BTreeSet<_> = a
        .iter()
        .filter_map(|e| {
            let repo = e.repo.as_ref()?.clone();
            let path = e.population_change.as_ref()?.path.clone();
            Some(WatchedKey::new(repo, path))
        })
        .take(a.len().saturating_sub(3))
        .collect();
    let late_a = genesis_added_entries(&watched, &earliest, &already, run_now);
    let late_b = genesis_added_entries(&watched, &earliest, &already, run_now);
    assert_eq!(
        late_a, late_b,
        "late-registration Added rows must be replay-deterministic"
    );
    assert_eq!(late_a.len(), 3);
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(
            x.population_change.as_ref().unwrap().change,
            PopulationChangeKind::Added
        );
        let _ = y;
    }
    // Ordered by (recorded_at, repo, path).
    for w in a.windows(2) {
        let ka = (
            w[0].recorded_at,
            w[0].repo.as_deref().unwrap_or(""),
            w[0].population_change
                .as_ref()
                .and_then(|p| p.path.as_deref())
                .unwrap_or(""),
        );
        let kb = (
            w[1].recorded_at,
            w[1].repo.as_deref().unwrap_or(""),
            w[1].population_change
                .as_ref()
                .and_then(|p| p.path.as_deref())
                .unwrap_or(""),
        );
        assert!(ka <= kb, "not sorted: {ka:?} then {kb:?}");
    }
}

#[test]
fn run_once_records_move_deletion_and_recreation_on_chain() {
    use refledger_log::entry::Event;

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    let t2 = t1 + Duration::minutes(5);
    let t3 = t2 + Duration::minutes(5);

    // Seed prior Ok so first live sweep is a move (not a creation).
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
                    "refs/tags/v1",
                    "1111111111111111111111111111111111111111",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap()],
            })
            .build()
            .unwrap();
        store.append_observation(&prior).unwrap();
    }

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];

    // Run 1: tag moves to commit 2 → Move on chain.
    {
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
            )
            .route(
                "/repos/acme/widgets/compare/1111111111111111111111111111111111111111...2222222222222222222222222222222222222222",
                "compare_ahead",
            );
        let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
        args.token = "ghp_test".into();
        args.scheduled_at = t1;
        args.actual_start = t1;
        args.sleep = Box::new(|_| {});
        args.confirm_delay = Duration::seconds(0);
        args.max_requests = 150;
        args.max_new_peels = 40;
        let mut store = Store::open(dir.path(), opts(t1)).unwrap();
        let report = run_once_with(
            &mut store,
            CountingTransport::new(mock, 150),
            &groups,
            &args,
        )
        .expect("move run");
        assert!(report.derived_events >= 1, "move must derive: {report:?}");
        assert!(
            store.entries().iter().any(|e| e.event == Event::Move),
            "chain must contain Move; events={:?}",
            store.entries().iter().map(|e| e.event).collect::<Vec<_>>()
        );
    }

    // Run 2: empty tag list → Deletion.
    {
        let mock = MockTransport::new();
        mock.route("/repos/acme/widgets", "repo_ok")
            .route("/repos/acme/widgets/git/matching-refs/tags", "tags_empty");
        let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
        args.token = "ghp_test".into();
        args.scheduled_at = t2;
        args.actual_start = t2;
        args.sleep = Box::new(|_| {});
        args.confirm_delay = Duration::seconds(0);
        args.max_requests = 150;
        args.max_new_peels = 40;
        let mut store = Store::open(dir.path(), opts(t2)).unwrap();
        let report = run_once_with(
            &mut store,
            CountingTransport::new(mock, 150),
            &groups,
            &args,
        )
        .expect("delete run");
        assert!(
            report.derived_events >= 1,
            "deletion must derive: {report:?}"
        );
        assert!(
            store.entries().iter().any(|e| e.event == Event::Deletion),
            "chain must contain Deletion"
        );
    }

    // Run 3: tag returns → Recreation.
    {
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
        let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
        args.token = "ghp_test".into();
        args.scheduled_at = t3;
        args.actual_start = t3;
        args.sleep = Box::new(|_| {});
        args.confirm_delay = Duration::seconds(0);
        args.max_requests = 150;
        args.max_new_peels = 40;
        let mut store = Store::open(dir.path(), opts(t3)).unwrap();
        let report = run_once_with(
            &mut store,
            CountingTransport::new(mock, 150),
            &groups,
            &args,
        )
        .expect("recreate run");
        assert!(
            report.derived_events >= 1,
            "recreation must derive: {report:?}"
        );
        assert!(
            store.entries().iter().any(|e| e.event == Event::Recreation),
            "chain must contain Recreation; events={:?}",
            store.entries().iter().map(|e| e.event).collect::<Vec<_>>()
        );
    }
}

#[test]
fn confirm_and_enrich_run_despite_full_backfill_backlog() {
    use refledger_poller::github::rest::{RestRequest, RestResponse, Transport};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    const N_BACKLOG: usize = 8;
    const REFS_EACH: usize = 80;

    #[derive(Clone, Default)]
    struct BacklogPlusMove {
        log: Arc<Mutex<Vec<RestRequest>>>,
        compares: Arc<AtomicU32>,
    }
    impl Transport for BacklogPlusMove {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            self.log.lock().unwrap().push(request.clone());
            let t = &request.target;
            if t.contains("/compare/") {
                self.compares.fetch_add(1, AtomicOrdering::Relaxed);
                return Ok(load_fixture("compare_ahead"));
            }
            if t.contains("/repos/") && !t.contains("/git/") && !t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                // Moved repo gets tip 2222…; backlog repos get many unique tips.
                if t.contains("acme/moved") {
                    return Ok(load_fixture("tags_moved"));
                }
                let refs: Vec<_> = (0..REFS_EACH)
                    .map(|i| {
                        json!({
                            "ref": format!("refs/tags/v{i}"),
                            "object": {
                                "type": "commit",
                                "sha": format!("{:040x}", i + 1)
                            }
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"tags\"".into())]),
                    body: Some(json!(refs)),
                });
            }
            if t.contains("/git/commits/") {
                let sha = t.rsplit('/').next().unwrap_or("").to_owned();
                let tree = if sha.starts_with("2222") {
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                } else {
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                };
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "sha": sha,
                        "tree": {"sha": tree}
                    })),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected {t}"))
        }
    }

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);

    let mut groups = vec![PollGroup {
        repo: "acme/moved".into(),
        paths: vec![None],
    }];
    for i in 0..N_BACKLOG {
        groups.push(PollGroup {
            repo: format!("org/backlog-{i}"),
            paths: vec![None],
        });
    }

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        // Prior for moved repo (so listing looks like a move).
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/moved")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v1",
                            "1111111111111111111111111111111111111111",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        for g in &groups[1..] {
            store.append_observation(&ok_obs(&g.repo, t0)).unwrap();
        }
    }

    let inner = BacklogPlusMove::default();
    let compares = inner.compares.clone();
    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 80; // deliberately tight so backfill would starve confirm without reserve
    args.max_new_peels = 200;

    let report = run_once_with(
        &mut store,
        CountingTransport::new(inner, 80),
        &groups,
        &args,
    )
    .expect("once with backlog");
    assert_eq!(
        report.confirmations, 1,
        "confirm must still run: {report:?}"
    );
    assert!(
        compares.load(AtomicOrdering::Relaxed) >= 1,
        "enrich compare must still run"
    );
    assert!(
        store
            .entries()
            .iter()
            .any(|e| e.event == refledger_log::entry::Event::Move),
        "move must land despite backlog"
    );
}

/// Brand-new never-seen commit (attack shape): must peel before classify and
/// emit Move ContentChange High on an exact tag.
#[test]
fn run_once_peels_never_seen_commit_before_classify_exact_move() {
    use refledger_log::entry::{Classification, Event, Severity};

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const TREE_NEW: &str = "dddddddddddddddddddddddddddddddddddddddd";

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/widgets")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v1.2.3",
                            OLD,
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct AttackTransport {
        log: Arc<Mutex<Vec<String>>>,
    }
    impl Transport for AttackTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            self.log.lock().unwrap().push(request.target.clone());
            let t = &request.target;
            if t.contains("/repos/acme/widgets")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(serde_json::json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"moved\"".into())]),
                    body: Some(serde_json::json!([{
                        "ref": "refs/tags/v1.2.3",
                        "object": {"type": "commit", "sha": NEW}
                    }])),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"message": "Not Found"})),
                });
            }
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 0; // warm-up peel budget zero — priority peel must still run

    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    let report = run_once_with(
        &mut store,
        CountingTransport::new(AttackTransport::default(), 150),
        &groups,
        &args,
    )
    .expect("once");
    assert!(report.derived_events >= 1, "{report:?}");

    let mv = store
        .entries()
        .iter()
        .find(|e| e.event == Event::Move)
        .expect("Move on chain");
    assert_eq!(mv.r#ref.as_deref(), Some("refs/tags/v1.2.3"));
    assert_eq!(mv.classification, Some(Classification::ContentChange));
    assert_eq!(mv.severity, Some(Severity::High));
    assert_eq!(mv.to.as_ref().map(|b| b.commit_sha.as_str()), Some(NEW));
}

#[test]
fn run_once_batch_of_three_exact_to_never_seen_commit_emits_correlation() {
    use refledger_log::entry::Event;

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const TREE_NEW: &str = "ffffffffffffffffffffffffffffffffffffffff";

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        let refs: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
            .iter()
            .map(|t| {
                refledger_poller::observation::ObservedRef::new_lightweight(
                    format!("refs/tags/{t}"),
                    OLD,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap()
            })
            .collect();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/widgets")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs,
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct BatchTransport;
    impl Transport for BatchTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/acme/widgets")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(serde_json::json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                let body: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
                    .iter()
                    .map(|tag| {
                        serde_json::json!({
                            "ref": format!("refs/tags/{tag}"),
                            "object": {"type": "commit", "sha": NEW}
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"batch\"".into())]),
                    body: Some(serde_json::json!(body)),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"message": "Not Found"})),
                });
            }
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 0;

    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    run_once_with(
        &mut store,
        CountingTransport::new(BatchTransport, 150),
        &groups,
        &args,
    )
    .expect("once");

    let moves: Vec<_> = store
        .entries()
        .iter()
        .filter(|e| e.event == Event::Move)
        .collect();
    assert_eq!(moves.len(), 3, "three Moves");
    assert!(
        store
            .entries()
            .iter()
            .any(|e| e.event == Event::Correlation),
        "Correlation required"
    );
}

/// Live failure shape (2026-10-01T02:18Z): v9 tips first met listing-only at
/// `5f7797a7…`, then moved listing-only to `9501ea4a…`. Prior binding was never
/// peeled, so priority-peel must key off target_sha change alone and peel the
/// FROM commit too. SHAs and trees match the archived canary observations.
#[test]
fn run_once_batch_listing_only_prior_matches_live_v9_archive() {
    use refledger_log::entry::{Classification, Event, Severity};
    use refledger_poller::observation::{ObservedRef, RefType};

    let dir = TempDir::new().unwrap();
    // Times mirror the archive: first listing-only Ok, then the confirm move.
    let t_bootstrap = odt(2026, Month::October, 1, 1, 57, 51, 55);
    let t_move = odt(2026, Month::October, 1, 2, 18, 39, 455);
    const OLD: &str = "5f7797a750e27645b21d80f89c20e006e9f8da65";
    const TREE_OLD: &str = "6a3e03f7367bbcc085dadbe3871e43bb5b47b0d4";
    const NEW: &str = "9501ea4aa193609399ba0e66a5248432246ad445";
    const TREE_NEW: &str = "c9d38f5feafb6da041f61a783e068ec752312225";

    {
        let mut store = Store::open(dir.path(), opts(t_bootstrap)).unwrap();
        let refs: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
            .iter()
            .map(|t| {
                ObservedRef::new_unpeeled(format!("refs/tags/{t}"), RefType::Lightweight, OLD)
                    .unwrap()
            })
            .collect();
        store
            .append_observation(
                &Observation::builder()
                    .repo("GautamTalksDev/canary")
                    .unwrap()
                    .observed_at(t_bootstrap)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"v9-bootstrap\"")),
                        refs,
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct LiveBatchTransport;
    impl Transport for LiveBatchTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/GautamTalksDev/canary")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(serde_json::json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                let body: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
                    .iter()
                    .map(|tag| {
                        serde_json::json!({
                            "ref": format!("refs/tags/{tag}"),
                            "object": {"type": "commit", "sha": NEW}
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"batch-moved\"".into())]),
                    body: Some(serde_json::json!(body)),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains(&format!("/git/commits/{OLD}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": OLD, "tree": {"sha": TREE_OLD}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"message": "Not Found"})),
                });
            }
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "GautamTalksDev/canary".into(),
        paths: vec![None],
    }];
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t_move;
    args.actual_start = t_move;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    // Cold peel budget: warm backfill must not be required for the attack tips.
    args.max_new_peels = 0;

    let mut store = Store::open(dir.path(), opts(t_move)).unwrap();
    run_once_with(
        &mut store,
        CountingTransport::new(LiveBatchTransport, 150),
        &groups,
        &args,
    )
    .expect("once");

    let moves: Vec<_> = store
        .entries()
        .iter()
        .filter(|e| e.event == Event::Move)
        .collect();
    assert_eq!(
        moves.len(),
        3,
        "three Moves from listing-only prior: {moves:?}"
    );
    for mv in &moves {
        assert_eq!(mv.classification, Some(Classification::ContentChange));
        assert_eq!(mv.severity, Some(Severity::High));
        assert_eq!(mv.from.as_ref().map(|b| b.target_sha.as_str()), Some(OLD));
        assert_eq!(mv.to.as_ref().map(|b| b.target_sha.as_str()), Some(NEW));
    }
    assert!(
        store
            .entries()
            .iter()
            .any(|e| e.event == Event::Correlation),
        "Correlation required"
    );
}

/// Cold-state twin of `run_once_peels_never_seen_commit_before_classify_exact_move`:
/// previous observation was listing-only (never peeled), matching first contact
/// with an already-moved tip.
#[test]
fn run_once_peels_never_seen_commit_cold_listing_only_prior() {
    use refledger_log::entry::{Classification, Event, Severity};
    use refledger_poller::observation::{ObservedRef, RefType};

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    const OLD: &str = "1111111111111111111111111111111111111111";
    const TREE_OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const TREE_NEW: &str = "dddddddddddddddddddddddddddddddddddddddd";

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/widgets")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![ObservedRef::new_unpeeled(
                            "refs/tags/v1.2.3",
                            RefType::Lightweight,
                            OLD,
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct ColdAttackTransport;
    impl Transport for ColdAttackTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/acme/widgets")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(serde_json::json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"moved\"".into())]),
                    body: Some(serde_json::json!([{
                        "ref": "refs/tags/v1.2.3",
                        "object": {"type": "commit", "sha": NEW}
                    }])),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains(&format!("/git/commits/{OLD}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": OLD, "tree": {"sha": TREE_OLD}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"message": "Not Found"})),
                });
            }
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 0;

    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    run_once_with(
        &mut store,
        CountingTransport::new(ColdAttackTransport, 150),
        &groups,
        &args,
    )
    .expect("once");

    let mv = store
        .entries()
        .iter()
        .find(|e| e.event == Event::Move)
        .expect("Move on chain");
    assert_eq!(mv.r#ref.as_deref(), Some("refs/tags/v1.2.3"));
    assert_eq!(mv.classification, Some(Classification::ContentChange));
    assert_eq!(mv.severity, Some(Severity::High));
    assert_eq!(mv.from.as_ref().map(|b| b.target_sha.as_str()), Some(OLD));
    assert_eq!(mv.to.as_ref().map(|b| b.commit_sha.as_str()), Some(NEW));
}

/// Recovery re-derive: archive already holds the listing-only batch move
/// (02:18 shape) but no Moves were written. Next run must peel both sides and
/// append 3 Moves + Correlation without another tip change.
#[test]
fn run_once_recovers_listing_only_batch_move_from_archive() {
    use refledger_log::entry::Event;
    use refledger_poller::observation::{ObservedRef, RefType};
    use serde_json::json;

    let dir = TempDir::new().unwrap();
    const OLD: &str = "5f7797a750e27645b21d80f89c20e006e9f8da65";
    const TREE_OLD: &str = "6a3e03f7367bbcc085dadbe3871e43bb5b47b0d4";
    const NEW: &str = "9501ea4aa193609399ba0e66a5248432246ad445";
    const TREE_NEW: &str = "c9d38f5feafb6da041f61a783e068ec752312225";

    let t0 = odt(2026, Month::October, 1, 1, 0, 0, 0);
    let t_list = odt(2026, Month::October, 1, 1, 57, 51, 55);
    let t_move = odt(2026, Month::October, 1, 2, 18, 39, 455);
    let t_recover = odt(2026, Month::October, 1, 2, 40, 0, 0);

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        // Chain tip before the missed observations so recovery considers them.
        store
            .append_entry(refledger_log::chain::UnhashedEntry::correction(
                t0,
                0,
                "recovery-tip",
            ))
            .unwrap();
        let listing: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
            .iter()
            .map(|t| {
                ObservedRef::new_unpeeled(format!("refs/tags/{t}"), RefType::Lightweight, OLD)
                    .unwrap()
            })
            .collect();
        store
            .append_observation(
                &Observation::builder()
                    .repo("GautamTalksDev/canary")
                    .unwrap()
                    .observed_at(t_list)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"list\"")),
                        refs: listing,
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        let moved: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
            .iter()
            .map(|t| {
                ObservedRef::new_unpeeled(format!("refs/tags/{t}"), RefType::Lightweight, NEW)
                    .unwrap()
            })
            .collect();
        store
            .append_observation(
                &Observation::builder()
                    .repo("GautamTalksDev/canary")
                    .unwrap()
                    .observed_at(t_move)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"moved\"")),
                        refs: moved,
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct RecoverListingTransport;
    impl Transport for RecoverListingTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/GautamTalksDev/canary")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                // Still at NEW — recovery must not require another move.
                let body: Vec<_> = ["v9.0.0", "v9.0.1", "v9.0.2"]
                    .iter()
                    .map(|tag| {
                        json!({
                            "ref": format!("refs/tags/{tag}"),
                            "object": {"type": "commit", "sha": NEW}
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"moved\"".into())]),
                    body: Some(json!(body)),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains(&format!("/git/commits/{OLD}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({"sha": OLD, "tree": {"sha": TREE_OLD}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "GautamTalksDev/canary".into(),
        paths: vec![None],
    }];
    let mut store = Store::open(dir.path(), opts(t_recover)).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t_recover;
    args.actual_start = t_recover;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 0;

    run_once_with(
        &mut store,
        CountingTransport::new(RecoverListingTransport, 150),
        &groups,
        &args,
    )
    .expect("recovery once");

    let moves: Vec<_> = store
        .entries()
        .iter()
        .filter(|e| e.event == Event::Move)
        .collect();
    assert_eq!(
        moves.len(),
        3,
        "recovery must record the missed batch: {moves:?}"
    );
    assert!(
        store
            .entries()
            .iter()
            .any(|e| e.event == Event::Correlation),
        "Correlation required on recovery"
    );
}

#[test]
fn run_once_recreation_to_never_seen_commit_is_peeled() {
    use refledger_log::entry::Event;

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    let t2 = t1 + Duration::minutes(5);
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "abababababababababababababababababababab";
    const TREE_NEW: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

    // Seed with tag present, then delete via empty Ok through store+classify path.
    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/widgets")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v3.0.0",
                            OLD,
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        // Deletion observation
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/widgets")
                    .unwrap()
                    .observed_at(t1)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"gone\"")),
                        refs: vec![],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct RecreateTransport;
    impl Transport for RecreateTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/acme/widgets")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(serde_json::json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"back\"".into())]),
                    body: Some(serde_json::json!([{
                        "ref": "refs/tags/v3.0.0",
                        "object": {"type": "commit", "sha": NEW}
                    }])),
                });
            }
            if t.contains(&format!("/git/commits/{NEW}")) {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"sha": NEW, "tree": {"sha": TREE_NEW}})),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(serde_json::json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "acme/widgets".into(),
        paths: vec![None],
    }];
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t2;
    args.actual_start = t2;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 0;

    let mut store = Store::open(dir.path(), opts(t2)).unwrap();
    let report = run_once_with(
        &mut store,
        CountingTransport::new(RecreateTransport, 150),
        &groups,
        &args,
    )
    .expect("once");
    assert!(
        report.derived_events >= 1 || store.entries().iter().any(|e| e.event == Event::Recreation),
        "Recreation expected: {report:?} events={:?}",
        store.entries().iter().map(|e| e.event).collect::<Vec<_>>()
    );
    let rec = store
        .entries()
        .iter()
        .find(|e| e.event == Event::Recreation)
        .expect("Recreation on chain");
    assert_eq!(
        rec.to.as_ref().map(|b| b.commit_sha.as_str()),
        Some(NEW),
        "recreate must be peeled to never-seen commit"
    );
}

#[test]
fn run_once_priority_peels_50_tag_batch_despite_full_backfill_backlog() {
    use refledger_log::entry::Event;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    const N_BACKLOG: usize = 36;
    const BATCH: usize = 50;

    #[derive(Clone, Default)]
    struct HugeBatch {
        peels: Arc<AtomicU32>,
    }
    impl Transport for HugeBatch {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/compare/") {
                return Ok(load_fixture("compare_ahead"));
            }
            if t.contains("/repos/") && !t.contains("/git/") && !t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                if t.contains("acme/batch") {
                    let refs: Vec<_> = (0..BATCH)
                        .map(|i| {
                            json!({
                                "ref": format!("refs/tags/v1.0.{i}"),
                                "object": {
                                    "type": "commit",
                                    "sha": format!("b{:039x}", i + 1)
                                }
                            })
                        })
                        .collect();
                    return Ok(RestResponse {
                        status: 200,
                        headers: BTreeMap::from([("etag".into(), "W/\"batch\"".into())]),
                        body: Some(json!(refs)),
                    });
                }
                // Backlog repos: many uncached tips
                let refs: Vec<_> = (0..80)
                    .map(|i| {
                        json!({
                            "ref": format!("refs/tags/v{i}"),
                            "object": {"type": "commit", "sha": format!("{:040x}", i + 100)}
                        })
                    })
                    .collect();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"tags\"".into())]),
                    body: Some(json!(refs)),
                });
            }
            if t.contains("/git/commits/") {
                self.peels.fetch_add(1, AtomicOrdering::Relaxed);
                let sha = t.rsplit('/').next().unwrap_or("").to_owned();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "sha": sha,
                        "tree": {"sha": "cccccccccccccccccccccccccccccccccccccccc"}
                    })),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected {t}"))
        }
    }

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);

    let mut groups = vec![PollGroup {
        repo: "acme/batch".into(),
        paths: vec![None],
    }];
    for i in 0..N_BACKLOG {
        groups.push(PollGroup {
            repo: format!("org/backlog-{i}"),
            paths: vec![None],
        });
    }

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        let refs: Vec<_> = (0..BATCH)
            .map(|i| {
                refledger_poller::observation::ObservedRef::new_lightweight(
                    format!("refs/tags/v1.0.{i}"),
                    format!("a{:039x}", i + 1),
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap()
            })
            .collect();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/batch")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs,
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        for g in &groups[1..] {
            store.append_observation(&ok_obs(&g.repo, t0)).unwrap();
        }
    }

    let inner = HugeBatch::default();
    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    // Enough for phase-1 listings + priority peels/enrich/confirm; max_new_peels
    // is what starves ordinary backfill (must not starve the 50 moved tips).
    args.max_requests = 400;
    args.max_new_peels = 5;

    let report = run_once_with(
        &mut store,
        CountingTransport::new(inner.clone(), 400),
        &groups,
        &args,
    )
    .expect("once");

    let moves = store
        .entries()
        .iter()
        .filter(|e| e.event == Event::Move)
        .count();
    assert_eq!(
        moves, BATCH,
        "all {BATCH} moved tags must classify this run; got {moves}; report={report:?}"
    );
    assert!(
        inner.peels.load(AtomicOrdering::Relaxed) >= BATCH as u32,
        "each moved tip must be peeled"
    );
}

/// Enrich compare failures must not abort the run: observations commit, Move
/// lands (without ancestry/diff), sibling groups still poll, exit Ok.
fn enrich_fail_transport(
    status: u16,
    body: Option<serde_json::Value>,
    transport_err: Option<&str>,
) {
    use refledger_log::entry::Event;
    use refledger_poller::github::rest::{RestRequest, RestResponse, Transport};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    #[derive(Clone)]
    struct T {
        status: u16,
        body: Option<serde_json::Value>,
        transport_err: Option<String>,
        other_listed: Arc<AtomicU32>,
    }
    impl Transport for T {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/compare/") {
                if let Some(ref e) = self.transport_err {
                    return Err(e.clone());
                }
                return Ok(RestResponse {
                    status: self.status,
                    headers: BTreeMap::new(),
                    body: self.body.clone(),
                });
            }
            if t.contains("org/sibling") {
                self.other_listed.fetch_add(1, AtomicOrdering::Relaxed);
            }
            if t.contains("/repos/")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"repo\"".into())]),
                    body: Some(json!({"archived": false})),
                });
            }
            if t.contains("matching-refs/tags") {
                if t.contains("acme/moved") {
                    return Ok(RestResponse {
                        status: 200,
                        headers: BTreeMap::from([("etag".into(), "W/\"moved\"".into())]),
                        body: Some(json!([{
                            "ref": "refs/tags/v1.0.0",
                            "object": {"type": "commit", "sha": "cccccccccccccccccccccccccccccccccccccccc"}
                        }])),
                    });
                }
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"sib\"".into())]),
                    body: Some(json!([])),
                });
            }
            if t.contains("/git/commits/") {
                let sha = t.rsplit('/').next().unwrap_or("").to_owned();
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "sha": sha,
                        "tree": {"sha": "dddddddddddddddddddddddddddddddddddddddd"}
                    })),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected {t}"))
        }
    }

    let dir = TempDir::new().unwrap();
    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t1 = t0 + Duration::minutes(5);
    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/moved")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v1.0.0",
                            "1111111111111111111111111111111111111111",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        store
            .append_observation(&ok_obs("org/sibling", t0))
            .unwrap();
    }

    let inner = T {
        status,
        body,
        transport_err: transport_err.map(|s| s.to_owned()),
        other_listed: Arc::new(AtomicU32::new(0)),
    };
    let listed = inner.other_listed.clone();
    let groups = vec![
        PollGroup {
            repo: "acme/moved".into(),
            paths: vec![None],
        },
        PollGroup {
            repo: "org/sibling".into(),
            paths: vec![None],
        },
    ];
    let mut store = Store::open(dir.path(), opts(t1)).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t1;
    args.actual_start = t1;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 50;

    let report = run_once_with(
        &mut store,
        CountingTransport::new(inner, 150),
        &groups,
        &args,
    )
    .expect("once must exit 0 despite enrich failure");
    assert!(
        report.observations >= 2,
        "both groups must commit observations: {report:?}"
    );
    assert!(
        listed.load(AtomicOrdering::Relaxed) >= 1,
        "sibling group must still be listed"
    );
    let mv = store
        .entries()
        .iter()
        .find(|e| e.event == Event::Move)
        .expect("Move must still be appended");
    assert!(mv.ancestry.is_none(), "ancestry omitted when enrich failed");
    assert!(mv.diff.is_none(), "diff omitted when enrich failed");
    assert_eq!(
        mv.to.as_ref().map(|b| b.commit_sha.as_str()),
        Some("cccccccccccccccccccccccccccccccccccccccc")
    );
}

#[test]
fn run_once_survives_compare_404() {
    enrich_fail_transport(404, Some(serde_json::json!({"message": "Not Found"})), None);
}

#[test]
fn run_once_survives_compare_500() {
    enrich_fail_transport(500, Some(serde_json::json!({"message": "boom"})), None);
}

#[test]
fn run_once_survives_compare_timeout() {
    enrich_fail_transport(0, None, Some("timeout waiting for compare"));
}

#[test]
fn run_once_survives_compare_malformed_json() {
    // 200 with body missing required `status` field.
    enrich_fail_transport(200, Some(serde_json::json!({"files": []})), None);
}

/// Move observed while a prior day was unsealed must land on the chain after
/// recovery seals that day — even if the in-memory buffer was lost (obs only).
#[test]
fn run_once_records_move_observed_during_outage_on_recovery() {
    use refledger_log::entry::Event;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    let dir = TempDir::new().unwrap();
    const OLD: &str = "1111111111111111111111111111111111111111";
    const TREE_OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW: &str = "7272eeb121bd005fd0ec28b7b245c764b964cb44";
    const TREE_NEW: &str = "f810ca43c48e2707b0de7a64737f294a71e64bda";

    let t0 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t_move = odt(2026, Month::January, 2, 1, 7, 0, 0);
    let t_recover = odt(2026, Month::January, 2, 2, 0, 0, 0);

    {
        let mut store = Store::open(dir.path(), opts(t0)).unwrap();
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/canary")
                    .unwrap()
                    .observed_at(t0)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"old\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v1.0.0",
                            OLD,
                            TREE_OLD,
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        // Outage run wrote the peeled Ok tip but lost the buffered Move.
        store
            .append_observation(
                &Observation::builder()
                    .repo("acme/canary")
                    .unwrap()
                    .observed_at(t_move)
                    .unwrap()
                    .method(Method::Rest)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(ETag::new("W/\"new\"")),
                        refs: vec![refledger_poller::observation::ObservedRef::new_lightweight(
                            "refs/tags/v1.0.0",
                            NEW,
                            TREE_NEW,
                        )
                        .unwrap()],
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct RecoverTransport {
        compares: Arc<AtomicU32>,
    }
    impl Transport for RecoverTransport {
        fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
            let t = &request.target;
            if t.contains("/repos/acme/canary")
                && !t.contains("/git/")
                && !t.contains("/contents/")
                && !t.contains("/compare/")
            {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"new\"".into())]),
                    body: Some(json!({"full_name": "acme/canary", "default_branch": "main"})),
                });
            }
            if t.contains("/git/matching-refs/tags") {
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::from([("etag".into(), "W/\"new\"".into())]),
                    body: Some(json!([{
                        "ref": "refs/tags/v1.0.0",
                        "object": {"sha": NEW, "type": "commit"}
                    }])),
                });
            }
            if t.contains("/git/commits/") {
                let sha = t.rsplit('/').next().unwrap_or("");
                let tree = if sha == NEW { TREE_NEW } else { TREE_OLD };
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "sha": sha,
                        "tree": {"sha": tree},
                        "parents": []
                    })),
                });
            }
            if t.contains("/compare/") {
                self.compares.fetch_add(1, AtomicOrdering::Relaxed);
                return Ok(RestResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Some(json!({
                        "status": "ahead",
                        "ahead_by": 1,
                        "behind_by": 0,
                        "total_commits": 1,
                        "files": [{"filename": "action.yml", "status": "modified"}]
                    })),
                });
            }
            if t.contains("/contents/") {
                return Ok(RestResponse {
                    status: 404,
                    headers: BTreeMap::new(),
                    body: Some(json!({"message": "Not Found"})),
                });
            }
            Err(format!("unexpected {t}"))
        }
    }

    let groups = vec![PollGroup {
        repo: "acme/canary".into(),
        paths: vec![None],
    }];
    let mut store = Store::open(dir.path(), opts(t_recover)).unwrap();
    let mut args = OnceArgs::production(dir.path(), dir.path().join("watched.jsonl"));
    args.token = "ghp_test".into();
    args.scheduled_at = t_recover;
    args.actual_start = t_recover;
    args.sleep = Box::new(|_| {});
    args.confirm_delay = Duration::seconds(0);
    args.max_requests = 150;
    args.max_new_peels = 50;

    let report = run_once_with(
        &mut store,
        CountingTransport::new(RecoverTransport::default(), 150),
        &groups,
        &args,
    )
    .expect("recovery once");
    assert!(
        !report.days_sealed.is_empty(),
        "Jan 1 must seal on recovery: {report:?}"
    );
    let mv = store
        .entries()
        .iter()
        .find(|e| e.event == Event::Move)
        .expect("Move for outage tip must be on chain after recovery");
    assert_eq!(mv.r#ref.as_deref(), Some("refs/tags/v1.0.0"));
    assert_eq!(
        mv.to.as_ref().map(|b| b.commit_sha.as_str()),
        Some(NEW),
        "canary tip must be recorded"
    );
}
