//! Derive is pure: `(ChainTip, Vec<ClassifiedEvent>) -> Vec<UnhashedEntry>`.
//!
//! Written before the implementation. Cross-sweep Correlation, growing batches,
//! detecting-observation timestamps, and byte-for-byte replay are the contract.

use std::collections::BTreeMap;
use std::fs;

use sha2::{Digest, Sha256};
use refledger_log::canonical_json;
use refledger_log::chain::{verify, Chain, UnhashedEntry};
use refledger_log::entry::{Diff, Event};
use refledger_poller::classify::{
    classify, Ancestry, ClassifiedEvent, Enrichment, MoveKind, RefForm, RepoState, Severity,
    CORRELATION_NOTE,
};
use refledger_poller::derive::{
    derive, derive_observation_digest, ChainTip, DeriveError, ObservationDayStats,
};
use refledger_poller::enrich::CompareCache;
use refledger_poller::observation::{
    store_observation_at, ETag, Method, Observation, ObservedRef, Outcome,
};
use tempfile::TempDir;
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};
use ulid::Ulid;

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

fn lw(name: &str, commit: &str, tree: &str) -> ObservedRef {
    ObservedRef::new_lightweight(name, commit, tree).unwrap()
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

fn tip_at(next_seq: u64, at: OffsetDateTime) -> ChainTip {
    ChainTip {
        next_seq,
        repo: "acme/widgets".into(),
        recorded_at: at,
        move_seqs_by_ref: BTreeMap::new(),
        diffs: BTreeMap::new(),
    }
}

fn enrichment_ahead(old: &str, new: &str) -> Enrichment {
    Enrichment::empty().with_ancestry(old, new, Ancestry::Ahead)
}

fn append_all(chain: &mut Chain, entries: Vec<UnhashedEntry>) {
    for e in entries {
        chain.append(e).expect("append");
    }
}

fn tip_from_chain(chain: &Chain, repo: &str, recorded_at: OffsetDateTime) -> ChainTip {
    ChainTip::from_entries(chain.entries(), repo, recorded_at, BTreeMap::new())
}

#[test]
fn recorded_at_comes_from_detecting_observation_never_wall_clock() {
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let detect = odt(2026, Month::March, 15, 12, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![lw("refs/tags/v1.0.0", &commit_a(), &tree_a())],
    ))
    .unwrap();
    let obs = ok_obs(detect, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (_s, events) = classify(&state, &obs, &enrich).unwrap();
    let out = derive(&tip_at(1, detect), &events).expect("derive");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].recorded_at, detect);
    assert_eq!(out[0].event, Event::Move);
}

#[test]
fn exhaustive_match_covers_every_classified_event_variant() {
    // Construct one of each variant and ensure derive accepts them.
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 2, 0, 0, 0, 0);
    let state = RepoState::from_ok_observation(&ok_obs(
        t0,
        vec![
            lw("refs/tags/v1.0.0", &commit_a(), &tree_a()),
            lw("refs/tags/v2.0.0", &commit_a(), &tree_a()),
        ],
    ))
    .unwrap();

    // Deletion + move
    let obs = ok_obs(t1, vec![lw("refs/tags/v1.0.0", &commit_b(), &tree_b())]);
    let enrich = enrichment_ahead(&commit_a(), &commit_b());
    let (state, events) = classify(&state, &obs, &enrich).unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, ClassifiedEvent::Deletion { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, ClassifiedEvent::Move { .. })));
    derive(&tip_at(1, t1), &events).expect("deletion+move");

    // Recreation
    let obs2 = ok_obs(
        t1 + Duration::hours(2),
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v2.0.0", &commit_a(), &tree_a()),
        ],
    );
    let (_s, events2) = classify(&state, &obs2, &Enrichment::empty()).unwrap();
    assert!(events2
        .iter()
        .any(|e| matches!(e, ClassifiedEvent::Recreation { .. })));
    derive(&tip_at(10, t1 + Duration::hours(2)), &events2).expect("recreation");

    // RepoUnavailable / RepoRedirected
    let fail = Observation::builder()
        .repo("acme/gone")
        .unwrap()
        .observed_at(t1)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Failed {
            http_status: 404,
            error_class: refledger_poller::observation::ErrorClass::Upstream,
            backoff_applied: Duration::seconds(0),
        })
        .build()
        .unwrap();
    let (_s, ev) = classify(&RepoState::default(), &fail, &Enrichment::empty()).unwrap();
    assert!(matches!(ev[0], ClassifiedEvent::RepoUnavailable { .. }));
    let mut tip = tip_at(1, t1);
    tip.repo = "acme/gone".into();
    derive(&tip, &ev).expect("unavailable");

    let redir = Observation::builder()
        .repo("acme/old")
        .unwrap()
        .observed_at(t1)
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Failed {
            http_status: 301,
            error_class: refledger_poller::observation::ErrorClass::Upstream,
            backoff_applied: Duration::seconds(0),
        })
        .redirect_location("https://api.github.com/repositories/1")
        .build()
        .unwrap();
    let (_s, ev) = classify(&RepoState::default(), &redir, &Enrichment::empty()).unwrap();
    assert!(matches!(ev[0], ClassifiedEvent::RepoRedirected { .. }));
    tip.repo = "acme/old".into();
    derive(&tip, &ev).expect("redirected");
}

#[test]
fn trivy_shape_across_three_sweeps_emits_moves_then_correlation() {
    // Sweep 1: two Exact moves — no Correlation yet.
    // Sweep 2: third Exact move crosses threshold — Move + Correlation(3).
    // Sweep 3: fourth Exact move grows the batch — Move + second Correlation(4),
    //          same batch_id, never editing the first.
    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t2 = odt(2026, Month::January, 1, 12, 10, 0, 0);
    let t3 = odt(2026, Month::January, 1, 12, 20, 0, 0);

    let prior: Vec<_> = (0..4)
        .map(|i| lw(&format!("refs/tags/v1.0.{i}"), &commit_a(), &tree_a()))
        .collect();
    let state0 = RepoState::from_ok_observation(&ok_obs(t0, prior)).unwrap();
    let enrich = enrichment_ahead(&commit_a(), &commit_b());

    let mut chain = Chain::ephemeral("test-trivy");
    chain
        .append(UnhashedEntry::correction(
            t0,
            0,
            "genesis placeholder — log opened",
        ))
        .unwrap();

    // Sweep 1: tags 0 and 1 move
    let obs1 = ok_obs(
        t1,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_a(), &tree_a()),
            lw("refs/tags/v1.0.3", &commit_a(), &tree_a()),
        ],
    );
    let (state1, e1) = classify(&state0, &obs1, &enrich).unwrap();
    assert!(e1.iter().all(|e| !matches!(
        e,
        ClassifiedEvent::Move {
            correlation: Some(_),
            ..
        }
    )));
    let derived1 = derive(&tip_from_chain(&chain, "acme/widgets", t1), &e1).unwrap();
    assert_eq!(derived1.len(), 2);
    assert!(derived1.iter().all(|e| e.event == Event::Move));
    assert!(derived1.iter().all(|e| e.correlation.is_none()));
    append_all(&mut chain, derived1);

    // Sweep 2: tag 2 joins — threshold crossed
    let obs2 = ok_obs(
        t2,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.3", &commit_a(), &tree_a()),
        ],
    );
    let (state2, e2) = classify(&state1, &obs2, &enrich).unwrap();
    let derived2 = derive(&tip_from_chain(&chain, "acme/widgets", t2), &e2).unwrap();
    assert_eq!(derived2.len(), 2, "Move + Correlation: {derived2:?}");
    assert_eq!(derived2[0].event, Event::Move);
    assert_eq!(derived2[1].event, Event::Correlation);
    let corr1 = derived2[1].correlation.as_ref().unwrap();
    assert_eq!(corr1.member_seqs.len(), 3);
    assert_eq!(corr1.note.as_deref(), Some(CORRELATION_NOTE));
    let batch_id = corr1.batch_id.clone();
    // member_seqs must all be < the Correlation's eventual seq (= chain.len() + 1 after Move)
    let corr_seq = chain.len() as u64 + 1; // after appending the Move
    assert!(corr1.member_seqs.iter().all(|s| *s < corr_seq + 1));
    append_all(&mut chain, derived2);
    verify(chain.entries()).unwrap();

    // Sweep 3: tag 3 grows the batch — second Correlation, same batch_id
    let obs3 = ok_obs(
        t3,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.3", &commit_b(), &tree_b()),
        ],
    );
    let (_s, e3) = classify(&state2, &obs3, &enrich).unwrap();
    let derived3 = derive(&tip_from_chain(&chain, "acme/widgets", t3), &e3).unwrap();
    assert_eq!(derived3.len(), 2);
    assert_eq!(derived3[0].event, Event::Move);
    assert_eq!(derived3[1].event, Event::Correlation);
    let corr2 = derived3[1].correlation.as_ref().unwrap();
    assert_eq!(corr2.batch_id, batch_id, "growing batch keeps batch_id");
    assert_eq!(corr2.member_seqs.len(), 4);
    append_all(&mut chain, derived3);

    let correlations: Vec<_> = chain
        .entries()
        .iter()
        .filter(|e| e.event == Event::Correlation)
        .collect();
    assert_eq!(correlations.len(), 2, "never edit the first Correlation");
    assert_eq!(
        correlations[0]
            .correlation
            .as_ref()
            .unwrap()
            .member_seqs
            .len(),
        3
    );
    assert_eq!(
        correlations[1]
            .correlation
            .as_ref()
            .unwrap()
            .member_seqs
            .len(),
        4
    );
    verify(chain.entries()).unwrap();
}

#[test]
fn observation_digest_commits_to_day_files() {
    let dir = TempDir::new().unwrap();
    let day = dir.path().join("2026/09/28");
    fs::create_dir_all(&day).unwrap();
    let path = day.join("acme--widgets.jsonl");
    fs::write(&path, b"line\n").unwrap();
    let digest_hex = hex::encode(Sha256::digest(b"line\n"));

    let at = odt(2026, Month::September, 29, 0, 0, 0, 0);
    let entry = derive_observation_digest(
        at,
        ObservationDayStats {
            date: "2026-09-28".into(),
            repos_polled: 1,
            ok: 1,
            not_modified: 0,
            failed: 0,
            skipped: 0,
            files: vec![("2026/09/28/acme--widgets.jsonl".into(), digest_hex.clone())],
            note: None,
        },
    )
    .unwrap();
    assert_eq!(entry.event, Event::ObservationDigest);
    let d = entry.observation_digest.as_ref().unwrap();
    assert_eq!(d.files[0].sha256, digest_hex);
    assert_eq!(d.ok, 1);
}

#[test]
fn replay_observe_classify_derive_chain_is_byte_for_byte() {
    // Full archive → two independent replays → identical canonical JSONL bytes.
    // Enrich results come from a persisted compare cache (no network on replay).
    let root = TempDir::new().unwrap();
    let obs_dir = root.path().join("observations");
    let cache_path = root.path().join("compare.jsonl");
    fs::create_dir_all(&obs_dir).unwrap();

    let t0 = odt(2026, Month::January, 1, 0, 0, 0, 0);
    let t1 = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let t2 = odt(2026, Month::January, 1, 12, 15, 0, 0);

    let seed = ok_obs(
        t0,
        vec![
            lw("refs/tags/v1.0.0", &commit_a(), &tree_a()),
            lw("refs/tags/v1.0.1", &commit_a(), &tree_a()),
            lw("refs/tags/v1.0.2", &commit_a(), &tree_a()),
            lw("refs/tags/v4", &commit_a(), &tree_a()),
        ],
    );
    store_observation_at(&seed, &obs_dir).unwrap();

    let sweep1 = ok_obs(
        t1,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_a(), &tree_a()),
            lw("refs/tags/v4", &commit_b(), &tree_b()),
        ],
    );
    store_observation_at(&sweep1, &obs_dir).unwrap();

    let sweep2 = ok_obs(
        t2,
        vec![
            lw("refs/tags/v1.0.0", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.1", &commit_b(), &tree_b()),
            lw("refs/tags/v1.0.2", &commit_b(), &tree_b()),
            lw("refs/tags/v4", &commit_b(), &tree_b()),
        ],
    );
    store_observation_at(&sweep2, &obs_dir).unwrap();

    // Seed compare cache as if enrich had already run (replay needs no network).
    let mut cache = CompareCache::open(&cache_path).unwrap();
    cache
        .put(
            &commit_a(),
            &commit_b(),
            refledger_poller::enrich::CompareResult {
                ancestry: Ancestry::Ahead,
                files_added: 1,
                files_removed: 0,
                files_modified: 2,
                files_renamed: 0,
                paths: vec!["action.yml".into(), "src/main.rs".into()],
                diff_possibly_truncated: false,
            },
        )
        .unwrap();
    drop(cache);

    let run = || -> Vec<u8> {
        let cache = CompareCache::open(&cache_path).unwrap();
        let mut chain = Chain::ephemeral("replay");
        chain
            .append(UnhashedEntry::correction(
                t0,
                0,
                "genesis placeholder — log opened",
            ))
            .unwrap();

        let mut state = RepoState::default();
        let observations = load_obs_sorted(&obs_dir);
        for obs in &observations {
            let enrich = enrichment_from_cache(&state, obs, &cache);
            let (next, events) = classify(&state, obs, &enrich).unwrap();
            let mut tip = tip_from_chain(
                &chain,
                obs.repo().as_str(),
                obs.observed_at().as_offset_datetime(),
            );
            tip.diffs = diffs_from_cache(&cache);
            let derived = derive(&tip, &events).unwrap();
            append_all(&mut chain, derived);
            state = next;
        }
        verify(chain.entries()).unwrap();
        let mut bytes = Vec::new();
        for e in chain.entries() {
            let v = serde_json::to_value(e).unwrap();
            bytes.extend(canonical_json(&v).unwrap());
            bytes.push(b'\n');
        }
        bytes
    };

    let a = run();
    let b = run();
    assert_eq!(a, b, "replay must regenerate the log BYTE FOR BYTE");
    assert!(!a.is_empty());
}

fn load_obs_sorted(dir: &std::path::Path) -> Vec<Observation> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<Observation>) {
        for ent in fs::read_dir(dir).unwrap() {
            let path = ent.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                for line in fs::read_to_string(&path).unwrap().lines() {
                    if line.is_empty() {
                        continue;
                    }
                    out.push(serde_json::from_str(line).unwrap());
                }
            }
        }
    }
    walk(dir, &mut out);
    out.sort_by_key(|o: &Observation| o.observed_at().as_offset_datetime());
    out
}

fn enrichment_from_cache(state: &RepoState, obs: &Observation, cache: &CompareCache) -> Enrichment {
    let mut e = Enrichment::empty();
    let Outcome::Ok { refs, .. } = obs.outcome() else {
        return e;
    };
    for r in refs {
        let (Some(new_commit), Some(_)) = (r.commit_sha(), r.tree_sha()) else {
            continue;
        };
        let Some(prev) = state.binding(r.name()) else {
            continue;
        };
        if prev.commit_sha() == new_commit {
            continue;
        }
        if let Some(c) = cache.get(prev.commit_sha(), new_commit) {
            e = e.with_ancestry(prev.commit_sha(), new_commit, c.ancestry);
        }
    }
    e
}

fn diffs_from_cache(cache: &CompareCache) -> BTreeMap<(String, String), Diff> {
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

#[test]
fn ui_new_classified_event_variant_without_mapping_fails_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/derive_ui/unmapped_event_variant.rs");
}

#[test]
fn derive_rejects_empty_source_observations_on_move() {
    // Hand-built incomplete event should surface as DeriveError if we ever
    // receive one — classify always supplies ≥2, but derive re-checks.
    let at = odt(2026, Month::January, 1, 12, 0, 0, 0);
    let from = refledger_poller::classify::BindingSnapshot {
        target_sha: commit_a(),
        commit_sha: commit_a(),
        tree_sha: tree_a(),
        ref_type: refledger_poller::observation::RefType::Lightweight,
        first_observed: at - Duration::days(10),
        last_observed: at - Duration::hours(1),
        observation_count: 3,
        action_yml_sha: None,
    };
    let to = refledger_poller::classify::BindingSnapshot {
        target_sha: commit_b(),
        commit_sha: commit_b(),
        tree_sha: tree_b(),
        ref_type: refledger_poller::observation::RefType::Lightweight,
        first_observed: at,
        last_observed: at,
        observation_count: 1,
        action_yml_sha: None,
    };
    let events = vec![ClassifiedEvent::Move {
        ref_name: "refs/tags/v1.0.0".into(),
        form: RefForm::Exact,
        kind: MoveKind::ContentChange,
        severity: Severity::High,
        from,
        to,
        observation_window_seconds: 3600,
        ancestry: Some(Ancestry::Ahead),
        correlation: None,
        source_observations: vec![Ulid::nil()], // deliberately short
    }];
    let err = derive(&tip_at(1, at), &events).expect_err("need ≥2 sources");
    assert!(matches!(err, DeriveError::SourceObservations));
}
