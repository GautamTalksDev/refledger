//! Watched population: seed set + transitive closure of composite `uses:`.
//!
//! Keys are `(repo, path)` — subdirectory actions are distinct entries that
//! share one ref-listing poll per repo (see [`poll_groups`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use refledger_log::chain::UnhashedEntry;
use refledger_log::entry::{Event, PopulationChange, PopulationChangeKind, PopulationReason};

/// Maximum composite-closure depth. Anything beyond is recorded in
/// [`ClosureCutoff`], never silently dropped. Documented in
/// `population/METHOD.md`.
pub const CLOSURE_DEPTH_CAP: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PopulationError {
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
    #[error("invalid watched entry: {0}")]
    Invalid(String),
}

/// Stable identity of a watched action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WatchedKey {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl WatchedKey {
    pub fn new(repo: impl Into<String>, path: Option<String>) -> Self {
        Self {
            repo: repo.into(),
            path,
        }
    }
}

/// How a seed entered the population (recorded in `watched.jsonl`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SeedSource {
    OfficialOrg { org: String },
    Marketplace,
    AcmRepPaper,
    IncidentReport { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchedReason {
    Seed {
        source: SeedSource,
    },
    Transitive {
        via_repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via_path: Option<String>,
        via_commit: String,
    },
    Manual,
    Restored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedEntry {
    #[serde(flatten)]
    pub key: WatchedKey,
    #[serde(with = "time::serde::rfc3339")]
    pub added_at: OffsetDateTime,
    pub reason: WatchedReason,
    pub active: bool,
    /// Free-form annotation (e.g. `"canary"`). Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One external `uses:` reference extracted from an action.yml.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionRef {
    pub repo: String,
    pub path: Option<String>,
    pub rev: String,
}

impl ActionRef {
    pub fn key(&self) -> WatchedKey {
        WatchedKey::new(self.repo.clone(), self.path.clone())
    }
}

/// Parse composite `uses:` values. Ignores `./local` and `docker://`.
pub fn extract_external_uses(yml: &str) -> Vec<ActionRef> {
    let mut out = Vec::new();
    for line in yml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = strip_uses_key(trimmed) else {
            continue;
        };
        let val = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if val.is_empty() {
            continue;
        }
        if val.starts_with("./") || val == "." || val.starts_with("docker://") {
            continue;
        }
        if let Some(r) = parse_action_ref(val) {
            out.push(r);
        }
    }
    out
}

fn strip_uses_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    // Allow YAML list form: `- uses: owner/repo@ref`
    let after_list = trimmed
        .strip_prefix('-')
        .map(|s| s.trim_start())
        .unwrap_or(trimmed);
    let lower = after_list.to_ascii_lowercase();
    let rest = lower.strip_prefix("uses:")?;
    let offset = after_list.len() - rest.len();
    Some(&after_list[offset..])
}

fn parse_action_ref(val: &str) -> Option<ActionRef> {
    let (left, rev) = val.rsplit_once('@')?;
    if rev.is_empty() || left.is_empty() {
        return None;
    }
    let parts: Vec<&str> = left.split('/').collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = format!("{}/{}", parts[0], parts[1]);
    let path = if parts.len() > 2 {
        Some(parts[2..].join("/"))
    } else {
        None
    };
    Some(ActionRef {
        repo,
        path,
        rev: rev.to_owned(),
    })
}

/// Repo that shares one matching-refs poll among one or more `(repo, path)` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollGroup {
    pub repo: String,
    pub paths: Vec<Option<String>>,
}

/// Group active watched entries by repo so subdirectory actions share one poll.
pub fn poll_groups(entries: &[WatchedEntry]) -> Vec<PollGroup> {
    let mut by_repo: BTreeMap<String, BTreeSet<Option<String>>> = BTreeMap::new();
    for e in entries {
        if !e.active {
            continue;
        }
        by_repo
            .entry(e.key.repo.clone())
            .or_default()
            .insert(e.key.path.clone());
    }
    by_repo
        .into_iter()
        .map(|(repo, paths)| PollGroup {
            repo,
            paths: paths.into_iter().collect(),
        })
        .collect()
}

pub fn load_watched(path: &Path) -> Result<Vec<WatchedEntry>, PopulationError> {
    let file = File::open(path).map_err(|e| PopulationError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| PopulationError::Io(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let e: WatchedEntry = serde_json::from_str(&line)
            .map_err(|e| PopulationError::Serde(format!("line {}: {e}", i + 1)))?;
        if e.key.repo.is_empty() || !e.key.repo.contains('/') {
            return Err(PopulationError::Invalid(format!(
                "line {}: bad repo",
                i + 1
            )));
        }
        out.push(e);
    }
    Ok(out)
}

pub fn save_watched(path: &Path, entries: &[WatchedEntry]) -> Result<(), PopulationError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| PopulationError::Io(e.to_string()))?;
        }
    }
    let mut file = File::create(path).map_err(|e| PopulationError::Io(e.to_string()))?;
    for e in entries {
        let line = serde_json::to_string(e).map_err(|e| PopulationError::Serde(e.to_string()))?;
        writeln!(file, "{line}").map_err(|e| PopulationError::Io(e.to_string()))?;
    }
    file.sync_all()
        .map_err(|e| PopulationError::Io(e.to_string()))?;
    Ok(())
}

/// Bundle for [`expand_closure`].
pub struct ClosureInput<F>
where
    F: Fn(&WatchedKey, &str) -> Option<String>,
{
    pub seeds: Vec<WatchedKey>,
    pub lookup: F,
    pub depth_cap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureCutoff {
    pub key: WatchedKey,
    pub via: String,
    pub depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureAdded {
    pub key: WatchedKey,
    pub via_repo: String,
    pub via_path: Option<String>,
    pub via_commit: String,
    pub depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureResult {
    pub added: Vec<ClosureAdded>,
    pub cutoffs: Vec<ClosureCutoff>,
}

/// Expand the transitive composite-action closure from seeds.
///
/// Pure given `lookup`. Cycles terminate via a visited set. Depth beyond
/// `depth_cap` is recorded in [`ClosureResult::cutoffs`].
pub fn expand_closure<F>(input: ClosureInput<F>) -> Result<ClosureResult, PopulationError>
where
    F: Fn(&WatchedKey, &str) -> Option<String>,
{
    let mut scratch = ClosureScratch {
        watched: input.seeds.iter().cloned().collect(),
        added: Vec::new(),
        cutoffs: Vec::new(),
        queue: VecDeque::new(),
    };

    for s in &input.seeds {
        for rev in ["c0", "v1", "HEAD"] {
            if let Some(yml) = (input.lookup)(s, rev) {
                enqueue_deps(s, rev, &yml, 1, input.depth_cap, &mut scratch);
                break;
            }
        }
    }

    while let Some((parent, parent_rev, depth)) = scratch.queue.pop_front() {
        let Some(yml) = (input.lookup)(&parent, &parent_rev) else {
            continue;
        };
        enqueue_deps(
            &parent,
            &parent_rev,
            &yml,
            depth,
            input.depth_cap,
            &mut scratch,
        );
    }

    Ok(ClosureResult {
        added: scratch.added,
        cutoffs: scratch.cutoffs,
    })
}

struct ClosureScratch {
    watched: BTreeSet<WatchedKey>,
    added: Vec<ClosureAdded>,
    cutoffs: Vec<ClosureCutoff>,
    queue: VecDeque<(WatchedKey, String, u32)>,
}

fn enqueue_deps(
    parent: &WatchedKey,
    parent_rev: &str,
    yml: &str,
    child_depth: u32,
    depth_cap: u32,
    scratch: &mut ClosureScratch,
) {
    for r in extract_external_uses(yml) {
        let child = r.key();
        let via = match &parent.path {
            Some(p) => format!("{}@{p}@{parent_rev}", parent.repo),
            None => format!("{}@{parent_rev}", parent.repo),
        };
        if child_depth > depth_cap {
            scratch.cutoffs.push(ClosureCutoff {
                key: child,
                via,
                depth: child_depth,
            });
            continue;
        }
        if !scratch.watched.insert(child.clone()) {
            continue;
        }
        scratch.added.push(ClosureAdded {
            key: child.clone(),
            via_repo: parent.repo.clone(),
            via_path: parent.path.clone(),
            via_commit: parent_rev.to_owned(),
            depth: child_depth,
        });
        scratch.queue.push_back((child, r.rev, child_depth + 1));
    }
}

/// Build an unhashed PopulationChange log entry.
pub fn derive_population_change(
    recorded_at: OffsetDateTime,
    repo: &str,
    change: PopulationChangeKind,
    reason: PopulationReason,
    path: Option<&str>,
    note: Option<&str>,
) -> UnhashedEntry {
    derive_population_change_with_sources(recorded_at, repo, change, reason, path, note, None)
}

/// Like [`derive_population_change`] but attaches `source_observations`.
pub fn derive_population_change_with_sources(
    recorded_at: OffsetDateTime,
    repo: &str,
    change: PopulationChangeKind,
    reason: PopulationReason,
    path: Option<&str>,
    note: Option<&str>,
    source_observations: Option<Vec<String>>,
) -> UnhashedEntry {
    let mut entry = UnhashedEntry::empty(recorded_at, Event::PopulationChange);
    entry.repo = Some(repo.to_owned());
    entry.population_change = Some(PopulationChange {
        path: path.map(|p| p.to_owned()),
        change,
        reason,
        note: note.map(|n| n.to_owned()),
    });
    entry.source_observations = source_observations;
    entry
}

/// Earliest observation of a watched key, used for genesis Added rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarliestObservation {
    pub observed_at: OffsetDateTime,
    pub observation_id: String,
}

/// Resolve the earliest observation for a watched key.
///
/// Exact `(repo, path)` match wins. Otherwise fall back to the earliest
/// observation of the same repo (any path) — subdirectory keys share one
/// poll group's observation until they have a dedicated path poll.
pub fn earliest_for_key<'a>(
    earliest: &'a BTreeMap<WatchedKey, EarliestObservation>,
    key: &WatchedKey,
) -> Option<&'a EarliestObservation> {
    if let Some(obs) = earliest.get(key) {
        return Some(obs);
    }
    earliest
        .iter()
        .filter(|(k, _)| k.repo == key.repo)
        .map(|(_, obs)| obs)
        .min_by_key(|obs| (obs.observed_at, obs.observation_id.as_str()))
}

/// Commit that fixed per-group genesis skipping subdirectory keys.
/// Filled when this change lands on `main`.
pub const PER_KEY_ADDED_FIX_COMMIT: &str = "PENDING_PER_KEY_ADDED";

/// Deterministic Added entries for every watched key that still lacks one.
///
/// - True genesis (`already_added` empty): `recorded_at` = earliest observation.
/// - Late registration (chain already has some Added rows): `recorded_at` =
///   `run_now` (monotonic) with a `note` explaining the miss.
///
/// Ordered by `(recorded_at, repo, path)`. Same inputs → identical entries.
pub fn genesis_added_entries(
    watched: &[WatchedEntry],
    earliest: &BTreeMap<WatchedKey, EarliestObservation>,
    already_added: &BTreeSet<WatchedKey>,
    run_now: OffsetDateTime,
) -> Vec<UnhashedEntry> {
    let late = !already_added.is_empty();
    let mut rows: Vec<(
        OffsetDateTime,
        &WatchedEntry,
        &EarliestObservation,
        Option<String>,
    )> = Vec::new();
    for e in watched.iter().filter(|e| e.active) {
        if already_added.contains(&e.key) {
            continue;
        }
        let Some(obs) = earliest_for_key(earliest, &e.key) else {
            continue;
        };
        let (recorded_at, note) = if late {
            let note = format!(
                "late registration; first observed {}, observation {}, missing due to per-group derivation bug fixed in {}",
                format_obs_ts(obs.observed_at),
                obs.observation_id,
                PER_KEY_ADDED_FIX_COMMIT
            );
            (run_now, Some(note))
        } else {
            (obs.observed_at, e.note.clone())
        };
        rows.push((recorded_at, e, obs, note));
    }
    rows.sort_by(|(at_a, a, _, _), (at_b, b, _, _)| {
        (
            *at_a,
            a.key.repo.as_str(),
            a.key.path.as_deref().unwrap_or(""),
        )
            .cmp(&(
                *at_b,
                b.key.repo.as_str(),
                b.key.path.as_deref().unwrap_or(""),
            ))
    });
    rows.into_iter()
        .map(|(recorded_at, e, obs, note)| {
            derive_population_change_with_sources(
                recorded_at,
                &e.key.repo,
                PopulationChangeKind::Added,
                log_reason(&e.reason),
                e.key.path.as_deref(),
                note.as_deref(),
                Some(vec![obs.observation_id.clone()]),
            )
        })
        .collect()
}

fn format_obs_ts(t: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 1_000_000
    )
}

/// Convert a [`WatchedReason`] into the log's [`PopulationReason`].
pub fn log_reason(r: &WatchedReason) -> PopulationReason {
    match r {
        WatchedReason::Seed { .. } => PopulationReason::Seed,
        WatchedReason::Transitive {
            via_repo,
            via_path,
            via_commit,
        } => {
            let via = match via_path {
                Some(p) => format!("{via_repo}@{p}@{via_commit}"),
                None => format!("{via_repo}@{via_commit}"),
            };
            PopulationReason::Transitive { via }
        }
        WatchedReason::Manual => PopulationReason::Manual,
        WatchedReason::Restored => PopulationReason::Restored,
    }
}

/// Stable start index for fair budget skipping across runs.
///
/// Derived from the run's scheduled UTC slot so consecutive cron ticks
/// (`:02`, `:07`, …) rotate which poll groups are dropped when the request
/// budget is exhausted.
pub fn fair_skip_offset(scheduled: OffsetDateTime, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    // Five-minute cron slots: floor minutes-of-day / 5, plus day-of-year so
    // the rotation keeps moving across midnight.
    let mins = usize::from(scheduled.hour()) * 60 + usize::from(scheduled.minute());
    let day = scheduled.date().ordinal() as usize;
    ((day * 288) + (mins / 5)) % n
}

/// Rotate `groups` so index `offset` is first; order otherwise preserved.
pub fn rotate_groups<T: Clone>(groups: &[T], offset: usize) -> Vec<T> {
    if groups.is_empty() {
        return Vec::new();
    }
    let start = offset % groups.len();
    let mut out = Vec::with_capacity(groups.len());
    out.extend_from_slice(&groups[start..]);
    out.extend_from_slice(&groups[..start]);
    out
}
