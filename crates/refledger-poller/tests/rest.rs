//! Tag resolution over recorded GitHub REST fixtures.
//!
//! No live network. Fixtures under `tests/fixtures/rest/` are replayed through
//! a mock transport. Git objects are content-addressed and immutable: a cached
//! dereference is permanently correct, so ObjectCache has no TTL and no
//! invalidation path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use refledger_poller::github::etag::{AuthToken, ConditionalRequest, ETagStore};
use refledger_poller::github::rest::{
    resolve_repo, Client, ObjectCache, PageBodyCache, RepoMetaCache, RestRequest, RestResponse,
    Transport,
};
use refledger_poller::observation::{ErrorClass, Method, Outcome, PeeledType, RefType, RepoSlug};
use serde_json::Value;
use tempfile::TempDir;
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

const COMMIT_1: &str = "1111111111111111111111111111111111111111";
const COMMIT_2: &str = "2222222222222222222222222222222222222222";
const TREE_1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TREE_2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TAG_1: &str = "d111111111111111111111111111111111111111";
const BLOB_1: &str = "b100000000000000000000000000000000000001";
const ACTION_YML: &str = "a100000000000000000000000000000000000001";
const TAG_NEST_0: &str = "d000000000000000000000000000000000000002";

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

fn now() -> OffsetDateTime {
    odt(2026, Month::September, 28, 12, 0, 0, 0)
}

/// Variant of the tidy `now()` helper: a real clock reading that still
/// produces a valid observation after boundary normalisation in resolve_repo.
fn live_now() -> OffsetDateTime {
    let raw = OffsetDateTime::now_utc();
    raw.replace_nanosecond((raw.nanosecond() / 1_000_000) * 1_000_000 + 42_001)
        .unwrap_or(raw)
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

/// Routes request targets to fixture names. Records every target requested.
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

    fn requested_targets(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.target.clone())
            .collect()
    }

    fn requested_containing(&self, needle: &str) -> Vec<String> {
        self.requested_targets()
            .into_iter()
            .filter(|t| t.contains(needle))
            .collect()
    }

    fn clear_log(&self) {
        self.log.lock().unwrap().clear();
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
                // Exact path (or full target) match beats every substring.
                10_000 + key.len()
            } else if path.starts_with(&format!("{key}/")) {
                // `/repos/acme/widgets` must not capture `/repos/acme/widgets/git/...`.
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
        let fixture = best.map(|(f, _)| f).ok_or_else(|| {
            format!(
                "no fixture route for target {} (headers {:?})",
                request.target, request.headers
            )
        })?;
        Ok(load_fixture(fixture))
    }
}

struct Harness {
    _dir: TempDir,
    etags: ETagStore,
    pages: PageBodyCache,
    repo_meta: RepoMetaCache,
    objects: ObjectCache,
    client: Client<MockTransport>,
    repo: RepoSlug,
}

impl Harness {
    fn new(transport: MockTransport) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let etags =
            ETagStore::open(dir.path().join("etags.jsonl"), Duration::hours(24)).expect("etags");
        let pages = PageBodyCache::open(dir.path().join("pages.jsonl")).expect("pages");
        let repo_meta = RepoMetaCache::open(dir.path().join("repo_meta.jsonl")).expect("repo_meta");
        let objects = ObjectCache::open(dir.path().join("objects.jsonl")).expect("objects");
        let client = Client::new(transport, AuthToken::new("ghp_test_token").unwrap());
        Self {
            _dir: dir,
            etags,
            pages,
            repo_meta,
            objects,
            client,
            repo: RepoSlug::parse("acme/widgets").unwrap(),
        }
    }

    fn resolve(&mut self, path: Option<&str>) -> refledger_poller::observation::Observation {
        self.resolve_at(path, now())
    }

    fn resolve_at(
        &mut self,
        path: Option<&str>,
        at: OffsetDateTime,
    ) -> refledger_poller::observation::Observation {
        resolve_repo(
            &self.repo,
            path,
            &mut self.etags,
            &mut self.pages,
            &mut self.repo_meta,
            &mut self.objects,
            &self.client,
            at,
        )
    }
}

// ---------------------------------------------------------------------------
// EMPTY vs MISSING — verified against live GitHub, not assumed.
// ---------------------------------------------------------------------------

#[test]
fn empty_vs_missing_are_distinct_via_matching_refs() {
    // Live capture 2026-09-28 against octocat/Spoon-Knife (zero tags) and a
    // nonexistent owner/repo:
    //
    //   GET .../git/refs/tags
    //     empty repo  -> 404 Not Found  (same as missing repo)
    //     missing repo -> 404 Not Found
    //     => NOT distinguishable.
    //
    //   GET .../git/matching-refs/tags
    //     empty repo  -> 200 []
    //     missing repo -> 404 Not Found
    //     => distinguishable. rest.rs therefore lists via matching-refs.
    //
    // Fixtures `empty_repo_*.json` and `missing_repo_*.json` are the recorded
    // responses (status + body). Do not "fix" this by assuming refs/tags
    // returns [].

    let empty_matching = load_fixture("empty_repo_matching_refs");
    assert_eq!(empty_matching.status, 200);
    assert_eq!(empty_matching.body, Some(Value::Array(vec![])));

    let empty_refs = load_fixture("empty_repo_refs_tags");
    assert_eq!(empty_refs.status, 404, "refs/tags 404s on a tagless repo");

    let missing_matching = load_fixture("missing_repo_matching_refs");
    assert_eq!(missing_matching.status, 404);
    let missing_refs = load_fixture("missing_repo_refs_tags");
    assert_eq!(missing_refs.status, 404);

    // Empty repo observation is Ok with zero refs — normal.
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("/git/matching-refs/tags", "empty_repo_matching_refs");
    let mut h = Harness::new(transport);
    let obs = h.resolve(None);
    assert_eq!(obs.method(), Method::Rest);
    match obs.outcome() {
        Outcome::Ok {
            refs, http_status, ..
        } => {
            assert_eq!(*http_status, 200);
            assert!(refs.is_empty(), "empty is a normal zero-ref observation");
        }
        other => panic!("expected Ok empty, got {other:?}"),
    }

    // Missing repo is a deletion-class Failed observation — not an empty Ok.
    let transport = MockTransport::new();
    transport.route(
        "/repos/refledger-definitely-does-not-exist-xyzzy/nope",
        "repo_missing",
    );
    let mut h = Harness::new(transport);
    h.repo = RepoSlug::parse("refledger-definitely-does-not-exist-xyzzy/nope").unwrap();
    let obs = h.resolve(None);
    match obs.outcome() {
        Outcome::Failed {
            http_status,
            error_class,
            ..
        } => {
            assert_eq!(*http_status, 404);
            assert_eq!(*error_class, ErrorClass::ApiClient);
        }
        other => panic!("missing repo must be Failed, not empty Ok: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Pagination + ETags
// ---------------------------------------------------------------------------

#[test]
fn paginated_matching_refs_uses_per_page_etags_and_link_header() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags?per_page=100&page=1", "tags_page1_200")
        .route("matching-refs/tags?per_page=100&page=2", "tags_page2_200")
        .route("/git/commits/", "git_commit_1"); // overridden per sha below
                                                 // More specific commit routes:
    transport
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route(&format!("/git/commits/{COMMIT_2}"), "git_commit_2")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);

    let targets = transport.requested_targets();
    assert!(
        targets.iter().any(|t| t.contains("page=1")),
        "page 1 must be requested: {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t.contains("page=2")),
        "Link-driven page 2 must be requested, never a guessed page count: {targets:?}"
    );
    assert!(
        !targets.iter().any(|t| t.contains("page=3")),
        "must not invent page 3 without a Link rel=next: {targets:?}"
    );

    let refs = obs.refs();
    assert_eq!(refs.len(), 2, "merged both pages");
    let names: Vec<_> = refs.iter().map(|r| r.name()).collect();
    assert!(names.contains(&"refs/tags/v1"));
    assert!(names.contains(&"refs/tags/v2"));

    // Each page stores its own ETag under its own query string.
    let page1 = ConditionalRequest::endpoint(
        "/repos/{owner}/{repo}/git/matching-refs/tags",
        "per_page=100&page=1",
    )
    .unwrap();
    let page2 = ConditionalRequest::endpoint(
        "/repos/{owner}/{repo}/git/matching-refs/tags",
        "per_page=100&page=2",
    )
    .unwrap();
    assert_eq!(
        h.etags
            .get(&h.repo, &page1, now())
            .map(|e| e.as_str().to_owned()),
        Some("W/\"tags-p1\"".into())
    );
    assert_eq!(
        h.etags
            .get(&h.repo, &page2, now())
            .map(|e| e.as_str().to_owned()),
        Some("W/\"tags-p2\"".into())
    );
}

#[test]
fn page1_304_plus_page2_200_merges_complete_ref_set() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags?per_page=100&page=1", "tags_page1_200")
        .route("matching-refs/tags?per_page=100&page=2", "tags_page2_200")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route(&format!("/git/commits/{COMMIT_2}"), "git_commit_2")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let first = h.resolve(None);
    assert_eq!(first.refs().len(), 2);

    // Second sweep: page 1 unchanged (304), page 2 has new content.
    transport.clear_log();
    transport
        .route("matching-refs/tags?per_page=100&page=1", "tags_page1_304")
        .route(
            "matching-refs/tags?per_page=100&page=2",
            "tags_page2_changed",
        );

    let second = h.resolve(None);
    let names: Vec<_> = second.refs().iter().map(|r| r.name().to_owned()).collect();
    assert!(
        names.contains(&"refs/tags/v1".to_owned()),
        "304 on page 1 must reuse the cached page-1 body, not drop it: {names:?}"
    );
    assert!(names.contains(&"refs/tags/v2".to_owned()));
    assert!(names.contains(&"refs/tags/v3".to_owned()));
    assert_eq!(
        names.len(),
        3,
        "complete merge, not truncated to page 2 alone"
    );
}

#[test]
fn full_304_returns_not_modified_without_touching_object_cache() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let _first = h.resolve(None);
    let entries_after_first = h.objects.len();

    transport.clear_log();
    transport.route("matching-refs/tags", "tags_304");
    // Single-page collection: one 304 across all pages.
    let obs = h.resolve(None);
    assert!(
        matches!(
            obs.outcome(),
            Outcome::NotModified {
                http_status: 304,
                ..
            }
        ),
        "got {:?}",
        obs.outcome()
    );
    assert_eq!(
        h.objects.len(),
        entries_after_first,
        "NotModified must not touch ObjectCache"
    );
    assert!(
        transport.requested_containing("/git/commits/").is_empty(),
        "no dereference on full 304"
    );
    assert!(
        transport.requested_containing("/git/tags/").is_empty(),
        "no tag peel on full 304"
    );
}

// ---------------------------------------------------------------------------
// Dereferencing
// ---------------------------------------------------------------------------

#[test]
fn lightweight_tag_is_one_hop_no_git_tags_request() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.ref_type(), RefType::Lightweight);
    assert_eq!(r.target_sha(), COMMIT_1);
    assert_eq!(r.commit_sha(), Some(COMMIT_1));
    assert_eq!(r.tree_sha(), Some(TREE_1));
    assert!(
        transport.requested_containing("/git/tags/").is_empty(),
        "lightweight needs no /git/tags hop"
    );
    let commits = transport.requested_containing("/git/commits/");
    assert_eq!(commits.len(), 1);
    assert!(
        commits[0].contains(&format!("/git/commits/{COMMIT_1}")),
        "must use /git/commits/{{sha}}, not /commits/{{sha}}: {commits:?}"
    );
    assert!(
        transport
            .requested_containing("/commits/")
            .iter()
            .all(|t| t.contains("/git/commits/")),
        "the heavier /commits/{{sha}} endpoint must not be called"
    );
}

/// Same resolve path as `lightweight_tag_resolves_commit_and_tree`, but with a
/// real system-clock reading (sub-millisecond). Only passes if the REST
/// boundary normalises before building the observation.
#[test]
fn lightweight_tag_resolves_with_live_system_clock() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let messy = live_now();
    assert_ne!(messy.nanosecond() % 1_000_000, 0);
    let mut h = Harness::new(transport);
    let obs = h.resolve_at(None, messy);
    assert!(
        matches!(obs.outcome(), Outcome::Ok { .. }),
        "{:?}",
        obs.outcome()
    );
    let wire = serde_json::to_string(&obs).unwrap();
    let v: Value = serde_json::from_str(&wire).unwrap();
    let at = v["observed_at"].as_str().unwrap();
    assert_eq!(at.len(), "YYYY-MM-DDTHH:MM:SS.sssZ".len());
    assert!(at.ends_with('Z'));
}

#[test]
fn annotated_tag_fetches_git_tags_and_keeps_distinct_shas() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_annotated")
        .route(&format!("/git/tags/{TAG_1}"), "git_tag_annotated")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.ref_type(), RefType::Annotated);
    assert_eq!(r.target_sha(), TAG_1);
    assert_eq!(r.commit_sha(), Some(COMMIT_1));
    assert_ne!(r.target_sha(), r.commit_sha().unwrap());
    assert!(
        !transport
            .requested_containing(&format!("/git/tags/{TAG_1}"))
            .is_empty(),
        "annotated must call /git/tags/{{sha}}"
    );
}

#[test]
fn nested_annotated_tags_follow_to_commit() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_nested")
        .route(&format!("/git/tags/{TAG_NEST_0}"), "git_tag_nest0")
        .route(
            "/git/tags/d000000000000000000000000000000000000003",
            "git_tag_nest1",
        )
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.ref_type(), RefType::Annotated);
    assert_eq!(r.commit_sha(), Some(COMMIT_1));
    assert_eq!(r.tree_sha(), Some(TREE_1));
    assert!(transport.requested_containing("/git/tags/").len() >= 2);
}

#[test]
fn nested_tag_depth_limit_8_is_failed_observation_not_panic() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_too_deep");
    for i in 0..9 {
        let sha = format!("d{:039}", i + 2);
        transport.route(&format!("/git/tags/{sha}"), &format!("git_tag_deep{i}"));
    }

    let mut h = Harness::new(transport);
    let obs = h.resolve(None);
    match obs.outcome() {
        Outcome::Failed {
            error_class: ErrorClass::Protocol,
            ..
        } => {}
        other => panic!("depth limit must be Failed/Protocol, got {other:?}"),
    }
}

#[test]
fn tag_pointing_at_tree_records_object_type_without_commit_fetch() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_tree")
        .route(&format!("/git/commits/{TREE_1}"), "git_commit_1"); // must NOT be hit

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.peeled_type(), PeeledType::Tree);
    assert_eq!(r.peeled_object_sha(), Some(TREE_1));
    assert!(r.commit_sha().is_none());
    assert!(
        transport.requested_containing("/git/commits/").is_empty(),
        "tree tip must not crash into commit resolution"
    );
}

#[test]
fn tag_pointing_at_blob_records_object_type() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_blob");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.peeled_type(), PeeledType::Blob);
    assert_eq!(r.peeled_object_sha(), Some(BLOB_1));
    assert!(transport.requested_containing("/git/commits/").is_empty());
}

// ---------------------------------------------------------------------------
// ObjectCache — the budget thesis
// ---------------------------------------------------------------------------

#[test]
fn second_sweep_over_unchanged_refs_issues_zero_dereference_requests() {
    // Content addressing makes a cached dereference permanently correct.
    // ObjectCache has no TTL and no invalidation path: an expiry would only
    // reintroduce cost. There is no `invalidate`, `expire`, or `ttl` API.
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_annotated")
        .route(&format!("/git/tags/{TAG_1}"), "git_tag_annotated")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_action_yml");

    let mut h = Harness::new(transport.clone());
    let _ = h.resolve(None);
    assert!(!h.objects.is_empty());
    assert!(
        !h.objects.has_ttl_or_invalidation_api(),
        "ObjectCache must not expose TTL or invalidation — content-addressed peels do not go stale"
    );

    transport.clear_log();
    // Refs payload changes ETag? Use 200 with same SHAs (new listing etag) to
    // force a full resolve path that still hits the object cache.
    transport.route("matching-refs/tags", "tags_annotated");
    let _ = h.resolve(None);

    assert!(
        transport.requested_containing("/git/tags/").is_empty(),
        "second sweep must not re-peel tag objects: {:?}",
        transport.requested_targets()
    );
    assert!(
        transport.requested_containing("/git/commits/").is_empty(),
        "second sweep must not re-fetch commits: {:?}",
        transport.requested_targets()
    );
    assert!(
        transport.requested_containing("/contents/").is_empty(),
        "action.yml is cached by commit SHA: {:?}",
        transport.requested_targets()
    );
}

#[test]
fn ref_move_dereferences_only_the_new_target_old_stays_cached() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route(&format!("/git/commits/{COMMIT_2}"), "git_commit_2")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let _ = h.resolve(None);
    assert!(h.objects.contains(COMMIT_1));

    transport.clear_log();
    transport.route("matching-refs/tags", "tags_moved");
    let obs = h.resolve(None);
    assert_eq!(obs.refs()[0].commit_sha(), Some(COMMIT_2));
    assert_eq!(obs.refs()[0].tree_sha(), Some(TREE_2));

    let commit_fetches = transport.requested_containing("/git/commits/");
    assert_eq!(
        commit_fetches.len(),
        1,
        "only the new target: {commit_fetches:?}"
    );
    assert!(commit_fetches[0].contains(COMMIT_2));
    assert!(
        h.objects.contains(COMMIT_1),
        "old target stays cached for classify.rs diffing"
    );
    assert!(h.objects.contains(COMMIT_2));
}

// ---------------------------------------------------------------------------
// action.yml
// ---------------------------------------------------------------------------

#[test]
fn action_yml_resolved_at_commit_sha_with_yaml_fallback() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml?", "contents_action_yml_404")
        .route("contents/action.yaml?", "contents_action_yaml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    let r = &obs.refs()[0];
    assert_eq!(r.action_yml_sha(), Some(ACTION_YML));

    let contents = transport.requested_containing("/contents/");
    assert!(
        contents
            .iter()
            .any(|t| t.contains("action.yml") && t.contains(&format!("ref={COMMIT_1}"))),
        "must resolve at commit SHA, never the moving ref name: {contents:?}"
    );
    assert!(
        contents.iter().any(|t| t.contains("action.yaml")),
        "fallback to action.yaml: {contents:?}"
    );
}

#[test]
fn missing_action_yml_and_yaml_records_none_not_failure() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport);
    let obs = h.resolve(None);
    assert!(matches!(obs.outcome(), Outcome::Ok { .. }));
    assert!(obs.refs()[0].action_yml_sha().is_none());
}

#[test]
fn absent_action_yml_at_commit_is_cached_and_never_re_fetched() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport.clone());
    let _ = h.resolve(None);
    transport.clear_log();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight");
    let _ = h.resolve(None);
    assert!(
        transport.requested_containing("/contents/").is_empty(),
        "negative action.yml result must be cached by commit sha: {:?}",
        transport.requested_targets()
    );
}

#[test]
fn subdirectory_action_path_resolves_metadata_under_path() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/actions/foo/action.yml", "contents_subdir_yml");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(Some("actions/foo"));
    assert_eq!(obs.action_path(), Some("actions/foo"));
    assert_eq!(obs.refs()[0].action_yml_sha(), Some(ACTION_YML));
    let contents = transport.requested_containing("/contents/");
    assert!(
        contents
            .iter()
            .any(|t| t.contains("contents/actions/foo/action.yml")),
        "{contents:?}"
    );
}

// ---------------------------------------------------------------------------
// Repository identity
// ---------------------------------------------------------------------------

#[test]
fn redirect_is_recorded_and_not_followed() {
    let transport = MockTransport::new();
    transport.route("/repos/acme/widgets", "repo_redirect");
    // If the client followed, it would request repositories/999001 — assert it does not.
    transport.route("repositories/999001", "repo_ok");

    let mut h = Harness::new(transport.clone());
    let obs = h.resolve(None);
    assert_eq!(
        obs.redirect_location(),
        Some("https://api.github.com/repositories/999001")
    );
    assert!(
        matches!(
            obs.outcome(),
            Outcome::Failed {
                http_status: 301,
                ..
            }
        ),
        "{:?}",
        obs.outcome()
    );
    assert!(
        transport
            .requested_containing("repositories/999001")
            .is_empty(),
        "must not follow the rename/transfer redirect: {:?}",
        transport.requested_targets()
    );
}

#[test]
fn archived_repo_is_recorded_and_still_polled() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_archived")
        .route("matching-refs/tags", "tags_lightweight")
        .route(&format!("/git/commits/{COMMIT_1}"), "git_commit_1")
        .route("contents/action.yml", "contents_neither_yml")
        .route("contents/action.yaml", "contents_neither_yaml");

    let mut h = Harness::new(transport);
    let obs = h.resolve(None);
    assert_eq!(obs.archived(), Some(true));
    assert!(
        matches!(obs.outcome(), Outcome::Ok { refs, .. } if !refs.is_empty()),
        "archived repos keep polling: {:?}",
        obs.outcome()
    );
}

#[test]
fn matching_refs_422_is_api_client_not_network() {
    let transport = MockTransport::new();
    transport
        .route("/repos/tj-actions/changed-files", "repo_tj_changed_files")
        .route("matching-refs/tags", "matching_refs_422");
    let mut h = Harness::new(transport);
    h.repo = RepoSlug::parse("tj-actions/changed-files").unwrap();
    let obs = h.resolve(None);
    match obs.outcome() {
        Outcome::Failed {
            http_status,
            error_class,
            ..
        } => {
            assert_eq!(*http_status, 422);
            assert_eq!(*error_class, ErrorClass::ApiClient);
        }
        other => panic!("expected Failed/ApiClient, got {other:?}"),
    }
}

#[test]
fn matching_refs_503_is_api_server() {
    let transport = MockTransport::new();
    transport
        .route("/repos/acme/widgets", "repo_ok")
        .route("matching-refs/tags", "matching_refs_503");
    let mut h = Harness::new(transport);
    let obs = h.resolve(None);
    match obs.outcome() {
        Outcome::Failed {
            http_status,
            error_class,
            ..
        } => {
            assert_eq!(*http_status, 503);
            assert_eq!(*error_class, ErrorClass::ApiServer);
        }
        other => panic!("expected Failed/ApiServer, got {other:?}"),
    }
}

#[test]
fn transport_err_is_network_with_status_zero() {
    #[derive(Clone)]
    struct Boom;
    impl Transport for Boom {
        fn send(&self, _request: &RestRequest) -> Result<RestResponse, String> {
            Err("dns failure: no such host".into())
        }
    }
    let dir = TempDir::new().unwrap();
    let mut etags = ETagStore::open(dir.path().join("etags.jsonl"), Duration::hours(24)).unwrap();
    let mut pages = PageBodyCache::open(dir.path().join("pages.jsonl")).unwrap();
    let mut repo_meta = RepoMetaCache::open(dir.path().join("repo_meta.jsonl")).unwrap();
    let mut objects = ObjectCache::open(dir.path().join("objects.jsonl")).unwrap();
    let client = Client::new(Boom, AuthToken::new("ghp_test_token").unwrap());
    let repo = RepoSlug::parse("acme/widgets").unwrap();
    let obs = resolve_repo(
        &repo,
        None,
        &mut etags,
        &mut pages,
        &mut repo_meta,
        &mut objects,
        &client,
        now(),
    );
    match obs.outcome() {
        Outcome::Failed {
            http_status,
            error_class,
            ..
        } => {
            assert_eq!(*http_status, 0);
            assert_eq!(*error_class, ErrorClass::Network);
        }
        other => panic!("expected Failed/Network status 0, got {other:?}"),
    }
}
