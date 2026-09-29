//! One-shot poller run for GitHub Actions.
//!
//! Each scheduled workflow invocation opens the store, records schedule gaps,
//! seals missed UTC days, runs one full sweep, confirms movements after ~60s,
//! and exits. There is no long-lived loop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

use thiserror::Error;
use time::{Date, Duration, OffsetDateTime};

use crate::github::etag::{AuthToken, ETagStore};
use crate::github::rest::{
    resolve_repo, Client, ObjectCache, PageBodyCache, RestRequest, RestResponse, Transport,
};
use crate::observation::{Observation, Outcome, RepoSlug, SkipReason, Timestamp};
use crate::population::{load_watched, poll_groups, PollGroup};
use crate::scheduler::{
    skip_observation, CONFIRM_DELAY, M1_INTERVAL, MAX_NEW_PEELS_PER_RUN, MAX_REQUESTS_PER_RUN,
};
use crate::store::{OsVolume, Store, StoreError, StoreOptions};

/// Repo variable that must equal `true` before the Actions job runs.
pub const ENABLED_VAR: &str = "REFLEDGER_ENABLED";

/// True only when the repo variable is exactly `true`.
pub fn poller_enabled(value: Option<&str>) -> bool {
    value == Some("true")
}

#[derive(Debug, Error)]
pub enum OnceError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("population: {0}")]
    Population(String),
    #[error("observation: {0}")]
    Observation(String),
    #[error("rest: {0}")]
    Rest(String),
    #[error("{0}")]
    Message(String),
}

/// Inputs for one Actions sweep.
pub struct OnceArgs {
    pub data_dir: PathBuf,
    pub watched_path: PathBuf,
    pub token: String,
    pub scheduled_at: OffsetDateTime,
    pub actual_start: OffsetDateTime,
    /// Injected sleep for confirmation delay (real runs sleep; tests no-op).
    pub sleep: Box<dyn Fn(Duration) + Send>,
    pub max_requests: u32,
    pub max_new_peels: u32,
    pub confirm_delay: Duration,
}

impl OnceArgs {
    pub fn production(data_dir: impl Into<PathBuf>, watched_path: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            watched_path: watched_path.into(),
            token: String::new(),
            scheduled_at: OffsetDateTime::now_utc(),
            actual_start: OffsetDateTime::now_utc(),
            sleep: Box::new(|d| {
                let secs = d.whole_seconds().max(0) as u64;
                let nanos = d.subsec_nanoseconds().max(0) as u32;
                thread::sleep(StdDuration::new(secs, nanos));
            }),
            max_requests: MAX_REQUESTS_PER_RUN,
            max_new_peels: MAX_NEW_PEELS_PER_RUN,
            confirm_delay: CONFIRM_DELAY,
        }
    }
}

/// Summary printed / logged at end of run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnceReport {
    pub gaps: usize,
    pub days_sealed: Vec<Date>,
    pub observations: usize,
    pub confirmations: usize,
    pub requests: u32,
    pub status_200: u32,
    pub status_304: u32,
    pub tip_seq: u64,
}

/// Counting transport wrapper: enforces the per-run request budget and tallies
/// 200 vs 304 responses (GITHUB_TOKEN rotates every run; ETags may not survive).
pub struct CountingTransport<T: Transport> {
    inner: T,
    max: u32,
    requests: AtomicU32,
    status_200: AtomicU32,
    status_304: AtomicU32,
}

impl<T: Transport> CountingTransport<T> {
    pub fn new(inner: T, max: u32) -> Self {
        Self {
            inner,
            max,
            requests: AtomicU32::new(0),
            status_200: AtomicU32::new(0),
            status_304: AtomicU32::new(0),
        }
    }

    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn status_200(&self) -> u32 {
        self.status_200.load(Ordering::Relaxed)
    }

    pub fn status_304(&self) -> u32 {
        self.status_304.load(Ordering::Relaxed)
    }
}

impl<T: Transport> Transport for CountingTransport<T> {
    fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if n > self.max {
            return Err(format!("request budget exhausted ({n} > {})", self.max));
        }
        let resp = self.inner.send(request)?;
        match resp.status {
            200 => {
                self.status_200.fetch_add(1, Ordering::Relaxed);
            }
            304 => {
                self.status_304.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        Ok(resp)
    }
}

/// Production HTTPS transport. Redirects surface as status responses.
pub struct UreqTransport;

impl Transport for UreqTransport {
    fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
        let url = if request.target.starts_with("http") {
            request.target.clone()
        } else {
            format!("https://api.github.com{}", request.target)
        };
        let mut builder = ureq::get(&url);
        for (k, v) in &request.headers {
            builder = builder.set(k, v);
        }
        let resp = match builder.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                return read_response(code, r);
            }
            Err(e) => return Err(e.to_string()),
        };
        read_response(resp.status(), resp)
    }
}

fn read_response(status: u16, resp: ureq::Response) -> Result<RestResponse, String> {
    let mut headers = BTreeMap::new();
    for name in [
        "etag",
        "location",
        "retry-after",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
        "link",
    ] {
        if let Some(v) = resp.header(name) {
            headers.insert(name.to_owned(), v.to_owned());
        }
    }
    let body = if status == 304 {
        None
    } else {
        let text = resp.into_string().map_err(|e| e.to_string())?;
        if text.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&text).map_err(|e| e.to_string())?)
        }
    };
    Ok(RestResponse {
        status,
        headers,
        body,
    })
}

/// Run one full Actions sweep against an injectable transport (tests).
pub fn run_once_with<T: Transport>(
    store: &mut Store<OsVolume>,
    transport: CountingTransport<T>,
    groups: &[PollGroup],
    args: &OnceArgs,
) -> Result<OnceReport, OnceError> {
    let scheduled_ts = Timestamp::from_offset_datetime(args.scheduled_at)
        .map_err(|e| OnceError::Observation(e.to_string()))?;
    let actual_ts = Timestamp::from_offset_datetime(args.actual_start)
        .map_err(|e| OnceError::Observation(e.to_string()))?;

    let gaps = store.record_schedule_gaps(
        groups,
        M1_INTERVAL,
        args.scheduled_at,
        args.actual_start,
    )?;
    let days_sealed = store.seal_missed_days_before(args.actual_start)?;

    let token = AuthToken::new(&args.token).map_err(|e| OnceError::Message(e.to_string()))?;
    let client = Client::new(transport, token);
    let data = &args.data_dir;
    let mut etags = ETagStore::open(data.join("etag.jsonl"), Duration::hours(48))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    let mut pages = PageBodyCache::open(data.join("page_bodies.jsonl"))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    let mut objects = ObjectCache::open(data.join("objects.jsonl"))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    objects.set_peel_budget(args.max_new_peels);

    let mut observations = 0usize;
    let mut moved: Vec<String> = Vec::new();

    for g in groups {
        if client.transport().requests() >= args.max_requests {
            let mut obs = skip_observation(&g.repo, args.actual_start, SkipReason::BudgetExhausted)
                .map_err(|e| OnceError::Observation(e.to_string()))?;
            obs.stamp_schedule(scheduled_ts, actual_ts);
            store.append_observation(&obs)?;
            observations += 1;
            continue;
        }
        let slug = RepoSlug::parse(&g.repo).map_err(|e| OnceError::Observation(e.to_string()))?;
        let path = g.paths.first().and_then(|p| p.clone());
        let prior_targets = store.latest_ok_targets(&g.repo)?;
        let mut obs = resolve_repo(
            &slug,
            path.as_deref(),
            &mut etags,
            &mut pages,
            &mut objects,
            &client,
            args.actual_start,
        );
        obs.stamp_schedule(scheduled_ts, actual_ts);
        if movement_detected(&obs, &prior_targets) {
            moved.push(g.repo.clone());
        }
        store.append_observation(&obs)?;
        observations += 1;
    }

    let confirmations = if !moved.is_empty() {
        (args.sleep)(args.confirm_delay);
        let confirm_at = args.actual_start + args.confirm_delay;
        let mut n = 0usize;
        for repo in &moved {
            if client.transport().requests() >= args.max_requests {
                break;
            }
            let slug = RepoSlug::parse(repo).map_err(|e| OnceError::Observation(e.to_string()))?;
            let mut obs = resolve_repo(
                &slug,
                None,
                &mut etags,
                &mut pages,
                &mut objects,
                &client,
                confirm_at,
            );
            obs.stamp_schedule(scheduled_ts, actual_ts);
            store.append_observation(&obs)?;
            observations += 1;
            n += 1;
        }
        n
    } else {
        0
    };

    Ok(OnceReport {
        gaps: gaps.len(),
        days_sealed,
        observations,
        confirmations,
        requests: client.transport().requests(),
        status_200: client.transport().status_200(),
        status_304: client.transport().status_304(),
        tip_seq: store.tip_seq(),
    })
}

/// Open the store, load watched groups, run one sweep with the live HTTP client.
pub fn run_once(opts: StoreOptions, args: OnceArgs) -> Result<OnceReport, OnceError> {
    let watched =
        load_watched(&args.watched_path).map_err(|e| OnceError::Population(e.to_string()))?;
    let groups = poll_groups(&watched);
    let mut store = Store::open(&args.data_dir, opts)?;
    let transport = CountingTransport::new(UreqTransport, args.max_requests);
    run_once_with(&mut store, transport, &groups, &args)
}

fn movement_detected(obs: &Observation, prior: &BTreeSet<String>) -> bool {
    if prior.is_empty() {
        return false;
    }
    match obs.outcome() {
        Outcome::Ok { refs, .. } => {
            let mut current = BTreeSet::new();
            for r in refs {
                current.insert(format!("{}:{}", r.name(), r.target_sha()));
            }
            current != *prior
        }
        _ => false,
    }
}

/// Prefer `REFLEDGER_SCHEDULED_AT`, else floor `actual` to the cron slot.
pub fn scheduled_time_from_env(actual: OffsetDateTime) -> OffsetDateTime {
    if let Ok(s) = std::env::var("REFLEDGER_SCHEDULED_AT") {
        if let Ok(t) = OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339) {
            return t;
        }
    }
    infer_scheduled_slot(actual)
}

/// Floor `actual` to the preceding cron slot at :02, :07, :12, … :57.
pub fn infer_scheduled_slot(actual: OffsetDateTime) -> OffsetDateTime {
    let minute = actual.minute();
    if minute < 2 {
        let prev = actual - Duration::minutes(i64::from(minute) + 3);
        return prev
            .replace_second(0)
            .and_then(|t| t.replace_nanosecond(0))
            .unwrap_or(prev);
    }
    let offset = (minute - 2) % 5;
    let floored = minute - offset;
    actual
        .replace_minute(floored)
        .and_then(|t| t.replace_second(0))
        .and_then(|t| t.replace_nanosecond(0))
        .unwrap_or(actual)
}
