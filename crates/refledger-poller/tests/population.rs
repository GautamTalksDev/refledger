//! Population: seed set + transitive closure, keyed on `(repo, path)`.
//!
//! Tests written first. The Trivy incident is the design constraint: consumers
//! never referenced `setup-trivy` directly — popularity ranking under-weights
//! exactly the dependency that mattered.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use refledger_log::entry::{Event, PopulationChangeKind, PopulationReason};
use refledger_poller::population::{
    derive_population_change, expand_closure, extract_external_uses, load_watched, save_watched,
    ActionRef, ClosureInput, SeedSource, WatchedEntry, WatchedKey, WatchedReason,
    CLOSURE_DEPTH_CAP,
};
use tempfile::TempDir;
use time::{Month, OffsetDateTime, PrimitiveDateTime, Time};

fn odt(year: i32, month: Month, day: u8, hour: u8, min: u8, sec: u8) -> OffsetDateTime {
    let time = Time::from_hms(hour, min, sec).unwrap();
    let date = time::Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn key(repo: &str, path: Option<&str>) -> WatchedKey {
    WatchedKey {
        repo: repo.into(),
        path: path.map(|p| p.into()),
    }
}

#[test]
fn extract_uses_tag_and_sha_are_external_local_and_docker_ignored() {
    let yml = r#"
name: composite
runs:
  using: composite
  steps:
    - uses: actions/checkout@v4
    - uses: owner/dep@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    - uses: ./local-action
    - uses: docker://alpine:3.19
    - run: echo hi
"#;
    let refs = extract_external_uses(yml);
    assert_eq!(refs.len(), 2);
    assert_eq!(
        refs[0],
        ActionRef {
            repo: "actions/checkout".into(),
            path: None,
            rev: "v4".into(),
        }
    );
    assert_eq!(
        refs[1],
        ActionRef {
            repo: "owner/dep".into(),
            path: None,
            rev: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        }
    );
}

#[test]
fn extract_uses_subdirectory_action() {
    let yml = r#"
runs:
  using: composite
  steps:
    - uses: aquasecurity/trivy-action/setup@v0.20.0
"#;
    let refs = extract_external_uses(yml);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].repo, "aquasecurity/trivy-action");
    assert_eq!(refs[0].path.as_deref(), Some("setup"));
    assert_eq!(refs[0].rev, "v0.20.0");
}

#[test]
fn closure_adds_tag_and_sha_refs_ignores_local_terminates_cycles() {
    let seed = key("root/action", None);
    let mut ymls: BTreeMap<(WatchedKey, String), String> = BTreeMap::new();
    // root@c0 uses mid@v1 and mid@deadbeef (SHA still adds the repo)
    ymls.insert(
        (seed.clone(), "c0".into()),
        r#"
runs:
  using: composite
  steps:
    - uses: mid/action@v1
    - uses: mid/action@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
    - uses: ./local
"#
        .into(),
    );
    // mid@v1 uses root@c0 (cycle) and leaf@v1
    ymls.insert(
        (key("mid/action", None), "v1".into()),
        r#"
runs:
  using: composite
  steps:
    - uses: root/action@c0
    - uses: leaf/action@v1
"#
        .into(),
    );
    // leaf has no further deps
    ymls.insert(
        (key("leaf/action", None), "v1".into()),
        "runs:\n  using: composite\n  steps: []\n".into(),
    );

    let result = expand_closure(ClosureInput {
        seeds: vec![seed.clone()],
        lookup: |k: &WatchedKey, rev: &str| ymls.get(&(k.clone(), rev.to_owned())).cloned(),
        depth_cap: CLOSURE_DEPTH_CAP,
    })
    .unwrap();

    let repos: BTreeSet<_> = result.added.iter().map(|e| e.key.repo.as_str()).collect();
    assert!(repos.contains("mid/action"));
    assert!(repos.contains("leaf/action"));
    assert!(!repos.contains("root/action"), "seed is not re-added");
    // Cycle must not loop forever — depth/visited terminates.
    assert!(result.added.len() <= 10);
}

#[test]
fn closure_depth_cap_records_cutoff() {
    assert_eq!(CLOSURE_DEPTH_CAP, 4);
    let mut ymls: BTreeMap<(WatchedKey, String), String> = BTreeMap::new();
    // Chain: d0 -> d1 -> d2 -> d3 -> d4 -> d5 (depth 5 exceeds cap 4)
    for i in 0..6 {
        let k = key(&format!("chain/d{i}"), None);
        let next = format!("chain/d{}@v1", i + 1);
        let body = if i < 5 {
            format!("runs:\n  using: composite\n  steps:\n    - uses: {next}\n")
        } else {
            "runs:\n  using: composite\n  steps: []\n".into()
        };
        ymls.insert((k, "v1".into()), body);
    }
    let seed = key("chain/d0", None);
    let result = expand_closure(ClosureInput {
        seeds: vec![seed],
        lookup: |k: &WatchedKey, rev: &str| ymls.get(&(k.clone(), rev.to_owned())).cloned(),
        depth_cap: CLOSURE_DEPTH_CAP,
    })
    .unwrap();
    assert!(
        !result.cutoffs.is_empty(),
        "depth > {CLOSURE_DEPTH_CAP} must be recorded, not silently dropped"
    );
    let cutoff_repos: BTreeSet<_> = result.cutoffs.iter().map(|c| c.key.repo.as_str()).collect();
    assert!(
        cutoff_repos.contains("chain/d5") || cutoff_repos.iter().any(|r| r.starts_with("chain/d")),
        "got cutoffs {:?}",
        result.cutoffs
    );
}

#[test]
fn subdirectory_actions_are_distinct_keys_sharing_one_poll_group() {
    let a = WatchedEntry {
        key: key("owner/repo", Some("a")),
        added_at: odt(2026, Month::September, 28, 0, 0, 0),
        reason: WatchedReason::Seed {
            source: SeedSource::OfficialOrg {
                org: "owner".into(),
            },
        },
        note: None,
        active: true,
    };
    let b = WatchedEntry {
        key: key("owner/repo", Some("b")),
        added_at: odt(2026, Month::September, 28, 0, 0, 0),
        reason: WatchedReason::Seed {
            source: SeedSource::OfficialOrg {
                org: "owner".into(),
            },
        },
        note: None,
        active: true,
    };
    assert_ne!(a.key, b.key);

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watched.jsonl");
    save_watched(&path, &[a.clone(), b.clone()]).unwrap();
    let loaded = load_watched(&path).unwrap();
    assert_eq!(loaded.len(), 2);

    let groups = refledger_poller::population::poll_groups(&loaded);
    assert_eq!(groups.len(), 1, "same repo must share one ref-listing poll");
    assert_eq!(groups[0].repo, "owner/repo");
    assert_eq!(groups[0].paths.len(), 2);
}

#[test]
fn population_change_emits_log_entry_for_add_and_remove() {
    let at = odt(2026, Month::September, 28, 12, 0, 0);
    let add = derive_population_change(
        at,
        "aquasecurity/setup-trivy",
        PopulationChangeKind::Added,
        PopulationReason::Transitive {
            via: "aquasecurity/trivy-action@cccccccccccccccccccccccccccccccccccccccc".into(),
        },
        None,
        None,
    );
    assert_eq!(add.event, Event::PopulationChange);
    assert_eq!(add.repo.as_deref(), Some("aquasecurity/setup-trivy"));

    let rem = derive_population_change(
        at,
        "acme/widgets",
        PopulationChangeKind::Removed,
        PopulationReason::Manual,
        Some("nested"),
        Some("maintainer request"),
    );
    assert_eq!(
        rem.population_change.as_ref().unwrap().change,
        PopulationChangeKind::Removed
    );
    assert_eq!(
        rem.population_change.as_ref().unwrap().path.as_deref(),
        Some("nested")
    );
}

#[test]
fn watched_jsonl_round_trips_seed_source() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watched.jsonl");
    let entries = vec![
        WatchedEntry {
            key: key("actions/checkout", None),
            added_at: odt(2026, Month::September, 28, 0, 0, 0),
            reason: WatchedReason::Seed {
                source: SeedSource::OfficialOrg {
                    org: "actions".into(),
                },
            },
            note: None,
            active: true,
        },
        WatchedEntry {
            key: key("aquasecurity/setup-trivy", None),
            added_at: odt(2026, Month::September, 28, 0, 1, 0),
            reason: WatchedReason::Transitive {
                via_repo: "aquasecurity/trivy-action".into(),
                via_path: None,
                via_commit: "dddddddddddddddddddddddddddddddddddddddd".into(),
            },
            note: None,
            active: true,
        },
    ];
    save_watched(&path, &entries).unwrap();
    let back = load_watched(&path).unwrap();
    assert_eq!(back, entries);
}

#[test]
fn fixture_population_file_is_loadable() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../population/watched.jsonl");
    assert!(root.is_file(), "missing {}", root.display());
    let entries = load_watched(&root).unwrap();
    assert!(!entries.is_empty(), "seed population must not be empty");
    for e in &entries {
        if let WatchedReason::Seed { source } = &e.reason {
            match source {
                SeedSource::OfficialOrg { org } => assert!(!org.is_empty()),
                SeedSource::IncidentReport { name } => assert!(!name.is_empty()),
                SeedSource::AcmRepPaper => {}
                SeedSource::Marketplace => {}
            }
        }
    }
}

#[test]
fn fair_skip_offset_is_stable_and_covers_all_slots() {
    use refledger_poller::population::{fair_skip_offset, rotate_groups};
    let n = 7usize;
    let mut offsets = std::collections::BTreeSet::new();
    for i in 0..n {
        let t = odt(2026, Month::January, 1, 0, 2, 0) + time::Duration::minutes((i * 5) as i64);
        offsets.insert(fair_skip_offset(t, n));
    }
    assert_eq!(offsets.len(), n);
    let groups: Vec<_> = (0..n).collect();
    let rotated = rotate_groups(&groups, 3);
    assert_eq!(rotated.first(), Some(&3));
    assert_eq!(rotated.last(), Some(&2));
}

#[test]
fn genesis_added_skips_keys_already_in_chain() {
    use refledger_poller::population::{
        genesis_added_entries, EarliestObservation, WatchedEntry, WatchedKey, WatchedReason,
    };
    use std::collections::{BTreeMap, BTreeSet};
    let key = WatchedKey::new("acme/widgets", None);
    let watched = vec![WatchedEntry {
        key: key.clone(),
        added_at: odt(2026, Month::January, 1, 0, 0, 0),
        reason: WatchedReason::Manual,
        active: true,
        note: None,
    }];
    let mut earliest = BTreeMap::new();
    earliest.insert(
        key.clone(),
        EarliestObservation {
            observed_at: odt(2026, Month::January, 1, 12, 0, 0),
            observation_id: "01TEST".into(),
        },
    );
    let mut already = BTreeSet::new();
    already.insert(key);
    assert!(genesis_added_entries(&watched, &earliest, &already).is_empty());
}
