//! ETag store, conditional requests, and the two rate-limit budgets.
//!
//! A 304 exempts the primary limit only when the request carried Authorization.
//! A 304 still costs one secondary point, and that budget is not in any header.

use proptest::prelude::*;
use refledger_poller::github::etag::{
    AuthToken, ConditionalRequest, ETagError, ETagStore, Request,
};
use refledger_poller::github::ratelimit::{
    backoff_delay, classify_refusal, parse_rate_limit_headers, ConcurrencyCap, InFlight,
    PointLedger, PrimaryPoints, RateLimitError, SecondaryBudget, SecondaryPoints,
};
use refledger_poller::observation::{
    ETag, Method, Outcome, RefreshReason, RepoSlug, SecondaryLimitEvent,
};
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

fn repo() -> RepoSlug {
    RepoSlug::parse("acme/widgets").expect("slug")
}

fn token() -> AuthToken {
    AuthToken::new("ghp_example").expect("token")
}

fn endpoint(query: &str) -> refledger_poller::github::etag::EndpointKey {
    ConditionalRequest::endpoint("/repos/{owner}/{repo}/git/refs/tags", query).expect("endpoint")
}

fn open_store(dir: &TempDir, max_age: Duration) -> ETagStore {
    ETagStore::open(dir.path().join("etags.jsonl"), max_age).expect("open")
}

const TEMPLATE: &str = "/repos/{owner}/{repo}/git/refs/tags";

#[test]
fn distinct_query_strings_do_not_share_an_etag() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = open_store(&dir, Duration::hours(24));
    let now = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let per_100 = endpoint("per_page=100");
    let per_50 = endpoint("per_page=50");
    let slug = repo();

    store
        .put(&slug, &per_100, &ETag::new("\"page-size-100\""), now)
        .expect("put");
    store
        .put(&slug, &per_50, &ETag::new("\"page-size-50\""), now)
        .expect("put");

    assert_eq!(
        store
            .get(&slug, &per_100, now)
            .map(|e| e.as_str().to_owned()),
        Some("\"page-size-100\"".into())
    );
    assert_eq!(
        store
            .get(&slug, &per_50, now)
            .map(|e| e.as_str().to_owned()),
        Some("\"page-size-50\"".into())
    );
    assert_ne!(per_100.request_target(), per_50.request_target());
}

#[test]
fn etags_are_stored_per_page_not_per_collection() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = open_store(&dir, Duration::hours(24));
    let now = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let page1 = endpoint("per_page=100&page=1");
    let page2 = endpoint("per_page=100&page=2");
    let slug = repo();

    store
        .put(&slug, &page1, &ETag::new("W/\"page-1\""), now)
        .expect("put page 1");

    assert!(
        store.get(&slug, &page2, now).is_none(),
        "a 304 on page 1 says nothing about page 2"
    );
    assert_eq!(
        store.get(&slug, &page1, now).map(|e| e.as_str().to_owned()),
        Some("W/\"page-1\"".into())
    );

    let other = RepoSlug::parse("other/repo").expect("slug");
    assert!(store.get(&other, &page1, now).is_none());
}

#[test]
fn store_survives_restart_and_rejects_a_corrupt_file() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("etags.jsonl");
    let now = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let max_age = Duration::hours(48);
    let slug = repo();
    let page1 = endpoint("page=1");
    let page2 = endpoint("page=2");

    {
        let mut store = ETagStore::open(&path, max_age).expect("open");
        store
            .put(&slug, &page1, &ETag::new("W/\"kept\""), now)
            .expect("put");
        store
            .put(&slug, &page2, &ETag::new("\"also-kept\""), now)
            .expect("put");
    }

    {
        let store = ETagStore::open(&path, max_age).expect("reload");
        assert_eq!(
            store.get(&slug, &page1, now).map(|e| e.as_str().to_owned()),
            Some("W/\"kept\"".into())
        );
        assert_eq!(
            store.get(&slug, &page2, now).map(|e| e.as_str().to_owned()),
            Some("\"also-kept\"".into())
        );
    }

    std::fs::write(&path, "{\"repo\":\"acme/widgets\"}\n").expect("corrupt");
    let err = ETagStore::open(&path, max_age).expect_err("corrupt store must not open");
    assert!(
        matches!(err, ETagError::CorruptStore(_)),
        "starting empty would look like a working poller, got {err:?}"
    );

    std::fs::write(
        &path,
        "{\"repo\":\"acme/widgets\",\"template\":\"/repos/{owner}/{repo}/git/refs/tags\",\"query\":\"page=1\",\"etag\":\"W/\\\"kept\\\"\",\"stored_at\":\"2026-09-21T12:00:00.000Z\"}\nNOT-JSON\n",
    )
    .expect("mixed");
    let err = ETagStore::open(&path, max_age).expect_err("partial journal is still corrupt");
    assert!(matches!(err, ETagError::CorruptStore(_)), "got {err:?}");
}

#[test]
fn expired_etag_forces_an_unconditional_request_recorded_on_the_observation() {
    let dir = TempDir::new().expect("tempdir");
    let max_age = Duration::hours(24);
    let mut store = open_store(&dir, max_age);
    let stored_at = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let slug = repo();
    let page = endpoint("page=1");
    let etag = ETag::new("W/\"stale\"");
    store.put(&slug, &page, &etag, stored_at).expect("put");

    let still_fresh = stored_at + max_age;
    assert!(store.get(&slug, &page, still_fresh).is_some());

    let expired_at = stored_at + max_age + Duration::milliseconds(1);
    assert!(
        store.get(&slug, &page, expired_at).is_none(),
        "a validator older than max age is discarded"
    );

    let request = ConditionalRequest::at(slug.clone(), expired_at)
        .build(&page, Some(&token()), &store)
        .expect("build");
    assert!(request.if_none_match_header().is_none());
    assert_eq!(request.http_method(), "GET");
    assert_eq!(request.refresh_reason(), Some(RefreshReason::EtagMaxAge));
    assert!(request.authorization_header().starts_with("Bearer "));

    let obs = request
        .to_observation(
            expired_at,
            Outcome::Ok {
                http_status: 200,
                etag: Some(ETag::new("W/\"fresh\"")),
                refs: Vec::new(),
            },
            Some(4_000),
            None,
        )
        .expect("observation");
    assert_eq!(obs.method(), Method::Rest);
    assert_eq!(
        obs.refresh_reason(),
        Some(RefreshReason::EtagMaxAge),
        "a coverage audit has to see why a 200 appeared where a 304 was expected"
    );
    let wire = serde_json::to_value(&obs).expect("json");
    assert_eq!(
        wire.get("refresh_reason").and_then(|v| v.as_str()),
        Some("etag_max_age")
    );
}

#[test]
fn conditional_request_requires_auth_and_sends_the_validator_unmodified() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = open_store(&dir, Duration::hours(24));
    let now = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let slug = repo();
    let page = endpoint("page=1");
    let weak = "W/\"67ab43\"";
    store.put(&slug, &page, &ETag::new(weak), now).expect("put");

    let err = ConditionalRequest::at(slug.clone(), now)
        .build(&page, None, &store)
        .expect_err("missing token");
    assert!(
        matches!(err, ETagError::NoAuthToken),
        "an unauthenticated conditional request must be unrepresentable, got {err:?}"
    );
    assert!(
        AuthToken::new("").is_err() && AuthToken::new("   ").is_err(),
        "a blank token is absence, not a token"
    );

    let request = ConditionalRequest::at(slug, now)
        .build(&page, Some(&token()), &store)
        .expect("authorized");
    assert!(
        request.authorization_header().starts_with("Bearer "),
        "conditional requests carry Authorization, got {}",
        request.authorization_header()
    );
    assert!(request.authorization_header().contains(token().as_str()));
    assert_eq!(request.if_none_match_header(), Some(weak));
    assert_eq!(
        request.parsed_validator().as_ref().map(ETag::as_str),
        Some(weak)
    );
    assert_eq!(request.refresh_reason(), None);
    assert_eq!(request.request_target(), format!("{TEMPLATE}?page=1"));
}

#[test]
fn response_200_updates_etag_and_304_does_not_clear_it() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = open_store(&dir, Duration::hours(24));
    let t0 = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let t1 = odt(2026, Month::September, 21, 13, 0, 0, 0);
    let slug = repo();
    let page = endpoint("page=1");
    let original = ETag::new("W/\"original\"");
    store.put(&slug, &page, &original, t0).expect("put");

    store
        .apply_response(&slug, &page, 304, None, t1)
        .expect("304 without etag");
    assert_eq!(
        store.get(&slug, &page, t1).map(|e| e.as_str().to_owned()),
        Some("W/\"original\"".into()),
        "a 304 with no etag header must not clear the stored validator"
    );

    store
        .apply_response(&slug, &page, 304, Some(&ETag::new("W/\"other\"")), t1)
        .expect("304 with a different etag");
    assert_eq!(
        store.get(&slug, &page, t1).map(|e| e.as_str().to_owned()),
        Some("W/\"original\"".into()),
        "a 304 leaves the stored validator unchanged"
    );

    store
        .apply_response(&slug, &page, 200, Some(&ETag::new("W/\"next\"")), t1)
        .expect("200");
    assert_eq!(
        store.get(&slug, &page, t1).map(|e| e.as_str().to_owned()),
        Some("W/\"next\"".into())
    );
}

#[test]
fn a_304_charges_secondary_only_and_a_200_charges_both() {
    let mut only_304 = PointLedger::new();
    only_304.record(304);
    assert_eq!(only_304.primary_points_used(), PrimaryPoints::new(0));
    assert_eq!(only_304.secondary_points_used(), SecondaryPoints::new(1));

    let mut both = PointLedger::new();
    both.record(200);
    assert_eq!(both.primary_points_used(), PrimaryPoints::new(1));
    assert_eq!(both.secondary_points_used(), SecondaryPoints::new(1));

    both.record(304);
    assert_eq!(both.primary_points_used(), PrimaryPoints::new(1));
    assert_eq!(both.secondary_points_used(), SecondaryPoints::new(2));
}

#[test]
fn remaining_header_is_primary_only_and_secondary_budget_is_unobservable() {
    let parsed = parse_rate_limit_headers([
        ("X-RateLimit-Limit", "5000"),
        ("X-RateLimit-Remaining", "4999"),
        ("X-RateLimit-Used", "1"),
    ]);
    assert_eq!(parsed.primary_remaining(), Some(PrimaryPoints::new(4999)));
    assert_eq!(parsed.secondary_budget(), SecondaryBudget::Unobservable);

    let absent = parse_rate_limit_headers([("X-RateLimit-Limit", "5000")]);
    assert_eq!(absent.primary_remaining(), None);
    assert_eq!(
        absent.secondary_budget(),
        SecondaryBudget::Unobservable,
        "no secondary header is not zero and not unlimited"
    );

    let garbage = parse_rate_limit_headers([("x-ratelimit-remaining", "many")]);
    assert_eq!(garbage.primary_remaining(), None);
    assert_eq!(garbage.secondary_budget(), SecondaryBudget::Unobservable);
}

#[test]
fn retry_after_is_honoured_and_the_no_header_case_waits_at_least_a_minute() {
    assert_eq!(
        backoff_delay(Some(Duration::seconds(0)), 3, Duration::seconds(5)).expect("zero"),
        Duration::seconds(0)
    );
    assert_eq!(
        backoff_delay(Some(Duration::seconds(5)), 4, Duration::seconds(30)).expect("short"),
        Duration::seconds(5)
    );
    assert_eq!(
        backoff_delay(None, 0, Duration::seconds(0)).expect("first"),
        Duration::seconds(60)
    );
    assert_eq!(
        backoff_delay(None, 1, Duration::seconds(0)).expect("second"),
        Duration::seconds(120)
    );
    assert_eq!(
        backoff_delay(None, 2, Duration::seconds(7)).expect("jitter"),
        Duration::seconds(240 + 7)
    );
    assert!(backoff_delay(None, 0, Duration::seconds(-1)).is_err());
}

#[test]
fn concurrency_defaults_to_20_and_stays_under_the_documented_ceiling() {
    let cap = ConcurrencyCap::default();
    assert_eq!(cap.max_in_flight(), 20);
    assert!(cap.max_in_flight() < ConcurrencyCap::DOCUMENTED_CEILING);

    assert!(ConcurrencyCap::new(0).is_err());
    assert!(matches!(
        ConcurrencyCap::new(100),
        Err(RateLimitError::ConcurrencyCap(100))
    ));
    assert_eq!(ConcurrencyCap::new(20).expect("20").max_in_flight(), 20);

    let gate = InFlight::new(cap);
    for _ in 0..20 {
        gate.try_acquire().expect("under the cap");
    }
    assert!(matches!(
        gate.try_acquire(),
        Err(RateLimitError::ConcurrencySaturated)
    ));
    gate.release();
    gate.try_acquire().expect("a released slot");
}

#[test]
fn every_403_and_429_becomes_a_secondary_limit_observation() {
    let when = odt(2026, Month::September, 21, 12, 0, 0, 0);
    for status in [403u16, 429u16] {
        let hit = classify_refusal(status, Some(Duration::seconds(15)), 940, 0, Duration::ZERO)
            .expect("refusal");
        assert_eq!(
            hit.event,
            SecondaryLimitEvent {
                status,
                retry_after: Some(Duration::seconds(15)),
                request_rate_rpm: 940,
            }
        );
        assert_eq!(hit.backoff, Duration::seconds(15));
        let obs = hit
            .observation("acme/widgets", when, Some(12))
            .expect("obs");
        assert_eq!(obs.secondary_limit_observed(), Some(&hit.event));
        assert_eq!(obs.http_status(), Some(status));
        assert_eq!(obs.method(), Method::Rest);
    }

    let no_header = classify_refusal(429, None, 1100, 0, Duration::seconds(0)).expect("429");
    assert!(no_header.event.retry_after.is_none());
    assert!(no_header.backoff >= Duration::seconds(60));
    let obs = no_header
        .observation("acme/widgets", when, None)
        .expect("obs without retry-after");
    assert!(obs.secondary_limit_observed().is_some());

    assert!(matches!(
        classify_refusal(200, None, 10, 0, Duration::seconds(0)),
        Err(RateLimitError::NotALimitStatus(200))
    ));
}

#[test]
fn ui_primary_plus_secondary_fails_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/etag_ui/primary_plus_secondary.rs");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_points_track_200s_and_total(
        statuses in prop::collection::vec(prop::sample::select(vec![200u16, 304u16]), 0..40)
    ) {
        let mut ledger = PointLedger::new();
        for status in &statuses {
            ledger.record(*status);
        }
        let oks = statuses.iter().filter(|status| **status == 200).count() as u32;
        prop_assert_eq!(ledger.primary_points_used().get(), oks);
        prop_assert_eq!(ledger.secondary_points_used().get(), statuses.len() as u32);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_stored_validator_round_trips_byte_for_byte(
        body in "[A-Za-z0-9]{1,24}",
        weak in proptest::bool::ANY
    ) {
        let raw = if weak {
            format!("W/\"{body}\"")
        } else {
            format!("\"{body}\"")
        };
        let dir = TempDir::new().expect("tempdir");
        let mut store = open_store(&dir, Duration::days(30));
        let now = odt(2026, Month::September, 21, 12, 0, 0, 0);
        let slug = repo();
        let page = endpoint("page=1");
        store.put(&slug, &page, &ETag::new(raw.clone()), now).expect("put");

        let reloaded = ETagStore::open(dir.path().join("etags.jsonl"), Duration::days(30)).expect("reload");
        let request: Request = ConditionalRequest::at(slug, now)
            .build(&page, Some(&token()), &reloaded)
            .expect("build");
        prop_assert_eq!(request.if_none_match_header(), Some(raw.as_str()));
        let parsed = request.parsed_validator();
        prop_assert_eq!(parsed.as_ref().map(ETag::as_str), Some(raw.as_str()));
    }
}
