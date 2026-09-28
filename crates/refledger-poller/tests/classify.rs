//! Classification is a pure function: `(RepoState, Observation, Enrichment) ->
//! (RepoState, Vec<ClassifiedEvent>)`.
//!
//! No network, no clock, no randomness. Every behavioural test runs twice and
//! compares output — pure classification is what makes the log replayable.

use proptest::prelude::*;
use refledger_poller::classify::{
    classify, Ancestry, BatchCorrelation, ClassifiedEvent, ClassifyError, Enrichment, MoveKind,
    RefForm, ReleaseLevelSub, RepoState, Severity,
};
use refledger_poller::enrich::{CompareCache, COMPARE_FILE_CAP};
use refledger_poller::observation::{ETag, ErrorClass, Method, Observation, ObservedRef, Outcome};
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

fn commit_a() -> String {
    sha('a')
}
fn commit_b() -> String {
    sha('b')
}
fn tree_a() -> String {
    sha('1')
}
fn tree_b() -> String {
    sha('2')
}
fn tag_obj() -> String {
    sha('d')
}

fn assert_pure(
    state: &RepoState,
    obs: &Observation,
    enrichment: &Enrichment,
) -> (RepoState, Vec<ClassifiedEvent>) {
    let (s1, e1) = classify(state, obs, enrichment).expect("classify");
    let (s2, e2) = classify(state, obs, enrichment).expect("classify replay");
    assert_eq!(e1, e2, "classify must be pure: events differ on replay");
    assert_eq!(s1, s2, "classify must be pure: state differs on replay");
    (s1, e1)
}

fn ok_obs(at: OffsetDateTime, refs: Vec<ObservedRef>) -> Observation {
    Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(at)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Ok {
            http_status: 200,
            etag: Some(ETag::new("W/\"e\"")),
            refs,
        })
        .build()
        .unwrap()
}

fn lw(name: &str, commit: &str, tree: &str) -> ObservedRef {
    ObservedRef::new_lightweight(name, commit, tree).unwrap()
}

fn ann(name: &str, tag: &str, commit: &str, tree: &str) -> ObservedRef {
    ObservedRef::new_annotated(name, tag, commit, tree).unwrap()
}

fn enrichment_ahead(old: &str, new: &str) -> Enrichment {
    Enrichment::empty().with_ancestry(old, new, Ancestry::Ahead)
}

fn enrichment_status(old: &str, new: &str, status: Ancestry) -> Enrichment {
    Enrichment::empty().with_ancestry(old, new, status)
}

// ---------------------------------------------------------------------------
// Ref form — tested first; severity and correlation depend on it.
// ---------------------------------------------------------------------------

#[test]
fn ref_form_table_from_real_population_names() {
    let cases: &[(&str, RefForm)] = &[
        ("v4", RefForm::FloatingMajor),
        ("4", RefForm::FloatingMajor),
        ("v10", RefForm::FloatingMajor),
        ("refs/tags/v3", RefForm::FloatingMajor),
        ("v4.2", RefForm::FloatingMinor),
        ("4.2", RefForm::FloatingMinor),
        ("v0.1", RefForm::FloatingMinor),
        ("v10.1", RefForm::FloatingMinor),
        ("refs/tags/v4.2", RefForm::FloatingMinor),
        ("v4.2.2", RefForm::Exact),
        ("4.2.2", RefForm::Exact),
        ("v4.2.2-rc.1", RefForm::Exact),
        ("v1.0.0", RefForm::Exact),
        ("v1.2.3-beta.1", RefForm::Exact),
        ("v0.0.1", RefForm::Exact),
        ("v23.0.0", RefForm::Exact),
        ("v2.6.11", RefForm::Exact),
        ("v4.0.0+build.1", RefForm::Exact),
        ("1.0.0", RefForm::Exact),
        ("refs/tags/v4.2.2", RefForm::Exact),
        ("main", RefForm::NamedChannel),
        ("latest", RefForm::NamedChannel),
        ("stable", RefForm::NamedChannel),
        ("refs/tags/latest", RefForm::NamedChannel),
        ("master", RefForm::Other),
        ("develop", RefForm::Other),
        ("HEAD", RefForm::Other),
        ("nightlies", RefForm::Other),
        ("release", RefForm::Other),
        ("next", RefForm::Other),
        ("canary", RefForm::Other),
        ("beta", RefForm::Other),
        ("alpha", RefForm::Other),
        ("v4.2.2.2", RefForm::Other),
        ("release-1.0", RefForm::Other),
        ("2024.01.15", RefForm::Other),
    ];
    assert!(cases.len() >= 30, "table must cover ≥30 real names");
    for (name, expected) in cases {
        assert_eq!(RefForm::parse(name), *expected, "RefForm::parse({name:?})");
    }
}

// ---------------------------------------------------------------------------
// Top-level events
// ---------------------------------------------------------------------------

#[test]
fn move_when_target_differs() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::March, 1, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ClassifiedEvent::Move {
            ref_name,
            kind: MoveKind::ContentChange,
            severity: Severity::High,
            ..
        } => assert_eq!(ref_name, "refs/tags/v1.0.0"),
        other => panic!("expected Exact ContentChange High, got {other:?}"),
    }
}

#[test]
fn deletion_only_from_ok_observation() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::February, 1, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![
            lw("refs/tags/v1", &commit_a(), &tree_a()),
            lw("refs/tags/v2", &commit_a(), &tree_a()),
        ],
    ))
    .unwrap();

    // Ok with one ref gone → deletion.
    let obs = ok_obs(t1, vec![lw("refs/tags/v1", &commit_a(), &tree_a())]);
    let (_s, events) = assert_pure(&state, &obs, &Enrichment::empty());
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        ClassifiedEvent::Deletion {
            ref_name,
            severity: Severity::Info,
            ..
        } if ref_name == "refs/tags/v2"
    ));

    // Failed / Skipped / NotModified never produce deletions.
    for outcome in [
        Outcome::Failed {
            http_status: 500,
            error_class: ErrorClass::Upstream,
            backoff_applied: Duration::seconds(0),
        },
        Outcome::Skipped {
            reason: refledger_poller::observation::SkipReason::BudgetExhausted,
        },
        Outcome::NotModified {
            http_status: 304,
            etag: ETag::new("W/\"x\""),
        },
    ] {
        let obs = Observation::builder()
            .repo("acme/widgets")
            .unwrap()
            .observed_at(t1)
            .unwrap()
            .method(Method::Rest)
            .outcome(outcome)
            .build()
            .unwrap();
        let (_s, events) = assert_pure(&state, &obs, &Enrichment::empty());
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ClassifiedEvent::Deletion { .. })),
            "non-Ok must not emit deletions: {events:?}"
        );
    }
}

#[test]
fn repo_404_with_many_tags_is_one_unavailable_not_n_deletions() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::February, 1, 0, 0, 0, 0);
    let refs: Vec<_> = (0..346)
        .map(|i| lw(&format!("refs/tags/v1.0.{i}"), &commit_a(), &tree_a()))
        .collect();
    let state = RepoState::from_ok_observation(&ok_obs(t0, refs)).unwrap();
    let obs = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(t1)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Failed {
            http_status: 404,
            error_class: ErrorClass::Upstream,
            backoff_applied: Duration::seconds(0),
        })
        .build()
        .unwrap();
    let (_s, events) = assert_pure(&state, &obs, &Enrichment::empty());
    assert_eq!(events.len(), 1, "exactly one event, not 346 deletions");
    assert!(matches!(
        &events[0],
        ClassifiedEvent::RepoUnavailable {
            http_status: 404,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Move subcategories
// ---------------------------------------------------------------------------

#[test]
fn release_level_only_when_commit_and_tree_unchanged() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    // Lightweight → annotated: new target is tag object, same commit+tree.
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(
        t1,
        vec![ann("refs/tags/v1", &tag_obj(), &commit_a(), &tree_a())],
    );
    let (_s, events) = assert_pure(&state, &obs, &Enrichment::empty());
    assert_eq!(events.len(), 1);
    match &events[0] {
        ClassifiedEvent::Move {
            kind:
                MoveKind::ReleaseLevelOnly {
                    sub: ReleaseLevelSub::LightweightToAnnotated,
                },
            severity: Severity::Info,
            ..
        } => {}
        other => panic!("expected LW→Ann ReleaseLevelOnly Info, got {other:?}"),
    }

    // Annotated → lightweight.
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![ann("refs/tags/v1", &tag_obj(), &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v1", &commit_a(), &tree_a())]);
    let (_s, events) = assert_pure(&state, &obs, &Enrichment::empty());
    match &events[0] {
        ClassifiedEvent::Move {
            kind:
                MoveKind::ReleaseLevelOnly {
                    sub: ReleaseLevelSub::AnnotatedToLightweight,
                },
            severity: Severity::Info,
            ..
        } => {}
        other => panic!("expected Ann→LW ReleaseLevelOnly Info, got {other:?}"),
    }
}

#[test]
fn commit_metadata_only_when_tree_unchanged() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    // Same tree, different commit.
    let obs = ok_obs(t1, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_a())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    match &events[0] {
        ClassifiedEvent::Move {
            kind: MoveKind::CommitMetadataOnly,
            severity: Severity::Medium,
            ..
        } => {}
        other => panic!("expected CommitMetadataOnly Medium for Exact, got {other:?}"),
    }
}

#[test]
fn content_change_when_tree_differs() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    assert!(matches!(
        &events[0],
        ClassifiedEvent::Move {
            kind: MoveKind::ContentChange,
            severity: Severity::High,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Delete-then-recreate
// ---------------------------------------------------------------------------

#[test]
fn recreation_same_target_and_different_target() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let t2 = odt(2026, Month::January, 3, 0, 0, 0, 0);

    let state0 = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v4.3.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    // Delete
    let (state1, del) = assert_pure(&state0, &ok_obs(t1, vec![]), &Enrichment::empty());
    assert!(matches!(del[0], ClassifiedEvent::Deletion { .. }));

    // Recreate same target
    let (state2, events) = assert_pure(
        &state1,
        &ok_obs(t2, vec![lw("refs/tags/v4.3.0", &commit_a(), &tree_a())]),
        &Enrichment::empty(),
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        ClassifiedEvent::Recreation {
            gap,
            same_target: true,
            severity: Severity::Info,
            ..
        } => {
            assert_eq!(gap.whole_days(), 1);
        }
        other => panic!("expected same-target Recreation, got {other:?}"),
    }

    // aws-actions v4.3.0 shape: recreate same day at a different commit.
    let t2b = odt(2026, Month::January, 2, 12, 0, 0, 0);
    let (state1b, _) = assert_pure(&state0, &ok_obs(t1, vec![]), &Enrichment::empty());
    let (state_after, events) = assert_pure(
        &state1b,
        &ok_obs(t2b, vec![lw("refs/tags/v4.3.0", &commit_b(), &tree_b())]),
        &Enrichment::empty(),
    );
    match &events[0] {
        ClassifiedEvent::Recreation {
            same_target: false,
            severity: Severity::Medium,
            from,
            to,
            ..
        } => {
            assert_eq!(from.commit_sha(), commit_a());
            assert_eq!(to.commit_sha(), commit_b());
        }
        other => panic!("expected different-target Recreation Medium, got {other:?}"),
    }
    // Stability clock must not hide the prior binding.
    let binding = state_after.binding("refs/tags/v4.3.0").expect("binding");
    assert_eq!(
        binding.first_observed(),
        state0.binding("refs/tags/v4.3.0").unwrap().first_observed(),
        "recreation must not reset first_observed to hide prior stability"
    );
    // Tombstone cleared after recreation
    assert!(state2.tombstone("refs/tags/v4.3.0").is_none());
    let _ = state_after;
}

#[test]
fn tombstone_persists_across_200_days() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t_del = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let t_rec = odt(2026, Month::July, 21, 0, 0, 0, 0); // ~200 days later
    let state0 = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let (state1, _) = assert_pure(&state0, &ok_obs(t_del, vec![]), &Enrichment::empty());
    assert!(state1.tombstone("refs/tags/v1.0.0").is_some());
    let (_s, events) = assert_pure(
        &state1,
        &ok_obs(t_rec, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]),
        &Enrichment::empty(),
    );
    match &events[0] {
        ClassifiedEvent::Recreation { gap, .. } => {
            assert!(gap.whole_days() >= 200, "gap={}", gap.whole_days());
        }
        other => panic!("expected Recreation after long tombstone, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Ancestry
// ---------------------------------------------------------------------------

#[test]
fn floating_tag_ancestry_all_four_statuses() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::June, 1, 0, 0, 0, 0); // long-stable floating tag
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v4", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v4", &commit_b(), &tree_b())]);

    for status in [
        Ancestry::Ahead,
        Ancestry::Behind,
        Ancestry::Diverged,
        Ancestry::Identical,
    ] {
        let enrich = enrichment_status(&commit_a(), &commit_b(), status);
        let (_s, events) = assert_pure(&state, &obs, &enrich);
        let ClassifiedEvent::Move {
            ancestry,
            severity: sev,
            ..
        } = &events[0]
        else {
            panic!("expected Move");
        };
        assert_eq!(*ancestry, Some(status));
        match status {
            Ancestry::Behind | Ancestry::Diverged => assert_eq!(*sev, Severity::High),
            Ancestry::Ahead => assert_eq!(*sev, Severity::Low),
            Ancestry::Identical => assert_ne!(*sev, Severity::High),
        }
    }
}

#[test]
fn routine_forward_v4_move_is_low_never_high_however_long_stable() {
    let t0 = odt(2024, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::September, 1, 0, 0, 0, 0); // ~2.5 years stable
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v4", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v4", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    match &events[0] {
        ClassifiedEvent::Move {
            severity: Severity::Low,
            kind: MoveKind::ContentChange,
            ..
        } => {}
        other => panic!("routine floating ahead must be Low, got {other:?}"),
    }
}

#[test]
fn single_exact_tag_content_change_is_high() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v4.2.2", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(t1, vec![lw("refs/tags/v4.2.2", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    assert!(matches!(
        &events[0],
        ClassifiedEvent::Move {
            severity: Severity::High,
            kind: MoveKind::ContentChange,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Batch correlation
// ---------------------------------------------------------------------------

const CORRELATION_NOTE: &str = refledger_poller::classify::CORRELATION_NOTE;

#[test]
fn normal_release_shape_does_not_trigger_correlation() {
    // v4 and v4.2 move to the commit that new exact tag v4.2.3 was created on.
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::February, 1, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![
            lw("refs/tags/v4", &commit_a(), &tree_a()),
            lw("refs/tags/v4.2", &commit_a(), &tree_a()),
        ],
    ))
    .unwrap();
    let obs = ok_obs(
        t1,
        vec![
            lw("refs/tags/v4", &commit_b(), &tree_b()),
            lw("refs/tags/v4.2", &commit_b(), &tree_b()),
            lw("refs/tags/v4.2.3", &commit_b(), &tree_b()), // new — must not count
        ],
    );
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    assert!(
        events.iter().all(|e| match e {
            ClassifiedEvent::Move { correlation, .. } => correlation.is_none(),
            _ => true,
        }),
        "normal release must not correlate: {events:?}"
    );
}

#[test]
fn trivy_shape_76_of_77_exact_tags_correlates() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::March, 15, 0, 0, 0, 0);
    let mut prior = Vec::new();
    for i in 0..77 {
        prior.push(lw(&format!("refs/tags/v0.50.{i}"), &commit_a(), &tree_a()));
    }
    let state = RepoState::from_ok_observation(&ok_obs(t0, prior)).unwrap();
    let mut next = Vec::new();
    // 76 move to commit_b; one stays.
    for i in 0..76 {
        next.push(lw(&format!("refs/tags/v0.50.{i}"), &commit_b(), &tree_b()));
    }
    next.push(lw("refs/tags/v0.50.76", &commit_a(), &tree_a()));
    let obs = ok_obs(t1, next);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    let correlated: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ClassifiedEvent::Move {
                correlation: Some(c),
                severity: Severity::High,
                ..
            } => Some(c),
            _ => None,
        })
        .collect();
    assert!(
        correlated.len() >= 76,
        "Trivy shape must correlate, got {}",
        correlated.len()
    );
    let c = correlated[0];
    assert!(c.all_to_same_target);
    assert_eq!(c.refs_moved_together.len(), 76);
    assert_eq!(c.note.as_deref(), Some(CORRELATION_NOTE));
    assert!(!c.batch_id.is_empty());
}

#[test]
fn tj_actions_shape_346_exact_tags_correlates() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::March, 15, 12, 0, 0, 0);
    let prior: Vec<_> = (0..346)
        .map(|i| lw(&format!("refs/tags/v1.{i}.0"), &commit_a(), &tree_a()))
        .collect();
    let state = RepoState::from_ok_observation(&ok_obs(t0, prior)).unwrap();
    let next: Vec<_> = (0..346)
        .map(|i| lw(&format!("refs/tags/v1.{i}.0"), &commit_b(), &tree_b()))
        .collect();
    let obs = ok_obs(t1, next);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    let moves: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ClassifiedEvent::Move {
                    correlation: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(moves.len(), 346);
}

#[test]
fn correlation_spans_consecutive_sweeps_within_window() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t2 = odt(2026, Month::January, 1, 12, 20, 0, 0); // +20 min < 30 min default
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![
            lw("refs/tags/v1.0.0", &commit_a(), &tree_a()),
            lw("refs/tags/v1.0.1", &commit_a(), &tree_a()),
            lw("refs/tags/v1.0.2", &commit_a(), &tree_a()),
        ],
    ))
    .unwrap();
    // Sweep 1: two Exact tags move
    let obs1 = ok_obs(
        t1,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_a(), &tree_a()),
        ],
    );
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (state1, e1) = assert_pure(&state, &obs1, &enrich);
    assert!(
        e1.iter().all(|e| !matches!(
            e,
            ClassifiedEvent::Move {
                correlation: Some(_),
                ..
            }
        )),
        "2 < 3 must not correlate yet"
    );
    // Sweep 2: third Exact tag joins same target within window
    let obs2 = ok_obs(
        t2,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_b(), &tree_b()),
        ],
    );
    let (_s, e2) = assert_pure(&state1, &obs2, &enrich);
    assert!(
        e2.iter().any(|e| matches!(
            e,
            ClassifiedEvent::Move {
                correlation: Some(BatchCorrelation {
                    refs_moved_together,
                    ..
                }),
                ..
            } if refs_moved_together.len() >= 3
        )),
        "third move within window must correlate: {e2:?}"
    );
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[test]
fn observation_window_required_and_zero_errors() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    // Same timestamp as last_observed → zero window.
    let obs = ok_obs(t0, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let err = classify(&state, &obs, &enrich).expect_err("zero window");
    assert!(matches!(err, ClassifyError::ZeroObservationWindow));

    let t1 = odt(2026, Month::January, 1, 1, 0, 0, 0);
    let obs = ok_obs(t1, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let (_s, events) = assert_pure(&state, &obs, &enrich);
    match &events[0] {
        ClassifiedEvent::Move {
            observation_window_seconds,
            from,
            to,
            ..
        } => {
            assert_eq!(*observation_window_seconds, 3600);
            assert_eq!(from.last_observed(), t0);
            assert_eq!(to.first_observed(), t1);
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Enrich compare truncation (documented + live-observed cap)
// ---------------------------------------------------------------------------

#[test]
fn compare_file_cap_is_observed_300() {
    // Live 2026-09-28: torvalds/linux v5.0...v6.0 returned files_len=300 and
    // no top-level `truncated` field. Docs: up to 300 changed files on page 1.
    let finding = include_str!("fixtures/enrich/compare_file_cap.json");
    assert!(finding.contains("\"files_len\": 300"));
    assert_eq!(COMPARE_FILE_CAP, 300);
    assert!(
        CompareCache::is_possibly_truncated(COMPARE_FILE_CAP),
        "exactly the cap must be treated as possibly truncated"
    );
    assert!(CompareCache::is_possibly_truncated(COMPARE_FILE_CAP + 1));
    assert!(!CompareCache::is_possibly_truncated(COMPARE_FILE_CAP - 1));
}

// ---------------------------------------------------------------------------
// Proptests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_replay_identical(
        seed in 0u64..1000,
        n_obs in 2usize..8,
    ) {
        let mut t = odt(2026, Month::January, 1, 0, 0, 0, 0);
        let mut commit = commit_a();
        let mut tree = tree_a();
        let mut observations = Vec::new();
        for i in 0..n_obs {
            t += Duration::hours(1);
            if seed % 3 == 0 && i > 0 {
                commit = if commit == commit_a() { commit_b() } else { commit_a() };
                tree = if tree == tree_a() { tree_b() } else { tree_a() };
            }
            observations.push(ok_obs(t, vec![lw("refs/tags/v1.0.0", &commit, &tree)]));
        }
        let enrich = enrichment_ahead(&commit_a(), &commit_b())
            .with_ancestry(&commit_b(), &commit_a(), Ancestry::Ahead);

        let mut state = RepoState::default();
        let mut all_events = Vec::new();
        for obs in &observations {
            let (s1, e1) = classify(&state, obs, &enrich).expect("classify");
            let (s2, e2) = classify(&state, obs, &enrich).expect("classify replay");
            prop_assert_eq!(&e1, &e2);
            prop_assert_eq!(&s1, &s2);
            all_events.extend(e1);
            state = s1;
        }

        // Same observation objects → same Ulids → identical event stream.
        let mut state_b = RepoState::default();
        let mut replayed = Vec::new();
        for obs in &observations {
            let (s, e) = classify(&state_b, obs, &enrich).unwrap();
            replayed.extend(e);
            state_b = s;
        }
        prop_assert_eq!(all_events, replayed);
    }

    #[test]
    fn prop_non_ok_only_unavailable_or_redirect(
        status in prop::sample::select(vec![304u16, 500, 403, 429, 404, 301]),
    ) {
        let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
        let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
        let state = RepoState::from_ok_observation(&ok_obs(
            t0,
            vec![lw("refs/tags/v1", &commit_a(), &tree_a())],
        )).unwrap();
        let outcome = match status {
            304 => Outcome::NotModified {
                http_status: 304,
                etag: ETag::new("W/\"x\""),
            },
            404 => Outcome::Failed {
                http_status: 404,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            301 => Outcome::Failed {
                http_status: 301,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
            s => Outcome::Failed {
                http_status: s,
                error_class: ErrorClass::Upstream,
                backoff_applied: Duration::seconds(0),
            },
        };
        let mut b = Observation::builder()
            .repo("acme/widgets")
            .unwrap()
            .observed_at(t1)
            .unwrap()
            .method(Method::Rest);
        if status == 301 {
            b = b.redirect_location("https://api.github.com/repositories/1");
        }
        if status == 403 || status == 429 {
            b = b.secondary_limit_observed(refledger_poller::observation::SecondaryLimitEvent {
                status,
                retry_after: Some(Duration::seconds(60)),
                request_rate_rpm: 900,
            });
        }
        let obs = b.outcome(outcome).build().unwrap();
        let (_s, events) = classify(&state, &obs, &Enrichment::empty()).unwrap();
        for e in &events {
            prop_assert!(
                matches!(e, ClassifiedEvent::RepoUnavailable { .. } | ClassifiedEvent::RepoRedirected { .. }),
                "non-Ok produced unexpected event {e:?}"
            );
        }
        if status == 404 {
            prop_assert_eq!(events.len(), 1);
        }
        if status == 301 {
            prop_assert!(
                events
                    .iter()
                    .any(|e| matches!(e, ClassifiedEvent::RepoRedirected { .. })),
                "expected RepoRedirected for 301"
            );
        }
        if status == 304 || status == 500 || status == 403 || status == 429 {
            prop_assert!(events.is_empty());
        }
    }

    #[test]
    fn prop_forward_floating_never_high(days in 1i64..400) {
        let t0 = odt(2025, Month::January, 1, 0, 0, 0, 0);
        let t1 = t0 + Duration::days(days);
        let state = RepoState::from_ok_observation(&ok_obs(
            t0,
            vec![lw("refs/tags/v4", &commit_a(), &tree_a())],
        )).unwrap();
        let obs = ok_obs(t1, vec![lw("refs/tags/v4", &commit_b(), &tree_b())]);
        let enrich = enrichment_ahead(&commit_a(), &commit_b());
        let (_s, events) = classify(&state, &obs, &enrich).unwrap();
        for e in events {
            if let ClassifiedEvent::Move { severity, ancestry: Some(Ancestry::Ahead), .. } = e {
                prop_assert_ne!(severity, Severity::High);
            }
        }
    }
}
