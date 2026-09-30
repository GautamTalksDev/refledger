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

use refledger_log::normalize_to_utc_millis;

use crate::github::etag::{AuthToken, ETagStore};
use crate::github::rest::{
    resolve_repo, resolve_repo_listing, resolve_repo_warm_up, Client, ObjectCache, PageBodyCache,
    RepoMetaCache, ResolveFail, RestRequest, RestResponse, Transport, WarmUpContext,
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
        let now = normalize_to_utc_millis(OffsetDateTime::now_utc());
        Self {
            data_dir: data_dir.into(),
            watched_path: watched_path.into(),
            token: String::new(),
            scheduled_at: now,
            actual_start: now,
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
    pub conditional_requests: u32,
    pub tip_seq: u64,
}

/// Counting transport wrapper: enforces the per-run request budget and tallies
/// 200 vs 304 responses. API reads use a stable `REFLEDGER_GITHUB_TOKEN` so
/// ETags can survive across runs; `GITHUB_TOKEN` is push-only.
pub struct CountingTransport<T: Transport> {
    inner: T,
    max: u32,
    requests: AtomicU32,
    status_200: AtomicU32,
    status_304: AtomicU32,
    conditional_requests: AtomicU32,
}

impl<T: Transport> CountingTransport<T> {
    pub fn new(inner: T, max: u32) -> Self {
        Self {
            inner,
            max,
            requests: AtomicU32::new(0),
            status_200: AtomicU32::new(0),
            status_304: AtomicU32::new(0),
            conditional_requests: AtomicU32::new(0),
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

    pub fn conditional_requests(&self) -> u32 {
        self.conditional_requests.load(Ordering::Relaxed)
    }
}

impl<T: Transport> Transport for CountingTransport<T> {
    fn send(&self, request: &RestRequest) -> Result<RestResponse, String> {
        // Claim a slot before sending. Never let the counter exceed `max`,
        // even when a concurrent claim races (compare-exchange loop).
        if request
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("if-none-match"))
        {
            self.conditional_requests.fetch_add(1, Ordering::Relaxed);
        }
        loop {
            let cur = self.requests.load(Ordering::Relaxed);
            if cur >= self.max {
                return Err(format!("request budget exhausted ({cur} >= {})", self.max));
            }
            if self
                .requests
                .compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        let resp = match self.inner.send(request) {
            Ok(r) => r,
            Err(e) => {
                // A failed send still consumed budget: the attempt happened.
                // Callers that need to distinguish budget-deny from transport
                // failure inspect the error string.
                return Err(e);
            }
        };
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
    let url = resp.get_url().to_owned();
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
            match serde_json::from_str(&text) {
                Ok(v) => Some(v),
                Err(e) if (400..600).contains(&status) => {
                    // Preserve HTTP status for classification; attach a truncated
                    // body so callers can log the failure cause.
                    eprintln!(
                        "github non-json body url={url} status={status} parse={e} body={}",
                        truncate_chars(&text, 512)
                    );
                    Some(serde_json::json!({ "_non_json_body": truncate_chars(&text, 512) }))
                }
                Err(e) => {
                    return Err(format!(
                        "GET {url} status={status} body parse: {e}; body={}",
                        truncate_chars(&text, 512)
                    ));
                }
            }
        }
    };
    if !(200..300).contains(&status) && status != 304 {
        let snip = body
            .as_ref()
            .map(|v| truncate_chars(&v.to_string(), 512))
            .unwrap_or_default();
        eprintln!("github response url={url} status={status} body={snip}");
    }
    Ok(RestResponse {
        status,
        headers,
        body,
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Run one full Actions sweep against an injectable transport (tests).
pub fn run_once_with<T: Transport>(
    store: &mut Store<OsVolume>,
    transport: CountingTransport<T>,
    groups: &[PollGroup],
    args: &OnceArgs,
) -> Result<OnceReport, OnceError> {
    // Boundary: callers may pass a raw clock or env time; never let those
    // reach Timestamp / observation builders unnormalised.
    let scheduled_at = normalize_to_utc_millis(args.scheduled_at);
    let actual_start = normalize_to_utc_millis(args.actual_start);
    let scheduled_ts = Timestamp::from_offset_datetime(scheduled_at)
        .map_err(|e| OnceError::Observation(e.to_string()))?;
    let actual_ts = Timestamp::from_offset_datetime(actual_start)
        .map_err(|e| OnceError::Observation(e.to_string()))?;

    let gaps = store.record_schedule_gaps(groups, M1_INTERVAL, scheduled_at, actual_start)?;
    // Retry any pending ledger publish before the sweep so a failed seal
    // reaches main on the next poll, not 24 hours later.
    let _ = store.retry_pending_publishes()?;
    let days_sealed = store.seal_missed_days_before(actual_start)?;

    let token = AuthToken::new(&args.token).map_err(|e| OnceError::Message(e.to_string()))?;
    let client = Client::new(transport, token);
    let data = &args.data_dir;
    let mut etags = ETagStore::open(data.join("etag.jsonl"), Duration::hours(48))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    let mut pages = PageBodyCache::open(data.join("page_bodies.jsonl"))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    let mut repo_meta = RepoMetaCache::open(data.join("repo_meta.jsonl"))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    let mut objects = ObjectCache::open(data.join("objects.jsonl"))
        .map_err(|e| OnceError::Rest(e.to_string()))?;
    objects.set_peel_budget(args.max_new_peels);

    let mut observations = 0usize;
    let mut moved: Vec<String> = Vec::new();
    let mut warm_jobs: Vec<WarmUpContext> = Vec::new();

    // Phase 1: every poll group gets a conditional listing (and repo metadata)
    // before any peel or action.yml work consumes the shared budget.
    for g in groups {
        let slug = RepoSlug::parse(&g.repo).map_err(|e| OnceError::Observation(e.to_string()))?;
        let path = g.paths.first().and_then(|p| p.clone());
        let pass = match resolve_repo_listing(
            &slug,
            path.as_deref(),
            &mut etags,
            &mut pages,
            &mut repo_meta,
            &client,
            actual_start,
        ) {
            Ok(p) => p,
            Err(e) => {
                let obs = match e {
                    ResolveFail::BudgetExhausted => {
                        skip_observation(&g.repo, actual_start, SkipReason::BudgetExhausted)
                            .map_err(|e| OnceError::Observation(e.to_string()))?
                    }
                    _ => {
                        return Err(OnceError::Rest(format!("listing: {e:?}")));
                    }
                };
                let mut obs = obs;
                obs.stamp_schedule(scheduled_ts, actual_ts);
                store.append_observation(&obs)?;
                observations += 1;
                continue;
            }
        };
        if let Some(obs) = pass.early_observation {
            let prior_targets = store.latest_ok_targets(&g.repo)?;
            let mut obs = obs;
            obs.stamp_schedule(scheduled_ts, actual_ts);
            if movement_detected(&obs, &prior_targets) {
                moved.push(g.repo.clone());
            }
            store.append_observation(&obs)?;
            observations += 1;
        }
        if let Some(w) = pass.warm {
            warm_jobs.push(w);
        }
    }

    // Phase 2: warm-up peels and action.yml fetches with whatever budget remains.
    for w in warm_jobs {
        if let Err(e) = resolve_repo_warm_up(&w, &pages, &mut objects, &client) {
            if !matches!(e, ResolveFail::BudgetExhausted) {
                return Err(OnceError::Rest(format!("warm-up: {e:?}")));
            }
        }
    }

    let confirmations = if !moved.is_empty() {
        (args.sleep)(args.confirm_delay);
        let confirm_at = actual_start + args.confirm_delay;
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
                &mut repo_meta,
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
        conditional_requests: client.transport().conditional_requests(),
        tip_seq: store.tip_seq(),
    })
}

/// Open the store, load watched groups, run one sweep with the live HTTP client.
pub fn run_once(opts: StoreOptions, args: OnceArgs) -> Result<OnceReport, OnceError> {
    let watched =
        load_watched(&args.watched_path).map_err(|e| OnceError::Population(e.to_string()))?;
    let groups = poll_groups(&watched);
    let mut store = Store::open(&args.data_dir, opts)?;
    // Queue the false-422 digest note before any seal of 2026-09-29.
    store.ensure_false_422_digest_note()?;
    // First durable chain rows: Added for every watched key that already has
    // an observation but no PopulationChange yet (including the canary and
    // subdirectory keys that share a poll group).
    store.emit_genesis_population_adds(&watched, args.actual_start)?;
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
            return normalize_to_utc_millis(t);
        }
    }
    normalize_to_utc_millis(infer_scheduled_slot(actual))
}

/// Floor `actual` to the preceding cron slot at :02, :07, :12, … :57.
pub fn infer_scheduled_slot(actual: OffsetDateTime) -> OffsetDateTime {
    let actual = normalize_to_utc_millis(actual);
    let minute = actual.minute();
    if minute < 2 {
        let prev = actual - Duration::minutes(i64::from(minute) + 3);
        return normalize_to_utc_millis(
            prev.replace_second(0)
                .and_then(|t| t.replace_nanosecond(0))
                .unwrap_or(prev),
        );
    }
    let offset = (minute - 2) % 5;
    let floored = minute - offset;
    normalize_to_utc_millis(
        actual
            .replace_minute(floored)
            .and_then(|t| t.replace_second(0))
            .and_then(|t| t.replace_nanosecond(0))
            .unwrap_or(actual),
    )
}
