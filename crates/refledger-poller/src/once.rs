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

use crate::classify::{classify, Enrichment, RepoState};
use crate::derive::{derive, ChainTip};
use crate::enrich::{enrich, CompareCache};
use crate::github::etag::{AuthToken, ETagStore};
use crate::github::rest::{
    build_obs_for_once, peel_priority_refs, priority_peel_names, resolve_repo,
    resolve_repo_listing, resolve_repo_warm_up, Client, ObjectCache, PageBodyCache, RepoMetaCache,
    ResolveFail, RestRequest, RestResponse, Transport, WarmUpContext,
};
use crate::observation::{Observation, Outcome, RepoSlug, SkipReason, Timestamp};
use crate::population::{load_watched, poll_groups, PollGroup};
use crate::scheduler::{
    skip_observation, CONFIRM_DELAY, M1_INTERVAL, MAX_NEW_PEELS_PER_RUN, MAX_REQUESTS_PER_RUN,
};
use crate::store::{OsVolume, Store, StoreError, StoreOptions};
use refledger_log::entry::Diff;

/// Repo variable that must equal `true` before the Actions job runs.
pub const ENABLED_VAR: &str = "REFLEDGER_ENABLED";

/// True only when `value` is exactly `true`.
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
    #[error("classify: {0}")]
    Classify(String),
    #[error("enrich: {0}")]
    Enrich(String),
    #[error("derive: {0}")]
    Derive(String),
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
    pub derived_events: usize,
}

/// Counting transport wrapper: enforces the per-run request budget and tallies
/// 200 vs 304 responses. API reads use a stable `REFLEDGER_GITHUB_TOKEN` so
/// ETags can survive across runs; `GITHUB_TOKEN` is push-only.
pub struct CountingTransport<T: Transport> {
    inner: T,
    max: AtomicU32,
    requests: AtomicU32,
    status_200: AtomicU32,
    status_304: AtomicU32,
    conditional_requests: AtomicU32,
}

impl<T: Transport> CountingTransport<T> {
    pub fn new(inner: T, max: u32) -> Self {
        Self {
            inner,
            max: AtomicU32::new(max),
            requests: AtomicU32::new(0),
            status_200: AtomicU32::new(0),
            status_304: AtomicU32::new(0),
            conditional_requests: AtomicU32::new(0),
        }
    }

    /// Cap further sends at `max` (used to hold back confirm/enrich budget).
    pub fn set_max(&self, max: u32) {
        self.max.store(max, Ordering::Relaxed);
    }

    pub fn max(&self) -> u32 {
        self.max.load(Ordering::Relaxed)
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
        let max = self.max.load(Ordering::Relaxed);
        loop {
            let cur = self.requests.load(Ordering::Relaxed);
            if cur >= max {
                return Err(format!("request budget exhausted ({cur} >= {max})"));
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

/// Held after phase 1 so warm-up can replace listing stubs with real peels
/// before anything is appended for an Ok listing.
struct PendingOk {
    early: Observation,
    warm: WarmUpContext,
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
    let mut compare = CompareCache::open(data.join("compare.jsonl"))
        .map_err(|e| OnceError::Enrich(e.to_string()))?;

    let mut observations = 0usize;
    let mut derived_events = 0usize;
    let mut moved: Vec<String> = Vec::new();
    let mut pending_ok: Vec<PendingOk> = Vec::new();
    let mut warm_only: Vec<WarmUpContext> = Vec::new();
    // Repo states rebuilt lazily from the observation archive, then updated
    // as this run appends.
    let mut states: BTreeMap<String, RepoState> = BTreeMap::new();

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
            &objects,
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
                derived_events += process_observation(
                    store,
                    &mut states,
                    &mut compare,
                    client.transport(),
                    &args.token,
                    &obs,
                )?;
                continue;
            }
        };

        let Some(mut early) = pass.early_observation else {
            continue;
        };
        early.stamp_schedule(scheduled_ts, actual_ts);
        let warm = pass.warm;

        match early.outcome() {
            Outcome::Ok { .. } => {
                let prior_targets = store.latest_ok_targets(&g.repo)?;
                let moved_n = movement_target_count(&early, &prior_targets);
                if moved_n > 0 {
                    moved.push(g.repo.clone());
                }
                if let Some(w) = warm {
                    pending_ok.push(PendingOk { early, warm: w });
                } else {
                    // Empty-repo Ok: nothing to peel.
                    store.append_observation(&early)?;
                    observations += 1;
                    derived_events += process_observation(
                        store,
                        &mut states,
                        &mut compare,
                        client.transport(),
                        &args.token,
                        &early,
                    )?;
                }
            }
            _ => {
                // 304 / Failed / Skipped: append now. 304 may still warm cache.
                store.append_observation(&early)?;
                observations += 1;
                derived_events += process_observation(
                    store,
                    &mut states,
                    &mut compare,
                    client.transport(),
                    &args.token,
                    &early,
                )?;
                if let Some(w) = warm {
                    warm_only.push(w);
                }
            }
        }
    }

    // Reserve confirm + enrich + priority peels (changed/reappeared tips) before
    // phase-2 backfill takes anything. Priority peels are what make brand-new
    // attack commits classifiable in the same run.
    let mut priority_tip_count = 0usize;
    for pending in &pending_ok {
        let state = rebuild_repo_state(store, pending.warm.repo.as_str(), None)?;
        priority_tip_count += priority_peel_names(&state, &pending.warm.refs).len();
    }
    let reserve = reserved_budget(moved.len(), priority_tip_count);
    let hard_max = args.max_requests;
    let phase1_used = client.transport().requests();
    let phase2_ceiling = hard_max.saturating_sub(reserve).max(phase1_used);
    client.transport().set_max(phase2_ceiling);

    // Phase 2a: warm object cache for 304s (no new observation).
    for w in warm_only {
        if client.transport().requests() >= phase2_ceiling {
            break;
        }
        let _ = resolve_repo_warm_up(&w, &pages, &mut objects, &client);
    }

    // Phase 2b: optional backfill peels only (may leave changed tips listing-only
    // until priority peel below). Do not append yet.
    let mut pending_after_warm: Vec<PendingOk> = Vec::new();
    for pending in pending_ok {
        if client.transport().requests() < phase2_ceiling {
            let _ = resolve_repo_warm_up(&pending.warm, &pages, &mut objects, &client);
        }
        pending_after_warm.push(pending);
    }

    // Restore budget for priority peels + enrich, but hold confirm slots aside
    // so optional backfill cannot climb into the re-poll reserve.
    let confirm_reserve = (moved.len() as u32).saturating_mul(2);
    let peel_enrich_max = hard_max.saturating_sub(confirm_reserve);
    client.transport().set_max(peel_enrich_max);

    for pending in pending_after_warm {
        let repo_key = pending.warm.repo.as_str().to_owned();
        if !states.contains_key(&repo_key) {
            let rebuilt = rebuild_repo_state(store, &repo_key, None)?;
            states.insert(repo_key.clone(), rebuilt);
        }
        let state = states.get(&repo_key).expect("just inserted");
        let priority = priority_peel_names(state, &pending.warm.refs);

        let (http_status, etag, archived, at) = match pending.early.outcome() {
            Outcome::Ok {
                http_status, etag, ..
            } => (
                *http_status,
                etag.clone(),
                pending.early.archived(),
                pending.early.observed_at().as_offset_datetime(),
            ),
            _ => (
                200,
                None,
                pending.early.archived(),
                pending.early.observed_at().as_offset_datetime(),
            ),
        };

        let observed = if !priority.is_empty() {
            // Peel every changed/reappeared tip before classify — even never-seen commits.
            // Priority peels are mandatory: temporarily allow the full hard max
            // minus confirm so a large batch move still completes this run.
            client
                .transport()
                .set_max(peel_enrich_max.max(client.transport().requests()));
            let peeled = peel_priority_refs(&pending.warm, &priority, &mut objects, &client)
                .map_err(|e| OnceError::Rest(format!("priority peel: {e:?}")))?;
            client.transport().set_max(peel_enrich_max);
            peeled
        } else if client.transport().requests() < peel_enrich_max {
            match resolve_repo_warm_up(&pending.warm, &pages, &mut objects, &client) {
                Ok((refs, _)) => refs,
                Err(_) => {
                    peel_priority_refs(&pending.warm, &BTreeSet::new(), &mut objects, &client)
                        .unwrap_or_default()
                }
            }
        } else {
            peel_priority_refs(&pending.warm, &BTreeSet::new(), &mut objects, &client)
                .unwrap_or_default()
        };

        let etag = if observed
            .iter()
            .any(|r| r.commit_sha().is_none() && r.tree_sha().is_none())
        {
            // Incomplete peels remaining: do not claim a stable ETag.
            None
        } else {
            etag
        };

        let mut obs = build_obs_for_once(
            pending.warm.repo.as_str(),
            pending.warm.path.as_deref(),
            at,
            Outcome::Ok {
                http_status,
                etag,
                refs: observed,
            },
            archived,
        )
        .unwrap_or(pending.early);
        obs.stamp_schedule(scheduled_ts, actual_ts);
        store.append_observation(&obs)?;
        observations += 1;
        derived_events += process_observation(
            store,
            &mut states,
            &mut compare,
            client.transport(),
            &args.token,
            &obs,
        )?;
    }

    // Confirm re-polls get the held-back reserve.
    client.transport().set_max(hard_max);
    let confirmations = if !moved.is_empty() {
        (args.sleep)(args.confirm_delay);
        let confirm_at = actual_start + args.confirm_delay;
        let mut n = 0usize;
        for repo in &moved {
            // Confirm always runs when reserved; only stop if the hard max is hit.
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
            derived_events += process_observation(
                store,
                &mut states,
                &mut compare,
                client.transport(),
                &args.token,
                &obs,
            )?;
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
        derived_events,
    })
}

/// Requests held back from phase-2 backfill for confirm + enrich + priority peels.
fn reserved_budget(moved_repos: usize, priority_tips: usize) -> u32 {
    // Confirm resolve_repo: repo metadata + tag listing (often 304) ≈ 2.
    // Priority peel: git/commits (+ optional action.yml) ≈ 2 per tip.
    // Enrich: one compare per moved tip.
    let confirm = (moved_repos as u32).saturating_mul(2);
    let peel = (priority_tips as u32).saturating_mul(2);
    let enrich = priority_tips as u32;
    confirm.saturating_add(peel).saturating_add(enrich)
}

/// Shared observe → enrich → classify → derive → append path.
///
/// [`run_once_with`] calls this after every stored observation. Replay tests
/// must use this helper (not a hand-rolled classify/derive loop) so the
/// Actions path and the library path cannot diverge again.
pub fn classify_enrich_derive_append<T: Transport>(
    store: &mut Store<OsVolume>,
    state: &mut RepoState,
    compare: &mut CompareCache,
    transport: &T,
    token: &str,
    obs: &Observation,
) -> Result<usize, OnceError> {
    let enrichment = enrich(state, obs, compare, transport, token)
        .map_err(|e| OnceError::Enrich(e.to_string()))?;
    let (next, events) =
        classify(state, obs, &enrichment).map_err(|e| OnceError::Classify(e.to_string()))?;
    *state = next;
    if events.is_empty() {
        return Ok(0);
    }
    let mut tip = ChainTip::from_entries(
        store.entries(),
        obs.repo().as_str(),
        obs.observed_at().as_offset_datetime(),
        BTreeMap::new(),
    );
    tip.diffs = diffs_from_compare(compare);
    let derived = derive(&tip, &events).map_err(|e| OnceError::Derive(e.to_string()))?;
    let n = derived.len();
    for entry in derived {
        store.append_entry(entry)?;
    }
    Ok(n)
}

/// Classify → enrich → derive → append for one observation.
///
/// Caller must append the observation to the store first. Rebuild skips this
/// observation's id so state reflects only prior sweeps.
fn process_observation<T: Transport>(
    store: &mut Store<OsVolume>,
    states: &mut BTreeMap<String, RepoState>,
    compare: &mut CompareCache,
    transport: &T,
    token: &str,
    obs: &Observation,
) -> Result<usize, OnceError> {
    let repo = obs.repo().as_str().to_owned();
    if !states.contains_key(&repo) {
        let rebuilt = rebuild_repo_state(store, &repo, Some(obs.observation_id()))?;
        states.insert(repo.clone(), rebuilt);
    }
    let state = states.get_mut(&repo).expect("just inserted");
    classify_enrich_derive_append(store, state, compare, transport, token, obs)
}

fn diffs_from_compare(cache: &CompareCache) -> BTreeMap<(String, String), Diff> {
    let mut m = BTreeMap::new();
    for ((old, new), c) in cache.iter() {
        m.insert(
            (old.clone(), new.clone()),
            Diff {
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

/// Replay Ok observations for `repo` to rebuild live classification state.
fn rebuild_repo_state(
    store: &Store<OsVolume>,
    repo: &str,
    exclude: Option<ulid::Ulid>,
) -> Result<RepoState, OnceError> {
    let mut state = RepoState::default();
    for obs in store.ok_observations_chronological(repo)? {
        if exclude.is_some_and(|id| obs.observation_id() == id) {
            continue;
        }
        let (next, _) = classify(&state, &obs, &Enrichment::empty())
            .map_err(|e| OnceError::Classify(e.to_string()))?;
        state = next;
    }
    Ok(state)
}

/// Open the store, load watched groups, run one sweep with the live HTTP client.
pub fn run_once(opts: StoreOptions, args: OnceArgs) -> Result<OnceReport, OnceError> {
    let watched =
        load_watched(&args.watched_path).map_err(|e| OnceError::Population(e.to_string()))?;
    let groups = poll_groups(&watched);
    let mut store = Store::open(&args.data_dir, opts)?;
    // Queue the false-422 digest note before any seal of 2026-09-29.
    store.ensure_false_422_digest_note()?;
    store.ensure_gap_no_derive_digest_note()?;
    store.ensure_tree_sha_digest_note()?;
    store.ensure_seq_40_deletion_classification_correction(args.actual_start)?;
    // First durable chain rows: Added for every watched key that already has
    // an observation but no PopulationChange yet (including the canary and
    // subdirectory keys that share a poll group).
    store.emit_genesis_population_adds(&watched, args.actual_start)?;
    let transport = CountingTransport::new(UreqTransport, args.max_requests);
    run_once_with(&mut store, transport, &groups, &args)
}

fn movement_target_count(obs: &Observation, prior: &BTreeSet<String>) -> usize {
    if prior.is_empty() {
        return 0;
    }
    match obs.outcome() {
        Outcome::Ok { refs, .. } => {
            let mut prior_map: BTreeMap<&str, &str> = BTreeMap::new();
            for entry in prior {
                if let Some((name, sha)) = entry.split_once(':') {
                    prior_map.insert(name, sha);
                }
            }
            let mut n = 0usize;
            for r in refs {
                match prior_map.get(r.name()) {
                    Some(old) if *old != r.target_sha() => n += 1,
                    None => {} // creation: not a move
                    _ => {}
                }
            }
            // Deletions also count as movement for confirm.
            let current: BTreeSet<&str> = refs.iter().map(|r| r.name()).collect();
            for name in prior_map.keys() {
                if !current.contains(name) {
                    n += 1;
                }
            }
            n
        }
        _ => 0,
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

#[cfg(test)]
mod movement_tests {
    use super::*;

    #[test]
    fn reserve_covers_confirm_enrich_and_priority_peels() {
        // 1 repo, 1 tip: confirm=2 + peel=2 + enrich=1 = 5
        assert_eq!(reserved_budget(1, 1), 5);
        // 2 repos, 5 tips: confirm=4 + peel=10 + enrich=5 = 19
        assert_eq!(reserved_budget(2, 5), 19);
        assert_eq!(reserved_budget(0, 0), 0);
    }
}
