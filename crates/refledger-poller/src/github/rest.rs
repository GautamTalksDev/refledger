//! Tag resolution against GitHub's REST API.
//!
//! # Immutability
//!
//! Git objects are content-addressed. A tag object's SHA always peels to the
//! same commit; a commit's SHA always has the same tree. Only the *ref →
//! target* binding moves. [`ObjectCache`] therefore maps object SHA → resolved
//! `(commit, tree)` with **no TTL and no invalidation**. It is a fsynced
//! append-only JSONL journal and **only ever grows**. An expiry would only
//! reintroduce cost.
//!
//! # EMPTY vs MISSING
//!
//! Live check (2026-09-28): `GET /git/refs/tags` returns 404 both for a
//! tagless public repo (`octocat/Spoon-Knife`) and for a nonexistent repo.
//! `GET /git/matching-refs/tags` returns `200 []` for the tagless repo and
//! `404` for the missing repo. Listing therefore uses matching-refs.
//!
//! # Subdirectory actions
//!
//! `owner/repo/path@ref` keeps metadata at `<path>/action.yml`.
//! [`resolve_repo`] takes an optional `path` and records it on the observation.
//! **Population must key on `(repo, path)`, not repo alone** — do not retrofit
//! that later.
//!
//! # Redirects
//!
//! A 301 (renamed or transferred repo) is recorded on the observation and
//! **not** followed. Quietly following transfers launders a change of ownership
//! into continuity.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use crate::github::etag::{AuthToken, ConditionalRequest, ETagStore, EndpointKey};
use crate::observation::{
    error_class_for_http, ETag, ErrorClass, Method, Observation, ObservationError, ObservedRef,
    Outcome, PeeledType, RefType, RepoSlug,
};

/// Maximum number of annotated-tag objects followed while peeling to a commit.
pub const MAX_TAG_DEPTH: u32 = 8;

const REPO_TEMPLATE: &str = "/repos/{owner}/{repo}";
const TAGS_TEMPLATE: &str = "/repos/{owner}/{repo}/git/matching-refs/tags";
const PER_PAGE: u32 = 100;
const REPO_META_QUERY: &str = "";

/// One outbound REST request as the resolver sees it.
///
/// `Debug` redacts `authorization` (and any header whose name matches) so a
/// failed-send `{:?}` log cannot leak a bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct RestRequest {
    pub method: &'static str,
    pub target: String,
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for RestRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut headers = BTreeMap::new();
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("authorization") {
                headers.insert(k.clone(), "Bearer [redacted]".to_owned());
            } else {
                headers.insert(k.clone(), v.clone());
            }
        }
        f.debug_struct("RestRequest")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("headers", &headers)
            .finish()
    }
}

/// One REST response. `body` is `None` on 304 and similar.
#[derive(Debug, Clone, PartialEq)]
pub struct RestResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
}

impl RestResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Pluggable HTTP layer. Production wraps a real client with redirects disabled;
/// tests replay fixtures.
pub trait Transport {
    fn send(&self, request: &RestRequest) -> Result<RestResponse, String>;
}

/// Authenticated client over a [`Transport`]. Never follows redirects.
#[derive(Debug, Clone)]
pub struct Client<T: Transport> {
    transport: T,
    token: AuthToken,
    user_agent: String,
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T, token: AuthToken) -> Self {
        Self {
            transport,
            token,
            user_agent: crate::identity::user_agent(),
        }
    }

    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn token(&self) -> &AuthToken {
        &self.token
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    fn send_raw(
        &self,
        target: String,
        extra_headers: BTreeMap<String, String>,
    ) -> Result<RestResponse, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "authorization".into(),
            format!("Bearer {}", self.token.as_str()),
        );
        headers.insert("accept".into(), "application/vnd.github+json".into());
        headers.insert("x-github-api-version".into(), "2022-11-28".into());
        headers.insert("user-agent".into(), self.user_agent.clone());
        for (k, v) in extra_headers {
            headers.insert(k, v);
        }
        self.transport.send(&RestRequest {
            method: "GET",
            target,
            headers,
        })
    }
}

/// Last successful body per paginated endpoint. Needed so a 304 on page N can
/// still contribute its refs when another page returns 200.
#[derive(Debug)]
pub struct PageBodyCache {
    path: PathBuf,
    entries: BTreeMap<PageKey, Vec<RawRef>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PageKey {
    repo: String,
    template: String,
    query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageRecord {
    repo: String,
    template: String,
    query: String,
    refs: Vec<RawRef>,
}

impl PageBodyCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RestError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| RestError::Io(e.to_string()))?;
            }
        }
        let mut cache = Self {
            path,
            entries: BTreeMap::new(),
        };
        cache.load()?;
        Ok(cache)
    }

    fn get(&self, repo: &RepoSlug, endpoint: &EndpointKey) -> Option<&[RawRef]> {
        self.entries
            .get(&PageKey {
                repo: repo.as_str().to_owned(),
                template: endpoint.template().to_owned(),
                query: endpoint.query().to_owned(),
            })
            .map(|v| v.as_slice())
    }

    /// Merge every cached tag-listing page for `repo` (detection / warm-up input).
    pub(crate) fn merged_tag_refs(&self, repo: &RepoSlug) -> Vec<RawRef> {
        let mut pages: Vec<_> = self
            .entries
            .iter()
            .filter(|(k, _)| k.repo == repo.as_str() && k.template == TAGS_TEMPLATE)
            .collect();
        pages.sort_by(|(a, _), (b, _)| a.query.cmp(&b.query));
        let mut out = Vec::new();
        for (_, refs) in pages {
            out.extend(refs.iter().cloned());
        }
        out
    }

    fn put(
        &mut self,
        repo: &RepoSlug,
        endpoint: &EndpointKey,
        refs: Vec<RawRef>,
    ) -> Result<(), RestError> {
        let record = PageRecord {
            repo: repo.as_str().to_owned(),
            template: endpoint.template().to_owned(),
            query: endpoint.query().to_owned(),
            refs: refs.clone(),
        };
        self.append(&record)?;
        self.entries.insert(
            PageKey {
                repo: record.repo,
                template: record.template,
                query: record.query,
            },
            refs,
        );
        Ok(())
    }

    fn load(&mut self) -> Result<(), RestError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(RestError::Io(err.to_string())),
        };
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: PageRecord = serde_json::from_str(line)
                .map_err(|e| RestError::Corrupt(format!("page cache line {}: {e}", idx + 1)))?;
            self.entries.insert(
                PageKey {
                    repo: record.repo,
                    template: record.template,
                    query: record.query,
                },
                record.refs,
            );
        }
        Ok(())
    }

    fn append(&self, record: &PageRecord) -> Result<(), RestError> {
        let mut line = serde_json::to_vec(record).map_err(|e| RestError::Serde(e.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.write_all(&line)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| RestError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Last successful repository metadata body fields needed after a 304.
#[derive(Debug)]
pub struct RepoMetaCache {
    path: PathBuf,
    entries: BTreeMap<String, RepoMetaBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RepoMetaBody {
    archived: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoMetaRecord {
    repo: String,
    archived: Option<bool>,
}

impl RepoMetaCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RestError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| RestError::Io(e.to_string()))?;
            }
        }
        let mut cache = Self {
            path,
            entries: BTreeMap::new(),
        };
        cache.load()?;
        Ok(cache)
    }

    fn get(&self, repo: &RepoSlug) -> Option<&RepoMetaBody> {
        self.entries.get(repo.as_str())
    }

    fn put(&mut self, repo: &RepoSlug, archived: Option<bool>) -> Result<(), RestError> {
        let record = RepoMetaRecord {
            repo: repo.as_str().to_owned(),
            archived,
        };
        self.append(&record)?;
        self.entries
            .insert(record.repo.clone(), RepoMetaBody { archived });
        Ok(())
    }

    fn load(&mut self) -> Result<(), RestError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(RestError::Io(err.to_string())),
        };
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: RepoMetaRecord = serde_json::from_str(line).map_err(|e| {
                RestError::Corrupt(format!("repo meta cache line {}: {e}", idx + 1))
            })?;
            self.entries.insert(
                record.repo,
                RepoMetaBody {
                    archived: record.archived,
                },
            );
        }
        Ok(())
    }

    fn append(&self, record: &RepoMetaRecord) -> Result<(), RestError> {
        let mut line = serde_json::to_vec(record).map_err(|e| RestError::Serde(e.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.write_all(&line)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| RestError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Content-addressed peel cache. Append-only; never expires; never invalidates.
#[derive(Debug)]
pub struct ObjectCache {
    path: PathBuf,
    entries: BTreeMap<String, CacheEntry>,
    /// Cap on first-seen peels this process (warm-up spread across runs).
    peel_budget: Option<u32>,
    /// New peels performed since open (cache misses that hit the network).
    new_peels: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CacheEntry {
    /// Peel of a tag or commit object to a commit + tree.
    Commit {
        commit_sha: String,
        tree_sha: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action_yml_sha: Option<Option<String>>,
    },
    /// Peel ended at a non-commit object.
    NonCommit {
        object_type: String,
        object_sha: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ObjectRecord {
    sha: String,
    #[serde(flatten)]
    entry: CacheEntry,
}

impl ObjectCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RestError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| RestError::Io(e.to_string()))?;
            }
        }
        let mut cache = Self {
            path,
            entries: BTreeMap::new(),
            peel_budget: None,
            new_peels: 0,
        };
        cache.load()?;
        Ok(cache)
    }

    /// Limit newly dereferenced objects this run (0 = no new peels).
    pub fn set_peel_budget(&mut self, budget: u32) {
        self.peel_budget = Some(budget);
        self.new_peels = 0;
    }

    pub fn new_peels(&self) -> u32 {
        self.new_peels
    }

    /// Returns false when a new network peel would exceed the budget.
    pub fn try_consume_peel_budget(&mut self) -> bool {
        if let Some(budget) = self.peel_budget {
            if self.new_peels >= budget {
                return false;
            }
        }
        self.new_peels += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, sha: &str) -> bool {
        self.entries.contains_key(sha)
    }

    /// Tree SHA previously recorded for `commit_sha`, if any.
    pub fn tree_sha_for_commit(&self, commit_sha: &str) -> Option<&str> {
        match self.entries.get(commit_sha) {
            Some(CacheEntry::Commit { tree_sha, .. }) => Some(tree_sha.as_str()),
            _ => None,
        }
    }

    /// Intentionally absent: content-addressed peels do not go stale.
    ///
    /// Tests assert this returns false so a future TTL/invalidate API cannot
    /// land quietly and reintroduce per-sweep dereference cost.
    pub fn has_ttl_or_invalidation_api(&self) -> bool {
        false
    }

    fn get(&self, sha: &str) -> Option<&CacheEntry> {
        self.entries.get(sha)
    }

    fn put(&mut self, sha: &str, entry: CacheEntry) -> Result<(), RestError> {
        if let (
            Some(CacheEntry::Commit {
                tree_sha: prior, ..
            }),
            CacheEntry::Commit {
                tree_sha: next,
                commit_sha,
                ..
            },
        ) = (self.entries.get(sha), &entry)
        {
            if prior != next {
                return Err(RestError::Corrupt(format!(
                    "commit {commit_sha} tree is immutable: cache has {prior}, refusing {next}"
                )));
            }
        }
        let record = ObjectRecord {
            sha: sha.to_owned(),
            entry: entry.clone(),
        };
        self.append(&record)?;
        self.entries.insert(sha.to_owned(), entry);
        Ok(())
    }

    fn set_action_yml(&mut self, commit_sha: &str, action: Option<&str>) -> Result<(), RestError> {
        let tree_sha = match self.entries.get(commit_sha) {
            Some(CacheEntry::Commit { tree_sha, .. }) => tree_sha.clone(),
            _ => {
                return Err(RestError::Protocol(format!(
                    "action.yml cache requires prior commit peel for {commit_sha}"
                )))
            }
        };
        self.put(
            commit_sha,
            CacheEntry::Commit {
                commit_sha: commit_sha.to_owned(),
                tree_sha,
                action_yml_sha: Some(action.map(|s| s.to_owned())),
            },
        )
    }

    fn load(&mut self) -> Result<(), RestError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(RestError::Io(err.to_string())),
        };
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: ObjectRecord = serde_json::from_str(line)
                .map_err(|e| RestError::Corrupt(format!("object cache line {}: {e}", idx + 1)))?;
            self.entries.insert(record.sha, record.entry);
        }
        Ok(())
    }

    fn append(&self, record: &ObjectRecord) -> Result<(), RestError> {
        let mut line = serde_json::to_vec(record).map_err(|e| RestError::Serde(e.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.write_all(&line)
            .map_err(|e| RestError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| RestError::Io(e.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RestError {
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
    #[error("corrupt store: {0}")]
    Corrupt(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("observation: {0}")]
    Observation(String),
    #[error("etag: {0}")]
    Etag(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RawRef {
    name: String,
    object_type: String,
    object_sha: String,
}

#[derive(Debug, Clone)]
struct PeelResult {
    ref_type: RefType,
    target_sha: String,
    peeled: Peeled,
}

#[derive(Debug, Clone)]
enum Peeled {
    Commit {
        commit_sha: String,
        tree_sha: String,
    },
    NonCommit {
        object_type: PeeledType,
        object_sha: String,
    },
}

/// Phase-1 output: an observation that is already complete, and/or warm-up work
/// deferred to phase 2 (peels and action.yml only).
#[derive(Debug)]
pub(crate) struct ListingPass {
    /// Tag-movement state from listing alone (304, failures, empty repo, …).
    pub early_observation: Option<Observation>,
    pub warm: Option<WarmUpContext>,
}

/// Inputs for phase-2 warm-up (never repeats listing or repo metadata).
#[derive(Debug, Clone)]
pub(crate) struct WarmUpContext {
    pub repo: RepoSlug,
    pub path: Option<String>,
    pub refs: Vec<RawRef>,
}

/// Resolve one repository (and optional subdirectory action path) into an
/// [`Observation`]. Failures are `Outcome::Failed`, not panics.
#[allow(clippy::too_many_arguments)]
pub fn resolve_repo<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    repo_meta: &mut RepoMetaCache,
    objects: &mut ObjectCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Observation {
    let now = refledger_log::normalize_to_utc_millis(now);
    match resolve_repo_listing(repo, path, etags, pages, repo_meta, objects, client, now) {
        Ok(pass) => {
            let early = pass.early_observation;
            if let Some(w) = pass.warm {
                match resolve_repo_warm_up(&w, pages, objects, client) {
                    Ok((observed, peel_budget_hit)) => {
                        if let Some(obs) = early {
                            if let Outcome::Ok {
                                http_status, etag, ..
                            } = obs.outcome()
                            {
                                let etag = if peel_budget_hit { None } else { etag.clone() };
                                return build_obs(
                                    repo,
                                    path,
                                    now,
                                    Outcome::Ok {
                                        http_status: *http_status,
                                        etag,
                                        refs: observed,
                                    },
                                    None,
                                    None,
                                    obs.archived(),
                                )
                                .unwrap_or(obs);
                            }
                            return obs;
                        }
                    }
                    Err(ResolveFail::BudgetExhausted) => {
                        if let Some(obs) = early {
                            return obs;
                        }
                        return skip_budget(repo, path, now);
                    }
                    Err(e) => return failed_observation(repo, path, now, e),
                }
            }
            early.unwrap_or_else(|| skip_budget(repo, path, now))
        }
        Err(ResolveFail::BudgetExhausted) => skip_budget(repo, path, now),
        Err(err) => failed_observation(repo, path, now, err),
    }
}

/// Phase 1 only: repository metadata (conditional) and tag listing (conditional).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_repo_listing<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    repo_meta: &mut RepoMetaCache,
    objects: &ObjectCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Result<ListingPass, ResolveFail> {
    let now = refledger_log::normalize_to_utc_millis(now);
    resolve_repo_listing_inner(repo, path, etags, pages, repo_meta, objects, client, now)
}

/// Phase 2 only: peels and action.yml using the remaining request budget.
pub(crate) fn resolve_repo_warm_up<T: Transport>(
    ctx: &WarmUpContext,
    pages: &PageBodyCache,
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<(Vec<ObservedRef>, bool), ResolveFail> {
    let (owner, name) = split_slug(&ctx.repo)?;
    let path = ctx.path.as_deref();
    let refs = if ctx.refs.is_empty() {
        pages.merged_tag_refs(&ctx.repo)
    } else {
        ctx.refs.clone()
    };
    warm_up_refs(&owner, &name, path, &refs, objects, client)
}

fn skip_budget(repo: &RepoSlug, path: Option<&str>, now: OffsetDateTime) -> Observation {
    use crate::observation::SkipReason;
    let mut b = Observation::builder()
        .repo(repo.as_str())
        .expect("repo")
        .observed_at(now)
        .expect("time")
        .method(Method::Rest)
        .outcome(Outcome::Skipped {
            reason: SkipReason::BudgetExhausted,
        });
    if let Some(p) = path {
        b = b.action_path(p);
    }
    b.build().expect("budget skip observation")
}

#[allow(clippy::too_many_arguments)]
fn fetch_repo_metadata<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    owner: &str,
    name: &str,
    etags: &mut ETagStore,
    repo_meta: &mut RepoMetaCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Result<(Option<bool>, Option<Observation>), ResolveFail> {
    let endpoint =
        ConditionalRequest::endpoint(REPO_TEMPLATE, REPO_META_QUERY).map_err(ResolveFail::etag)?;
    let cond = ConditionalRequest::at(repo.clone(), now);
    let request = cond
        .build(&endpoint, Some(client.token()), etags)
        .map_err(ResolveFail::etag)?;
    let mut headers = BTreeMap::new();
    if let Some(inm) = request.if_none_match_header() {
        headers.insert("if-none-match".into(), inm.to_owned());
    }
    let target = expand_template(REPO_TEMPLATE, owner, name, REPO_META_QUERY);
    let resp = client
        .send_raw(target, headers)
        .map_err(ResolveFail::transport)?;

    if resp.status == 301 || resp.status == 302 {
        let location = resp.header("location").unwrap_or("").to_owned();
        let obs = build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: resp.status,
                error_class: error_class_for_http(resp.status),
                backoff_applied: Duration::seconds(0),
            },
            None,
            Some(location),
            None,
        )?;
        return Ok((None, Some(obs)));
    }
    if resp.status == 404 {
        let obs = build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: 404,
                error_class: error_class_for_http(404),
                backoff_applied: Duration::seconds(0),
            },
            None,
            None,
            None,
        )?;
        return Ok((None, Some(obs)));
    }

    let response_etag = resp.header("etag").map(|s| ETag::new(s.to_owned()));
    match resp.status {
        304 => {
            etags
                .apply_response(repo, &endpoint, resp.status, response_etag.as_ref(), now)
                .map_err(ResolveFail::etag)?;
            let cached = repo_meta
                .get(repo)
                .ok_or_else(|| ResolveFail::protocol("304 on repo metadata with no cached body"))?;
            Ok((cached.archived, None))
        }
        200 => {
            etags
                .apply_response(repo, &endpoint, resp.status, response_etag.as_ref(), now)
                .map_err(ResolveFail::etag)?;
            let archived = resp
                .body
                .as_ref()
                .and_then(|b| b.get("archived"))
                .and_then(|v| v.as_bool());
            repo_meta.put(repo, archived).map_err(ResolveFail::from)?;
            Ok((archived, None))
        }
        other => Err(ResolveFail::http(
            other,
            error_class_for_http(other),
            format!("repo metadata status {other}"),
        )),
    }
}

/// Peel refs whose tip changed or reappeared (tombstone), bypassing the
/// per-run warm-up peel budget. Used so brand-new attack commits are never
/// left listing-only before classify.
pub(crate) fn peel_priority_refs<T: Transport>(
    ctx: &WarmUpContext,
    priority_names: &BTreeSet<String>,
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<Vec<ObservedRef>, ResolveFail> {
    let (owner, name) = split_slug(&ctx.repo)?;
    let path = ctx.path.as_deref();
    let refs = if ctx.refs.is_empty() {
        // Caller should pass refs; fall back empty.
        Vec::new()
    } else {
        ctx.refs.clone()
    };
    for raw in &refs {
        if !priority_names.contains(&raw.name) {
            continue;
        }
        // Always peel priority tips — never leave an attack commit listing-only.
        let peel = peel_ref(&owner, &name, raw, objects, client)?;
        if let Peeled::Commit { commit_sha, .. } = &peel.peeled {
            let _ = resolve_action_yml(&owner, &name, commit_sha, path, objects, client)?;
        }
    }
    let mut observed = Vec::with_capacity(refs.len());
    for raw in &refs {
        observed.push(observed_ref_from_cache_or_listing(raw, objects)?);
    }
    Ok(observed)
}

/// Names that must be peeled before classify: target moved, or reappeared
/// after a tombstone. Pure first-seen creations are not included.
pub(crate) fn priority_peel_names(
    state: &crate::classify::RepoState,
    refs: &[RawRef],
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for raw in refs {
        let target = raw.object_sha.as_str();
        if state.tombstone(&raw.name).is_some() {
            out.insert(raw.name.clone());
            continue;
        }
        if let Some(prev) = state.binding(&raw.name) {
            if prev.target_sha() != target {
                out.insert(raw.name.clone());
            }
        }
    }
    out
}

fn warm_up_refs<T: Transport>(
    owner: &str,
    name: &str,
    path: Option<&str>,
    refs: &[RawRef],
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<(Vec<ObservedRef>, bool), ResolveFail> {
    let mut peel_budget_hit = false;
    for raw in refs {
        if objects.contains(&raw.object_sha) {
            // Cache hit: still resolve action.yml if missing for commit peels.
            let peel = peel_ref(owner, name, raw, objects, client)?;
            if let Peeled::Commit { commit_sha, .. } = &peel.peeled {
                let _ = resolve_action_yml(owner, name, commit_sha, path, objects, client)?;
            }
            continue;
        }
        if !objects.try_consume_peel_budget() {
            peel_budget_hit = true;
            continue;
        }
        let peel = peel_ref(owner, name, raw, objects, client)?;
        if let Peeled::Commit { commit_sha, .. } = &peel.peeled {
            let _ = resolve_action_yml(owner, name, commit_sha, path, objects, client)?;
        }
    }
    let mut observed = Vec::with_capacity(refs.len());
    for raw in refs {
        observed.push(observed_ref_from_cache_or_listing(raw, objects)?);
    }
    Ok((observed, peel_budget_hit))
}

/// Build an [`ObservedRef`] from the object cache, or a listing-only stub.
///
/// Never invents `tree_sha`. A commit already in the cache must contribute
/// exactly that cached tree.
fn observed_ref_from_cache_or_listing(
    raw: &RawRef,
    objects: &ObjectCache,
) -> Result<ObservedRef, ResolveFail> {
    if let Some(cached) = objects.get(&raw.object_sha) {
        let peel = peel_from_cache(raw.object_type.as_str(), &raw.object_sha, cached);
        return observed_from_peel(raw, &peel, objects);
    }
    // Tip not cached: listing-only. Do not pretend the tip SHA is a tree.
    let ref_type = match raw.object_type.as_str() {
        "tag" => RefType::Annotated,
        _ => RefType::Lightweight,
    };
    ObservedRef::new_unpeeled(&raw.name, ref_type, &raw.object_sha).map_err(ResolveFail::obs)
}

fn observed_from_peel(
    raw: &RawRef,
    peel: &PeelResult,
    objects: &ObjectCache,
) -> Result<ObservedRef, ResolveFail> {
    match &peel.peeled {
        Peeled::Commit {
            commit_sha,
            tree_sha,
        } => {
            if let Some(cached_tree) = objects.tree_sha_for_commit(commit_sha) {
                if cached_tree != tree_sha.as_str() {
                    return Err(ResolveFail::protocol(format!(
                        "commit {commit_sha} tree mismatch: cache={cached_tree} peel={tree_sha}"
                    )));
                }
            }
            let action = match objects.get(commit_sha) {
                Some(CacheEntry::Commit {
                    action_yml_sha: Some(a),
                    ..
                }) => a.clone(),
                _ => None,
            };
            let mut r = if peel.ref_type == RefType::Annotated {
                ObservedRef::new_annotated(&raw.name, &peel.target_sha, commit_sha, tree_sha)
            } else {
                ObservedRef::new_lightweight(&raw.name, commit_sha, tree_sha)
            }
            .map_err(ResolveFail::obs)?;
            if let Some(sha) = action {
                r = r.with_action_yml_sha(sha).map_err(ResolveFail::obs)?;
            }
            Ok(r)
        }
        Peeled::NonCommit {
            object_type,
            object_sha,
        } => ObservedRef::new_non_commit(
            &raw.name,
            peel.ref_type,
            &peel.target_sha,
            *object_type,
            object_sha,
        )
        .map_err(ResolveFail::obs),
    }
}

fn warm_job(repo: &RepoSlug, path: Option<&str>, refs: Vec<RawRef>) -> Option<WarmUpContext> {
    if refs.is_empty() {
        return None;
    }
    Some(WarmUpContext {
        repo: repo.clone(),
        path: path.map(|p| p.to_owned()),
        refs,
    })
}

/// Tag-list observation for movement detection before peels complete (phase 1).
///
/// Peeled `commit_sha`/`tree_sha` come only from the object cache. Uncached
/// tips are stored as listing-only refs (target SHA, no invented tree).
fn build_listing_ok_observation(
    repo: &RepoSlug,
    path: Option<&str>,
    now: OffsetDateTime,
    refs: &[RawRef],
    etag: Option<crate::observation::ETag>,
    archived: Option<bool>,
    objects: &ObjectCache,
) -> Result<Observation, ResolveFail> {
    let mut observed = Vec::with_capacity(refs.len());
    for raw in refs {
        observed.push(observed_ref_from_cache_or_listing(raw, objects)?);
    }
    build_obs(
        repo,
        path,
        now,
        Outcome::Ok {
            http_status: 200,
            etag,
            refs: observed,
        },
        None,
        None,
        archived,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_repo_listing_inner<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    repo_meta: &mut RepoMetaCache,
    objects: &ObjectCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Result<ListingPass, ResolveFail> {
    let (owner, name) = split_slug(repo)?;

    let (archived, terminal) =
        fetch_repo_metadata(repo, path, &owner, &name, etags, repo_meta, client, now)?;
    if let Some(obs) = terminal {
        return Ok(ListingPass {
            early_observation: Some(obs),
            warm: None,
        });
    }

    let list = list_tag_refs(repo, &owner, &name, etags, pages, client, now)?;
    match list {
        ListResult::NotModified { etag } => {
            let refs = pages.merged_tag_refs(repo);
            let obs = build_obs(
                repo,
                path,
                now,
                Outcome::NotModified {
                    http_status: 304,
                    etag,
                },
                None,
                None,
                archived,
            )?;
            Ok(ListingPass {
                early_observation: Some(obs),
                warm: warm_job(repo, path, refs),
            })
        }
        ListResult::Missing => Ok(ListingPass {
            early_observation: Some(build_obs(
                repo,
                path,
                now,
                Outcome::Failed {
                    http_status: 404,
                    error_class: error_class_for_http(404),
                    backoff_applied: Duration::seconds(0),
                },
                None,
                None,
                archived,
            )?),
            warm: None,
        }),
        ListResult::Redirect { status, location } => Ok(ListingPass {
            early_observation: Some(build_obs(
                repo,
                path,
                now,
                Outcome::Failed {
                    http_status: status,
                    error_class: error_class_for_http(status),
                    backoff_applied: Duration::seconds(0),
                },
                None,
                Some(location),
                archived,
            )?),
            warm: None,
        }),
        ListResult::Refs { refs, etag } => {
            let obs =
                build_listing_ok_observation(repo, path, now, &refs, etag, archived, objects)?;
            Ok(ListingPass {
                early_observation: Some(obs),
                warm: warm_job(repo, path, refs),
            })
        }
    }
}

enum ListResult {
    Refs {
        refs: Vec<RawRef>,
        etag: Option<ETag>,
    },
    NotModified {
        etag: ETag,
    },
    Missing,
    Redirect {
        status: u16,
        location: String,
    },
}

fn list_tag_refs<T: Transport>(
    repo: &RepoSlug,
    owner: &str,
    name: &str,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Result<ListResult, ResolveFail> {
    let cond = ConditionalRequest::at(repo.clone(), now);
    let mut page: u32 = 1;
    let mut merged: Vec<RawRef> = Vec::new();
    let mut any_200 = false;
    let mut all_304 = true;
    let mut first_etag: Option<ETag> = None;
    let mut next_url: Option<String> = None;

    loop {
        let query = match &next_url {
            Some(url) => query_of_url(url)?,
            None => format!("per_page={PER_PAGE}&page={page}"),
        };
        let endpoint =
            ConditionalRequest::endpoint(TAGS_TEMPLATE, query).map_err(ResolveFail::etag)?;
        let request = cond
            .build(&endpoint, Some(client.token()), etags)
            .map_err(ResolveFail::etag)?;

        let mut headers = BTreeMap::new();
        if let Some(inm) = request.if_none_match_header() {
            headers.insert("if-none-match".into(), inm.to_owned());
        }
        let target = expand_template(TAGS_TEMPLATE, owner, name, endpoint.query());
        let resp = client
            .send_raw(target.clone(), headers)
            .map_err(ResolveFail::transport)?;

        if resp.status == 301 || resp.status == 302 {
            return Ok(ListResult::Redirect {
                status: resp.status,
                location: resp.header("location").unwrap_or("").to_owned(),
            });
        }
        if resp.status == 404 && page == 1 && next_url.is_none() {
            return Ok(ListResult::Missing);
        }

        let response_etag = resp.header("etag").map(|s| ETag::new(s.to_owned()));
        if let Some(ref et) = response_etag {
            if first_etag.is_none() {
                first_etag = Some(et.clone());
            }
        }

        match resp.status {
            304 => {
                etags
                    .apply_response(repo, &endpoint, resp.status, response_etag.as_ref(), now)
                    .map_err(ResolveFail::etag)?;
                let cached = pages.get(repo, &endpoint).ok_or_else(|| {
                    ResolveFail::protocol(
                        "304 on a tag page with no cached body; cannot merge pages",
                    )
                })?;
                merged.extend(cached.iter().cloned());
            }
            200 => {
                etags
                    .apply_response(repo, &endpoint, resp.status, response_etag.as_ref(), now)
                    .map_err(ResolveFail::etag)?;
                all_304 = false;
                any_200 = true;
                let refs = parse_refs_body(resp.body.as_ref())?;
                pages
                    .put(repo, &endpoint, refs.clone())
                    .map_err(ResolveFail::from)?;
                merged.extend(refs);
            }
            other => {
                let snip = resp
                    .body
                    .as_ref()
                    .map(|v| {
                        let s = v.to_string();
                        if s.len() > 512 {
                            format!("{}…", &s[..512])
                        } else {
                            s
                        }
                    })
                    .unwrap_or_default();
                return Err(ResolveFail::http(
                    other,
                    error_class_for_http(other),
                    format!("GET {target} status={other} body={snip}"),
                ));
            }
        }

        // Link header drives pagination — never a guessed page count.
        // A 304 often omits Link; if we already know a later page (cached body
        // or stored ETag), continue so a 304+200 mix can still merge.
        next_url = resp
            .header("link")
            .and_then(parse_next_link)
            .map(|s| s.to_owned());
        if next_url.is_none() {
            let candidate = format!("per_page={PER_PAGE}&page={}", page + 1);
            let next_endpoint = ConditionalRequest::endpoint(TAGS_TEMPLATE, &candidate)
                .map_err(ResolveFail::etag)?;
            if pages.get(repo, &next_endpoint).is_some()
                || etags.get(repo, &next_endpoint, now).is_some()
            {
                next_url = Some(format!("http://local/?{candidate}"));
            }
        }
        if next_url.is_none() {
            break;
        }
        page += 1;
    }

    if all_304 && !merged.is_empty() {
        // Full conditional hit across every page we already knew.
        // When the collection was empty and we somehow 304'd with no body
        // history, fall through — but empty+304 still means NotModified if
        // we had a prior etag.
    }
    if all_304 {
        let etag = first_etag.ok_or_else(|| {
            ResolveFail::protocol("all pages 304 but no ETag available for NotModified")
        })?;
        return Ok(ListResult::NotModified { etag });
    }
    if !any_200 && merged.is_empty() {
        return Ok(ListResult::Refs {
            refs: merged,
            etag: first_etag,
        });
    }
    Ok(ListResult::Refs {
        refs: merged,
        etag: first_etag,
    })
}

fn peel_ref<T: Transport>(
    owner: &str,
    name: &str,
    raw: &RawRef,
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<PeelResult, ResolveFail> {
    let tip_type = raw.object_type.as_str();
    let tip_sha = raw.object_sha.as_str();

    if let Some(cached) = objects.get(tip_sha) {
        return Ok(peel_from_cache(tip_type, tip_sha, cached));
    }

    match tip_type {
        "commit" => {
            let tree = fetch_commit_tree(owner, name, tip_sha, objects, client)?;
            Ok(PeelResult {
                ref_type: RefType::Lightweight,
                target_sha: tip_sha.to_owned(),
                peeled: Peeled::Commit {
                    commit_sha: tip_sha.to_owned(),
                    tree_sha: tree,
                },
            })
        }
        "tree" => {
            objects
                .put(
                    tip_sha,
                    CacheEntry::NonCommit {
                        object_type: "tree".into(),
                        object_sha: tip_sha.to_owned(),
                    },
                )
                .map_err(ResolveFail::from)?;
            Ok(PeelResult {
                ref_type: RefType::Lightweight,
                target_sha: tip_sha.to_owned(),
                peeled: Peeled::NonCommit {
                    object_type: PeeledType::Tree,
                    object_sha: tip_sha.to_owned(),
                },
            })
        }
        "blob" => {
            objects
                .put(
                    tip_sha,
                    CacheEntry::NonCommit {
                        object_type: "blob".into(),
                        object_sha: tip_sha.to_owned(),
                    },
                )
                .map_err(ResolveFail::from)?;
            Ok(PeelResult {
                ref_type: RefType::Lightweight,
                target_sha: tip_sha.to_owned(),
                peeled: Peeled::NonCommit {
                    object_type: PeeledType::Blob,
                    object_sha: tip_sha.to_owned(),
                },
            })
        }
        "tag" => peel_tag_chain(owner, name, tip_sha, tip_sha, objects, client, 0),
        other => Err(ResolveFail::protocol(format!(
            "unsupported ref object type {other}"
        ))),
    }
}

fn peel_from_cache(tip_type: &str, tip_sha: &str, cached: &CacheEntry) -> PeelResult {
    let ref_type = if tip_type == "tag" {
        RefType::Annotated
    } else {
        RefType::Lightweight
    };
    match cached {
        CacheEntry::Commit {
            commit_sha,
            tree_sha,
            ..
        } => PeelResult {
            ref_type,
            target_sha: tip_sha.to_owned(),
            peeled: Peeled::Commit {
                commit_sha: commit_sha.clone(),
                tree_sha: tree_sha.clone(),
            },
        },
        CacheEntry::NonCommit {
            object_type,
            object_sha,
        } => {
            let object_type = match object_type.as_str() {
                "tree" => PeeledType::Tree,
                "blob" => PeeledType::Blob,
                _ => PeeledType::Tree,
            };
            PeelResult {
                ref_type,
                target_sha: tip_sha.to_owned(),
                peeled: Peeled::NonCommit {
                    object_type,
                    object_sha: object_sha.clone(),
                },
            }
        }
    }
}

fn peel_tag_chain<T: Transport>(
    owner: &str,
    name: &str,
    original_tag_sha: &str,
    current_sha: &str,
    objects: &mut ObjectCache,
    client: &Client<T>,
    depth: u32,
) -> Result<PeelResult, ResolveFail> {
    if depth >= MAX_TAG_DEPTH {
        return Err(ResolveFail::http(
            0,
            ErrorClass::Protocol,
            format!("annotated tag nesting exceeded depth {MAX_TAG_DEPTH}"),
        ));
    }

    if let Some(cached) = objects.get(current_sha) {
        // Cache hit on an intermediate tag object still attributes Annotated
        // to the original tip.
        let mut peel = peel_from_cache("tag", original_tag_sha, cached);
        peel.ref_type = RefType::Annotated;
        peel.target_sha = original_tag_sha.to_owned();
        return Ok(peel);
    }

    let target = format!("/repos/{owner}/{name}/git/tags/{current_sha}");
    let resp = client
        .send_raw(target, BTreeMap::new())
        .map_err(ResolveFail::transport)?;
    if resp.status != 200 {
        return Err(ResolveFail::http(
            resp.status,
            error_class_for_http(resp.status),
            format!("git/tags status {}", resp.status),
        ));
    }
    let body = resp
        .body
        .as_ref()
        .ok_or_else(|| ResolveFail::protocol("git/tags empty body"))?;
    let obj = body
        .get("object")
        .ok_or_else(|| ResolveFail::protocol("git/tags missing object"))?;
    let next_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveFail::protocol("git/tags object.type"))?;
    let next_sha = obj
        .get("sha")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveFail::protocol("git/tags object.sha"))?;

    match next_type {
        "commit" => {
            let tree = fetch_commit_tree(owner, name, next_sha, objects, client)?;
            let entry = CacheEntry::Commit {
                commit_sha: next_sha.to_owned(),
                tree_sha: tree.clone(),
                action_yml_sha: None,
            };
            // Cache under this tag object and the original tip.
            objects
                .put(current_sha, entry.clone())
                .map_err(ResolveFail::from)?;
            if current_sha != original_tag_sha {
                objects
                    .put(original_tag_sha, entry)
                    .map_err(ResolveFail::from)?;
            }
            Ok(PeelResult {
                ref_type: RefType::Annotated,
                target_sha: original_tag_sha.to_owned(),
                peeled: Peeled::Commit {
                    commit_sha: next_sha.to_owned(),
                    tree_sha: tree,
                },
            })
        }
        "tag" => {
            // Recurse; depth counts this hop.
            let result = peel_tag_chain(
                owner,
                name,
                original_tag_sha,
                next_sha,
                objects,
                client,
                depth + 1,
            )?;
            // Also remember this intermediate tag peels the same way.
            if let Peeled::Commit {
                commit_sha,
                tree_sha,
            } = &result.peeled
            {
                let _ = objects.put(
                    current_sha,
                    CacheEntry::Commit {
                        commit_sha: commit_sha.clone(),
                        tree_sha: tree_sha.clone(),
                        action_yml_sha: None,
                    },
                );
            }
            Ok(result)
        }
        "tree" | "blob" => {
            let object_type = if next_type == "tree" {
                PeeledType::Tree
            } else {
                PeeledType::Blob
            };
            let entry = CacheEntry::NonCommit {
                object_type: next_type.to_owned(),
                object_sha: next_sha.to_owned(),
            };
            objects
                .put(current_sha, entry.clone())
                .map_err(ResolveFail::from)?;
            if current_sha != original_tag_sha {
                objects
                    .put(original_tag_sha, entry)
                    .map_err(ResolveFail::from)?;
            }
            Ok(PeelResult {
                ref_type: RefType::Annotated,
                target_sha: original_tag_sha.to_owned(),
                peeled: Peeled::NonCommit {
                    object_type,
                    object_sha: next_sha.to_owned(),
                },
            })
        }
        other => Err(ResolveFail::protocol(format!(
            "tag object points at unsupported type {other}"
        ))),
    }
}

fn fetch_commit_tree<T: Transport>(
    owner: &str,
    name: &str,
    commit_sha: &str,
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<String, ResolveFail> {
    if let Some(CacheEntry::Commit { tree_sha, .. }) = objects.get(commit_sha) {
        return Ok(tree_sha.clone());
    }
    // Lighter than /commits/{sha}: no files array, no stats.
    let target = format!("/repos/{owner}/{name}/git/commits/{commit_sha}");
    let resp = client
        .send_raw(target, BTreeMap::new())
        .map_err(ResolveFail::transport)?;
    if resp.status != 200 {
        return Err(ResolveFail::http(
            resp.status,
            error_class_for_http(resp.status),
            format!("git/commits status {}", resp.status),
        ));
    }
    let tree_sha = resp
        .body
        .as_ref()
        .and_then(|b| b.get("tree"))
        .and_then(|t| t.get("sha"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| ResolveFail::protocol("git/commits missing tree.sha"))?
        .to_owned();
    objects
        .put(
            commit_sha,
            CacheEntry::Commit {
                commit_sha: commit_sha.to_owned(),
                tree_sha: tree_sha.clone(),
                action_yml_sha: None,
            },
        )
        .map_err(ResolveFail::from)?;
    Ok(tree_sha)
}

fn resolve_action_yml<T: Transport>(
    owner: &str,
    name: &str,
    commit_sha: &str,
    path: Option<&str>,
    objects: &mut ObjectCache,
    client: &Client<T>,
) -> Result<Option<String>, ResolveFail> {
    if let Some(CacheEntry::Commit {
        action_yml_sha: Some(cached),
        ..
    }) = objects.get(commit_sha)
    {
        return Ok(cached.clone());
    }

    let base = match path {
        Some(p) => {
            let p = p.trim_matches('/');
            format!("/repos/{owner}/{name}/contents/{p}")
        }
        None => format!("/repos/{owner}/{name}/contents"),
    };

    for filename in ["action.yml", "action.yaml"] {
        let target = format!("{base}/{filename}?ref={commit_sha}");
        let resp = client
            .send_raw(target, BTreeMap::new())
            .map_err(ResolveFail::transport)?;
        match resp.status {
            200 => {
                let sha = resp
                    .body
                    .as_ref()
                    .and_then(|b| b.get("sha"))
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| ResolveFail::protocol("contents missing sha"))?
                    .to_owned();
                // Ensure commit entry exists then record action digest.
                if objects.get(commit_sha).is_none() {
                    return Err(ResolveFail::protocol(
                        "action.yml resolved before commit peel was cached",
                    ));
                }
                objects
                    .set_action_yml(commit_sha, Some(&sha))
                    .map_err(ResolveFail::from)?;
                return Ok(Some(sha));
            }
            404 => continue,
            other => {
                return Err(ResolveFail::http(
                    other,
                    error_class_for_http(other),
                    format!("contents status {other}"),
                ));
            }
        }
    }

    objects
        .set_action_yml(commit_sha, None)
        .map_err(ResolveFail::from)?;
    Ok(None)
}

fn parse_refs_body(body: Option<&Value>) -> Result<Vec<RawRef>, ResolveFail> {
    let body = body.ok_or_else(|| ResolveFail::protocol("refs body missing"))?;
    let arr = body
        .as_array()
        .ok_or_else(|| ResolveFail::protocol("refs body not an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveFail::protocol("ref missing"))?
            .to_owned();
        let obj = item
            .get("object")
            .ok_or_else(|| ResolveFail::protocol("ref.object missing"))?;
        let object_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveFail::protocol("ref.object.type"))?
            .to_owned();
        let object_sha = obj
            .get("sha")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveFail::protocol("ref.object.sha"))?
            .to_owned();
        out.push(RawRef {
            name,
            object_type,
            object_sha,
        });
    }
    Ok(out)
}

fn parse_next_link(link: &str) -> Option<&str> {
    // <url>; rel="next", <url>; rel="last"
    for part in link.split(',') {
        let part = part.trim();
        if !part.contains("rel=\"next\"") && !part.contains("rel='next'") {
            continue;
        }
        let start = part.find('<')? + 1;
        let end = part.find('>')?;
        return Some(&part[start..end]);
    }
    None
}

fn query_of_url(url: &str) -> Result<String, ResolveFail> {
    let q = url
        .split_once('?')
        .map(|(_, q)| q.to_owned())
        .unwrap_or_default();
    Ok(q)
}

fn expand_template(template: &str, owner: &str, name: &str, query: &str) -> String {
    let path = template.replace("{owner}", owner).replace("{repo}", name);
    if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    }
}

fn split_slug(repo: &RepoSlug) -> Result<(String, String), ResolveFail> {
    let mut parts = repo.as_str().split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) => Ok((o.to_owned(), n.to_owned())),
        _ => Err(ResolveFail::protocol("invalid repo slug")),
    }
}

fn build_obs(
    repo: &RepoSlug,
    path: Option<&str>,
    now: OffsetDateTime,
    outcome: Outcome,
    rate_limit_remaining: Option<u32>,
    redirect_location: Option<String>,
    archived: Option<bool>,
) -> Result<Observation, ResolveFail> {
    let mut b = Observation::builder()
        .repo(repo.as_str())
        .map_err(ResolveFail::obs)?
        .observed_at(now)
        .map_err(ResolveFail::obs)?
        .method(Method::Rest);
    if let Some(r) = rate_limit_remaining {
        b = b.rate_limit_remaining(r);
    }
    if let Some(loc) = redirect_location {
        b = b.redirect_location(loc);
    }
    if let Some(a) = archived {
        b = b.archived(a);
    }
    if let Some(p) = path {
        b = b.action_path(p);
    }
    b.outcome(outcome).build().map_err(ResolveFail::obs)
}

/// Build an Ok observation after phase-2 warm-up (shared with `once`).
pub(crate) fn build_obs_for_once(
    repo: &str,
    path: Option<&str>,
    now: OffsetDateTime,
    outcome: Outcome,
    archived: Option<bool>,
) -> Result<Observation, ResolveFail> {
    let slug = RepoSlug::parse(repo).map_err(ResolveFail::obs)?;
    build_obs(&slug, path, now, outcome, None, None, archived)
}

fn failed_observation(
    repo: &RepoSlug,
    path: Option<&str>,
    now: OffsetDateTime,
    err: ResolveFail,
) -> Observation {
    let (http_status, error_class, message) = match &err {
        ResolveFail::Fail {
            http_status,
            error_class,
            message,
        } => (*http_status, *error_class, message.clone()),
        ResolveFail::BudgetExhausted => {
            return skip_budget(repo, path, now);
        }
        ResolveFail::Other(e) => (0, ErrorClass::Protocol, e.to_string()),
    };
    // Keep status 0 for transport/protocol failures that never saw HTTP.
    // Never rewrite Network into a fake 422 — that hid budget exhaustion.
    eprintln!(
        "resolve failed repo={} path={:?} status={} class={:?}: {message}",
        repo.as_str(),
        path,
        http_status,
        error_class
    );
    build_obs(
        repo,
        path,
        now,
        Outcome::Failed {
            http_status,
            error_class,
            backoff_applied: Duration::seconds(0),
        },
        None,
        None,
        None,
    )
    .unwrap_or_else(|_| {
        Observation::builder()
            .repo(repo.as_str())
            .expect("repo")
            .observed_at(now)
            .expect("time")
            .method(Method::Rest)
            .outcome(Outcome::Failed {
                http_status,
                error_class,
                backoff_applied: Duration::seconds(0),
            })
            .build()
            .expect("observation")
    })
}

#[derive(Debug)]
pub(crate) enum ResolveFail {
    Fail {
        http_status: u16,
        error_class: ErrorClass,
        message: String,
    },
    /// Mid-resolve request budget was exhausted; surface as Skipped.
    BudgetExhausted,
    #[allow(dead_code)]
    Other(RestError),
}

impl ResolveFail {
    fn http(status: u16, class: ErrorClass, message: impl Into<String>) -> Self {
        let message = message.into();
        // Log URL/status/body context carried in the message at construction.
        eprintln!("github http error status={status} class={class:?}: {message}");
        Self::Fail {
            http_status: status,
            error_class: class,
            message,
        }
    }
    fn protocol(message: impl Into<String>) -> Self {
        // Protocol failures are not HTTP responses; status 0.
        Self::http(0, ErrorClass::Protocol, message)
    }
    fn transport(message: impl Into<String>) -> Self {
        let message = message.into();
        if message.contains("request budget exhausted") {
            return Self::BudgetExhausted;
        }
        eprintln!("github transport error: {message}");
        Self::http(0, ErrorClass::Network, message)
    }
    fn etag(e: impl std::fmt::Display) -> Self {
        Self::Other(RestError::Etag(e.to_string()))
    }
    fn obs(e: ObservationError) -> Self {
        Self::Other(RestError::Observation(e.to_string()))
    }
}

impl From<RestError> for ResolveFail {
    fn from(e: RestError) -> Self {
        Self::Other(e)
    }
}
