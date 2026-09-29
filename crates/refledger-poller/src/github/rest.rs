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

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use crate::github::etag::{AuthToken, ConditionalRequest, ETagStore, EndpointKey};
use crate::observation::{
    ETag, ErrorClass, Method, Observation, ObservationError, ObservedRef, Outcome, PeeledType,
    RefType, RepoSlug,
};

/// Maximum number of annotated-tag objects followed while peeling to a commit.
pub const MAX_TAG_DEPTH: u32 = 8;

const TAGS_TEMPLATE: &str = "/repos/{owner}/{repo}/git/matching-refs/tags";
const PER_PAGE: u32 = 100;

/// One outbound REST request as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestRequest {
    pub method: &'static str,
    pub target: String,
    pub headers: BTreeMap<String, String>,
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
struct RawRef {
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

/// Resolve one repository (and optional subdirectory action path) into an
/// [`Observation`]. Failures are `Outcome::Failed`, not panics.
pub fn resolve_repo<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    objects: &mut ObjectCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Observation {
    match resolve_repo_inner(repo, path, etags, pages, objects, client, now) {
        Ok(obs) => obs,
        Err(err) => failed_observation(repo, path, now, err),
    }
}

fn resolve_repo_inner<T: Transport>(
    repo: &RepoSlug,
    path: Option<&str>,
    etags: &mut ETagStore,
    pages: &mut PageBodyCache,
    objects: &mut ObjectCache,
    client: &Client<T>,
    now: OffsetDateTime,
) -> Result<Observation, ResolveFail> {
    let (owner, name) = split_slug(repo)?;

    // --- repository identity -------------------------------------------------
    let repo_target = format!("/repos/{owner}/{name}");
    let repo_resp = client
        .send_raw(repo_target, BTreeMap::new())
        .map_err(ResolveFail::transport)?;

    if repo_resp.status == 301 || repo_resp.status == 302 {
        let location = repo_resp.header("location").unwrap_or("").to_owned();
        return build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: repo_resp.status,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            None,
            Some(location),
            None,
        );
    }
    if repo_resp.status == 404 {
        return build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: 404,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            None,
            None,
            None,
        );
    }
    if repo_resp.status != 200 {
        return Err(ResolveFail::http(
            repo_resp.status,
            ErrorClass::Upstream,
            format!("repo metadata status {}", repo_resp.status),
        ));
    }
    let archived = repo_resp
        .body
        .as_ref()
        .and_then(|b| b.get("archived"))
        .and_then(|v| v.as_bool());

    // --- list tags (matching-refs) with conditional pagination ---------------
    let list = list_tag_refs(repo, &owner, &name, etags, pages, client, now)?;
    match list {
        ListResult::NotModified { etag } => build_obs(
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
        ),
        ListResult::Missing => build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: 404,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            None,
            None,
            archived,
        ),
        ListResult::Redirect { status, location } => build_obs(
            repo,
            path,
            now,
            Outcome::Failed {
                http_status: status,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            None,
            Some(location),
            archived,
        ),
        ListResult::Refs { refs, etag } => {
            let mut observed = Vec::with_capacity(refs.len());
            let mut peel_budget_hit = false;
            for raw in &refs {
                if !objects.contains(&raw.object_sha) && !objects.try_consume_peel_budget() {
                    peel_budget_hit = true;
                    break;
                }
                let peel = peel_ref(&owner, &name, raw, objects, client)?;
                let mut observed_ref = match peel.peeled {
                    Peeled::Commit {
                        commit_sha,
                        tree_sha,
                    } => {
                        let action =
                            resolve_action_yml(&owner, &name, &commit_sha, path, objects, client)?;
                        let mut r = if peel.ref_type == RefType::Annotated {
                            ObservedRef::new_annotated(
                                &raw.name,
                                &peel.target_sha,
                                &commit_sha,
                                &tree_sha,
                            )
                        } else {
                            ObservedRef::new_lightweight(&raw.name, &commit_sha, &tree_sha)
                        }
                        .map_err(ResolveFail::obs)?;
                        if let Some(sha) = action {
                            r = r.with_action_yml_sha(sha).map_err(ResolveFail::obs)?;
                        }
                        r
                    }
                    Peeled::NonCommit {
                        object_type,
                        object_sha,
                    } => ObservedRef::new_non_commit(
                        &raw.name,
                        peel.ref_type,
                        &peel.target_sha,
                        object_type,
                        &object_sha,
                    )
                    .map_err(ResolveFail::obs)?,
                };
                let _ = &mut observed_ref;
                observed.push(observed_ref);
            }

            // Partial peel: omit ETag so the next run re-lists and continues
            // warm-up from the content-addressed cache.
            let etag = if peel_budget_hit { None } else { etag };

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
            .send_raw(target, headers)
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

        etags
            .apply_response(repo, &endpoint, resp.status, response_etag.as_ref(), now)
            .map_err(ResolveFail::etag)?;

        match resp.status {
            304 => {
                let cached = pages.get(repo, &endpoint).ok_or_else(|| {
                    ResolveFail::protocol(
                        "304 on a tag page with no cached body; cannot merge pages",
                    )
                })?;
                merged.extend(cached.iter().cloned());
            }
            200 => {
                all_304 = false;
                any_200 = true;
                let refs = parse_refs_body(resp.body.as_ref())?;
                pages
                    .put(repo, &endpoint, refs.clone())
                    .map_err(ResolveFail::from)?;
                merged.extend(refs);
            }
            other => {
                return Err(ResolveFail::http(
                    other,
                    ErrorClass::Upstream,
                    format!("matching-refs status {other}"),
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
            ErrorClass::Upstream,
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
            ErrorClass::Upstream,
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
                    ErrorClass::Upstream,
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

fn failed_observation(
    repo: &RepoSlug,
    path: Option<&str>,
    now: OffsetDateTime,
    err: ResolveFail,
) -> Observation {
    let (http_status, error_class) = match &err {
        ResolveFail::Fail {
            http_status,
            error_class,
            ..
        } => (*http_status, *error_class),
        _ => (0, ErrorClass::Protocol),
    };
    // Depth-limit and protocol failures use status 0 in the wire outcome when
    // no HTTP status applies; classification still sees ErrorClass::Protocol.
    build_obs(
        repo,
        path,
        now,
        Outcome::Failed {
            http_status: if http_status == 0 { 422 } else { http_status },
            error_class,
            backoff_applied: Duration::seconds(0),
        },
        None,
        None,
        None,
    )
    .unwrap_or_else(|_| {
        // Last resort: builder itself failed (bad clock). Construct minimally.
        Observation::builder()
            .repo(repo.as_str())
            .expect("repo")
            .observed_at(now)
            .expect("time")
            .method(Method::Rest)
            .outcome(Outcome::Failed {
                http_status: 422,
                error_class: ErrorClass::Protocol,
                backoff_applied: Duration::seconds(0),
            })
            .build()
            .expect("observation")
    })
}

#[derive(Debug)]
enum ResolveFail {
    Fail {
        http_status: u16,
        error_class: ErrorClass,
        #[allow(dead_code)]
        message: String,
    },
    #[allow(dead_code)]
    Other(RestError),
}

impl ResolveFail {
    fn http(status: u16, class: ErrorClass, message: impl Into<String>) -> Self {
        Self::Fail {
            http_status: status,
            error_class: class,
            message: message.into(),
        }
    }
    fn protocol(message: impl Into<String>) -> Self {
        Self::http(422, ErrorClass::Protocol, message)
    }
    fn transport(message: impl Into<String>) -> Self {
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
