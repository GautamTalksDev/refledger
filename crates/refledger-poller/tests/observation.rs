//! Observation model tests (spec §9.1).
//!
//! Written before the implementation. The non-negotiable design rule under
//! test: an observation records the CONDITIONS of the observation, not just
//! its result — including observations where nothing changed and ones that
//! failed. The type system must make a conditions-free observation impossible.

use proptest::prelude::*;
use serde_json::Value;
use refledger_poller::observation::{
    store_observation_at, ErrorClass, Method, Observation, ObservedRef, Outcome, RefType,
    SecondaryLimitEvent, SkipReason,
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

fn sha(digit: char) -> String {
    std::iter::repeat(digit).take(40).collect()
}

fn object_keys(value: &Value) -> Vec<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn base_observation(outcome: Outcome) -> Observation {
    Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(odt(2026, Month::September, 21, 12, 0, 0, 0))
        .unwrap()
        .method(Method::Rest)
        .outcome(outcome)
        .build()
        .expect("observation")
}

#[test]
fn observation_200_serialises_exact_key_set() {
    let refs = vec![
        ObservedRef::new_lightweight("refs/tags/v1", sha('a'), sha('b')).expect("lightweight"),
    ];

    let obs = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(odt(2026, Month::September, 21, 12, 0, 0, 0))
        .unwrap()
        .method(Method::Rest)
        .rate_limit_remaining(4_900)
        .outcome(Outcome::Ok {
            http_status: 200,
            etag: Some(refledger_poller::observation::ETag::new("W/\"abc\"")),
            refs,
        })
        .build()
        .expect("observation");

    let value = serde_json::to_value(&obs).expect("serialise");
    let mut keys = object_keys(&value);
    keys.sort();
    assert_eq!(
        keys,
        [
            "method",
            "observation_id",
            "observed_at",
            "outcome",
            "poller_version",
            "rate_limit_remaining",
            "repo",
        ]
    );
    assert_eq!(
        value.get("poller_version").and_then(|v| v.as_str()),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let _ = RefType::Lightweight;
}

#[test]
fn annotated_tag_rejects_commit_sha_equal_target_sha() {
    let err = ObservedRef::new_annotated(
        "refs/tags/v1",
        sha('a'), // target — tag object oid
        sha('a'), // commit — must differ for annotated
        sha('b'), // tree
    )
    .expect_err("annotated requires target_sha != commit_sha");
    assert!(
        matches!(
            err,
            refledger_poller::observation::ObservationError::AnnotatedShaCollision
        ),
        "got {err:?}"
    );
}

#[test]
fn annotated_tag_accepts_distinct_target_and_commit() {
    let r = ObservedRef::new_annotated("refs/tags/v1", sha('a'), sha('c'), sha('b'))
        .expect("annotated with distinct shas");
    assert_ne!(r.target_sha(), r.commit_sha().unwrap());
    assert_eq!(r.ref_type(), RefType::Annotated);
}

#[test]
fn lightweight_tag_sets_target_equal_commit_from_one_sha() {
    let r = ObservedRef::new_lightweight("refs/tags/v1", sha('a'), sha('b')).expect("one sha");
    assert_eq!(r.target_sha(), r.commit_sha().unwrap());
    assert_eq!(r.target_sha(), sha('a'));
    assert_eq!(r.tree_sha().unwrap(), sha('b'));
    assert_eq!(r.ref_type(), RefType::Lightweight);
}

#[test]
fn not_modified_304_is_not_error_and_extends_binding() {
    let prior = base_observation(Outcome::Ok {
        http_status: 200,
        etag: Some(refledger_poller::observation::ETag::new("W/\"stable-etag\"")),
        refs: vec![ObservedRef::new_lightweight("refs/tags/v1", sha('a'), sha('b')).unwrap()],
    });

    let not_mod = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(odt(2026, Month::September, 21, 13, 0, 0, 0))
        .unwrap()
        .method(Method::Rest)
        .rate_limit_remaining(4_800)
        .outcome(Outcome::NotModified {
            http_status: 304,
            etag: refledger_poller::observation::ETag::new("W/\"stable-etag\""),
        })
        .build()
        .expect("304 observation");

    assert!(not_mod.outcome().is_not_modified());
    assert!(not_mod.refs().is_empty());
    assert_eq!(
        not_mod.etag().map(str::to_owned),
        Some("W/\"stable-etag\"".into())
    );
    assert!(!not_mod.is_error());

    // A 304 still counts toward observation_count and still extends last_observed.
    // Without this, the 148-day stability figure that makes §6.3 meaningful is unprovable.
    let timeline = refledger_poller::observation::reconstruct_bindings(&[prior, not_mod.clone()]);
    let b = timeline.get("refs/tags/v1").expect("binding");
    assert_eq!(
        b.observation_count(),
        2,
        "304 must count toward observation_count"
    );
    assert_eq!(
        b.last_observed(),
        not_mod.observed_at(),
        "304 must extend last_observed"
    );
}

#[test]
fn failed_outcome_serialises_and_is_queryable() {
    let secondary = SecondaryLimitEvent {
        status: 403,
        retry_after: Some(Duration::seconds(60)),
        request_rate_rpm: 920,
    };
    let obs = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(odt(2026, Month::September, 21, 12, 0, 0, 0))
        .unwrap()
        .method(Method::Rest)
        .rate_limit_remaining(0)
        .secondary_limit_observed(secondary)
        .outcome(Outcome::Failed {
            http_status: 403,
            error_class: ErrorClass::SecondaryRateLimit,
            backoff_applied: Duration::seconds(60),
        })
        .build()
        .expect("failed observation");

    let value = serde_json::to_value(&obs).expect("serialise");
    assert!(value.get("outcome").is_some());
    assert!(obs.is_error());
    assert_eq!(obs.outcome().http_status(), Some(403));
    assert_eq!(obs.secondary_limit_observed().map(|s| s.status), Some(403));
}

#[test]
fn skipped_outcome_is_written_not_dropped() {
    let dir = TempDir::new().expect("tempdir");
    for reason in [
        SkipReason::BudgetExhausted,
        SkipReason::SecondaryLimitBackoff,
        SkipReason::ShutdownMidSweep,
    ] {
        let obs = base_observation(Outcome::Skipped { reason });
        assert!(
            store_observation_at(&obs, dir.path()).expect("persist"),
            "a skipped poll that leaves no record is an invisible hole in the coverage claim"
        );
        let wire = serde_json::to_string(&obs).expect("serialise");
        assert!(
            wire.to_lowercase().contains("skip"),
            "skip must serialise to storage, got {wire}"
        );
    }
}

#[test]
fn secondary_limit_required_on_403_and_429_failures() {
    for status in [403u16, 429u16] {
        let err = Observation::builder()
            .repo("acme/widgets")
            .unwrap()
            .observed_at(odt(2026, Month::September, 21, 12, 0, 0, 0))
            .unwrap()
            .method(Method::Rest)
            .outcome(Outcome::Failed {
                http_status: status,
                error_class: ErrorClass::SecondaryRateLimit,
                backoff_applied: Duration::seconds(30),
            })
            .build()
            .expect_err("403/429 Failed must carry SecondaryLimitEvent");
        assert!(
            matches!(
                err,
                refledger_poller::observation::ObservationError::MissingSecondaryLimit
            ),
            "got {err:?}"
        );

        let ok = Observation::builder()
            .repo("acme/widgets")
            .unwrap()
            .observed_at(odt(2026, Month::September, 21, 12, 0, 0, 0))
            .unwrap()
            .method(Method::Rest)
            .secondary_limit_observed(SecondaryLimitEvent {
                status,
                retry_after: Some(Duration::seconds(30)),
                request_rate_rpm: 950,
            })
            .outcome(Outcome::Failed {
                http_status: status,
                error_class: ErrorClass::SecondaryRateLimit,
                backoff_applied: Duration::seconds(30),
            })
            .build()
            .expect("with secondary limit event");
        assert!(ok.secondary_limit_observed().is_some());
    }
}

#[test]
fn poller_version_is_crate_version_not_a_literal_setter() {
    let obs = base_observation(Outcome::Skipped {
        reason: SkipReason::BudgetExhausted,
    });
    assert_eq!(obs.poller_version().as_str(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn ui_observation_without_observed_at_or_method_fails_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/observation_ui/missing_observed_at.rs");
    t.compile_fail("tests/observation_ui/missing_method.rs");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_observation_round_trip(obs in arb_observation()) {
        let bytes = serde_json::to_vec(&obs).expect("ser");
        let back: Observation = serde_json::from_slice(&bytes).expect("de");
        prop_assert_eq!(back, obs);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn prop_binding_timeline_deterministic_and_counts_304s(
        events in prop::collection::vec(arb_timeline_event(), 1..20)
    ) {
        let observations = materialise_timeline("acme/widgets", &events);
        let a = refledger_poller::observation::reconstruct_bindings(&observations);
        let b = refledger_poller::observation::reconstruct_bindings(&observations);
        prop_assert_eq!(&a, &b, "reconstructing the binding timeline must be deterministic");

        for (name, binding) in &a {
            let appeared = observations
                .iter()
                .filter(|o| ref_appears_in(o, name, &observations))
                .count() as u64;
            prop_assert_eq!(
                binding.observation_count(),
                appeared,
                "ref {}: observation_count must equal appearances including 304s",
                name
            );
        }
    }
}

fn ref_appears_in(obs: &Observation, name: &str, all: &[Observation]) -> bool {
    match obs.outcome() {
        Outcome::Ok { refs, .. } => refs.iter().any(|r| r.name() == name),
        Outcome::NotModified { .. } => {
            // A 304 continues every ref last seen in the most recent Ok.
            all.iter()
                .filter(|o| o.observed_at() <= obs.observed_at())
                .rev()
                .find(|o| matches!(o.outcome(), Outcome::Ok { .. }))
                .is_some_and(|ok| ok.refs().iter().any(|r| r.name() == name))
        }
        _ => false,
    }
}

#[derive(Debug, Clone)]
enum TimelineEvent {
    OkRefs(Vec<String>),
    NotModified,
    Failed,
    Skipped,
}

fn arb_timeline_event() -> impl Strategy<Value = TimelineEvent> {
    prop_oneof![
        prop::collection::vec("[a-z]{1,4}", 1..3).prop_map(|names| {
            TimelineEvent::OkRefs(
                names
                    .into_iter()
                    .map(|n| format!("refs/tags/{n}"))
                    .collect(),
            )
        }),
        Just(TimelineEvent::NotModified),
        Just(TimelineEvent::Failed),
        Just(TimelineEvent::Skipped),
    ]
}

fn materialise_timeline(repo: &str, events: &[TimelineEvent]) -> Vec<Observation> {
    let mut out = Vec::new();
    let mut t = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let mut last_etag = "W/\"e0\"".to_string();

    for (i, ev) in events.iter().enumerate() {
        t += Duration::hours(1);

        let obs = match ev {
            TimelineEvent::OkRefs(names) => {
                let refs: Vec<_> = names
                    .iter()
                    .map(|n| ObservedRef::new_lightweight(n, sha('a'), sha('b')).expect("lw"))
                    .collect();
                last_etag = format!("W/\"e{i}\"");
                Observation::builder()
                    .repo(repo)
                    .unwrap()
                    .observed_at(t)
                    .unwrap()
                    .method(Method::Rest)
                    .rate_limit_remaining(5_000)
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(refledger_poller::observation::ETag::new(last_etag.clone())),
                        refs,
                    })
                    .build()
                    .unwrap()
            }
            TimelineEvent::NotModified => Observation::builder()
                .repo(repo)
                .unwrap()
                .observed_at(t)
                .unwrap()
                .method(Method::Rest)
                .rate_limit_remaining(5_000)
                .outcome(Outcome::NotModified {
                    http_status: 304,
                    etag: refledger_poller::observation::ETag::new(last_etag.clone()),
                })
                .build()
                .unwrap(),
            TimelineEvent::Failed => Observation::builder()
                .repo(repo)
                .unwrap()
                .observed_at(t)
                .unwrap()
                .method(Method::Rest)
                .rate_limit_remaining(5_000)
                .outcome(Outcome::Failed {
                    http_status: 500,
                    error_class: ErrorClass::Upstream,
                    backoff_applied: Duration::seconds(5),
                })
                .build()
                .unwrap(),
            TimelineEvent::Skipped => Observation::builder()
                .repo(repo)
                .unwrap()
                .observed_at(t)
                .unwrap()
                .method(Method::Rest)
                .outcome(Outcome::Skipped {
                    reason: SkipReason::BudgetExhausted,
                })
                .build()
                .unwrap(),
        };
        out.push(obs);
    }
    out
}

fn arb_observation() -> impl Strategy<Value = Observation> {
    (
        any::<u16>(),
        proptest::string::string_regex("W/\"[a-z0-9]{1,8}\"").unwrap(),
        any::<u32>(),
        prop::collection::vec(arb_lightweight_ref(), 0..4),
        prop::bool::ANY,
        prop::sample::select(vec![Method::Rest, Method::GraphQl]),
    )
        .prop_map(|(n, etag, remaining, refs, prefer_ok, method)| {
            let (status, outcome_kind) = if prefer_ok && !refs.is_empty() {
                (200u16, 0u8)
            } else if n % 5 == 0 {
                (304, 1)
            } else if n % 5 == 1 {
                (403, 2)
            } else if n % 5 == 2 {
                (429, 2)
            } else if n % 5 == 3 {
                (500, 3)
            } else {
                (200, 0)
            };

            let base = Observation::builder()
                .repo("acme/widgets")
                .unwrap()
                .observed_at(odt(2026, Month::March, 1, 0, 0, 0, 0))
                .unwrap()
                .method(method)
                .rate_limit_remaining(remaining);

            match outcome_kind {
                1 => base
                    .outcome(Outcome::NotModified {
                        http_status: 304,
                        etag: refledger_poller::observation::ETag::new(etag),
                    })
                    .build()
                    .expect("arb observation"),
                2 => base
                    .secondary_limit_observed(SecondaryLimitEvent {
                        status,
                        retry_after: Some(Duration::seconds(10)),
                        request_rate_rpm: 900,
                    })
                    .outcome(Outcome::Failed {
                        http_status: status,
                        error_class: ErrorClass::SecondaryRateLimit,
                        backoff_applied: Duration::seconds(10),
                    })
                    .build()
                    .expect("arb observation"),
                3 => base
                    .outcome(Outcome::Failed {
                        http_status: status,
                        error_class: ErrorClass::Upstream,
                        backoff_applied: Duration::seconds(5),
                    })
                    .build()
                    .expect("arb observation"),
                _ => base
                    .outcome(Outcome::Ok {
                        http_status: 200,
                        etag: Some(refledger_poller::observation::ETag::new(etag)),
                        refs,
                    })
                    .build()
                    .expect("arb observation"),
            }
        })
}

fn arb_lightweight_ref() -> impl Strategy<Value = ObservedRef> {
    proptest::string::string_regex("refs/tags/[a-z]{1,6}")
        .unwrap()
        .prop_map(|name| ObservedRef::new_lightweight(name, sha('f'), sha('e')).unwrap())
}
