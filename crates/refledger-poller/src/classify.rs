//! Tag-event classification.
//!
//! # Ref form first, stability second
//!
//! §6.2's original severity ranking put "content-changing move on a long-stable
//! tag" at the top. Under GitHub Actions convention that describes **every
//! legitimate floating-tag release**: `v4` sits still for months, then moves to
//! new code. Ranking by stability alone makes routine releases look like
//! attacks (trap #2 — alert fatigue).
//!
//! The discriminator that separates maintenance from compromise is **ref form**:
//! floating tags (`v4`, `v4.2`) are supposed to move; exact tags (`v4.2.2`)
//! never are. Classification therefore keys severity on [`RefForm`] first and
//! uses stability / ancestry as secondary signals.
//!
//! # Purity
//!
//! [`classify`] is a pure function of `(RepoState, Observation, Enrichment)`.
//! It never touches the network, the clock, or a random source. Ancestry and
//! diffs arrive precomputed in [`Enrichment`] (see [`crate::enrich`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use crate::observation::{
    ErrorClass, Observation, ObservedRef, Outcome, RefType, RepoSlug, Timestamp,
};
use ulid::Ulid;

/// Default window for batch-correlation across consecutive sweeps.
pub const DEFAULT_CORRELATION_WINDOW: Duration = Duration::minutes(30);

/// Minimum pre-existing Exact tags moving to one target that trigger correlation.
pub const CORRELATION_MIN_EXACT: usize = 3;

/// Fixed factual note on every batch-correlation payload. Never generated prose.
pub const CORRELATION_NOTE: &str = "At least 3 pre-existing exact-version tags moved to the same target within the correlation window.";

/// One sweep-local move candidate before correlation attachment.
type SweepMove = (
    String,
    RefForm,
    String,
    BindingSnapshot,
    BindingSnapshot,
    MoveKind,
    Option<Ancestry>,
);

/// How a tag name is meant to be used under Actions conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefForm {
    FloatingMajor,
    FloatingMinor,
    Exact,
    NamedChannel,
    Other,
}

impl RefForm {
    /// Parse a ref name (`v4`, `refs/tags/v4.2.2`, …).
    pub fn parse(name: &str) -> Self {
        let bare = name.strip_prefix("refs/tags/").unwrap_or(name);
        let lower = bare.to_ascii_lowercase();
        match lower.as_str() {
            "main" | "latest" | "stable" => return Self::NamedChannel,
            _ => {}
        }
        parse_semver_form(bare)
    }

    pub fn is_floating(self) -> bool {
        matches!(self, Self::FloatingMajor | Self::FloatingMinor)
    }
}

fn parse_semver_form(bare: &str) -> RefForm {
    let s = bare.strip_prefix('v').unwrap_or(bare);
    // Exact: MAJOR.MINOR.PATCH with optional pre-release / build.
    if let Some((core, _)) = s.split_once('+') {
        return parse_semver_core(core, true);
    }
    if let Some((core, pre)) = s.split_once('-') {
        if !pre.is_empty() && looks_like_triple(core) {
            return RefForm::Exact;
        }
    }
    parse_semver_core(s, false)
}

fn parse_semver_core(s: &str, force_exact_if_triple: bool) -> RefForm {
    let parts: Vec<&str> = s.split('.').collect();
    match parts.as_slice() {
        [maj] if is_u64(maj) => {
            if force_exact_if_triple {
                RefForm::Other
            } else {
                RefForm::FloatingMajor
            }
        }
        [maj, min] if is_u64(maj) && is_u64(min) => RefForm::FloatingMinor,
        [maj, min, pat] if is_u64(maj) && is_u64(min) && is_u64(pat) => RefForm::Exact,
        _ => {
            if force_exact_if_triple && looks_like_triple(s) {
                RefForm::Exact
            } else {
                RefForm::Other
            }
        }
    }
}

fn looks_like_triple(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    matches!(parts.as_slice(), [a, b, c] if is_u64(a) && is_u64(b) && is_u64(c))
}

fn is_u64(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Reject leading zeros so date-like `2024.01.15` is Other, not Exact.
    if s.len() > 1 && s.starts_with('0') {
        return false;
    }
    true
}

/// Compare ancestry of new vs old commit (`GET .../compare/{old}...{new}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ancestry {
    Ahead,
    Behind,
    Diverged,
    Identical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Medium,
    Low,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseLevelSub {
    LightweightToAnnotated,
    AnnotatedToLightweight,
    UnchangedRefType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveKind {
    ReleaseLevelOnly { sub: ReleaseLevelSub },
    CommitMetadataOnly,
    ContentChange,
}

/// One observed binding at a point in the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingSnapshot {
    pub target_sha: String,
    pub commit_sha: String,
    pub tree_sha: String,
    pub ref_type: RefType,
    pub first_observed: OffsetDateTime,
    pub last_observed: OffsetDateTime,
    pub observation_count: u64,
    pub action_yml_sha: Option<String>,
}

impl BindingSnapshot {
    pub fn first_observed(&self) -> OffsetDateTime {
        self.first_observed
    }

    pub fn last_observed(&self) -> OffsetDateTime {
        self.last_observed
    }

    pub fn commit_sha(&self) -> &str {
        &self.commit_sha
    }

    pub fn target_sha(&self) -> &str {
        &self.target_sha
    }

    pub fn tree_sha(&self) -> &str {
        &self.tree_sha
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCorrelation {
    pub batch_id: String,
    pub refs_moved_together: Vec<String>,
    pub all_to_same_target: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClassifiedEvent {
    Move {
        ref_name: String,
        form: RefForm,
        kind: MoveKind,
        severity: Severity,
        from: BindingSnapshot,
        to: BindingSnapshot,
        observation_window_seconds: u64,
        ancestry: Option<Ancestry>,
        correlation: Option<BatchCorrelation>,
        /// Prior Ok that held `from`, then the detecting Ok. Always length ≥ 2.
        source_observations: Vec<Ulid>,
    },
    Deletion {
        ref_name: String,
        form: RefForm,
        severity: Severity,
        from: BindingSnapshot,
        to: BindingSnapshot,
        observation_window_seconds: u64,
        source_observations: Vec<Ulid>,
    },
    Recreation {
        ref_name: String,
        form: RefForm,
        severity: Severity,
        from: BindingSnapshot,
        to: BindingSnapshot,
        gap: Duration,
        same_target: bool,
        observation_window_seconds: u64,
        source_observations: Vec<Ulid>,
    },
    RepoUnavailable {
        http_status: u16,
        error_class: ErrorClass,
    },
    RepoRedirected {
        http_status: u16,
        location: String,
    },
}

/// Precomputed per-move facts. Produced by [`crate::enrich`]; never fetched here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Enrichment {
    ancestry: BTreeMap<(String, String), Ancestry>,
}

impl Enrichment {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_ancestry(mut self, old_commit: &str, new_commit: &str, status: Ancestry) -> Self {
        self.ancestry
            .insert((old_commit.to_owned(), new_commit.to_owned()), status);
        self
    }

    pub fn ancestry(&self, old_commit: &str, new_commit: &str) -> Option<Ancestry> {
        self.ancestry
            .get(&(old_commit.to_owned(), new_commit.to_owned()))
            .copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveBinding {
    snap: BindingSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tombstone {
    deleted_at: OffsetDateTime,
    last: BindingSnapshot,
    /// Ok observation that last confirmed the binding before deletion.
    prior_observation_id: Ulid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferedMove {
    ref_name: String,
    form: RefForm,
    new_target: String,
    at: OffsetDateTime,
}

/// Per-repository classification state. [`classify`] returns a new value; it
/// never mutates the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    repo: Option<RepoSlug>,
    bindings: BTreeMap<String, LiveBinding>,
    tombstones: BTreeMap<String, Tombstone>,
    move_buffer: Vec<BufferedMove>,
    correlation_window: Duration,
    /// Most recent Ok observation id (for `source_observations`).
    last_ok_observation_id: Option<Ulid>,
}

impl Default for RepoState {
    fn default() -> Self {
        Self {
            repo: None,
            bindings: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            move_buffer: Vec::new(),
            correlation_window: DEFAULT_CORRELATION_WINDOW,
            last_ok_observation_id: None,
        }
    }
}

impl RepoState {
    pub fn with_correlation_window(mut self, window: Duration) -> Self {
        self.correlation_window = window;
        self
    }

    /// Seed state from a successful observation (first poll of a repo).
    pub fn from_ok_observation(obs: &Observation) -> Result<Self, ClassifyError> {
        let mut state = Self {
            repo: Some(obs.repo().clone()),
            last_ok_observation_id: Some(obs.observation_id()),
            ..Self::default()
        };
        let at = obs.observed_at().as_offset_datetime();
        if let Outcome::Ok { refs, .. } = obs.outcome() {
            for r in refs {
                if let Some(b) = binding_from_ref(r, at, at, 1)? {
                    state
                        .bindings
                        .insert(r.name().to_owned(), LiveBinding { snap: b });
                }
            }
        }
        Ok(state)
    }

    pub fn binding(&self, name: &str) -> Option<&BindingSnapshot> {
        self.bindings.get(name).map(|b| &b.snap)
    }

    pub fn tombstone(&self, name: &str) -> Option<&BindingSnapshot> {
        self.tombstones.get(name).map(|t| &t.last)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClassifyError {
    #[error("observation_window_seconds must be > 0 (a zero-width window is a bug)")]
    ZeroObservationWindow,
    #[error("ref missing commit/tree for classification: {0}")]
    IncompleteRef(String),
    #[error("observation: {0}")]
    Observation(String),
}

/// Pure classification step. Run it twice on the same inputs and the outputs
/// must match — that property is what makes the signed log replayable.
pub fn classify(
    state: &RepoState,
    observation: &Observation,
    enrichment: &Enrichment,
) -> Result<(RepoState, Vec<ClassifiedEvent>), ClassifyError> {
    let mut next = state.clone();
    next.repo = Some(observation.repo().clone());
    let at = observation.observed_at().as_offset_datetime();

    match observation.outcome() {
        Outcome::Ok { refs, .. } => {
            let out = classify_ok(
                &mut next,
                refs,
                at,
                observation.observation_id(),
                enrichment,
            )?;
            Ok(out)
        }
        Outcome::NotModified { .. } => {
            for b in next.bindings.values_mut() {
                b.snap.last_observed = at;
                b.snap.observation_count = b.snap.observation_count.saturating_add(1);
            }
            Ok((next, Vec::new()))
        }
        Outcome::Skipped { .. } => Ok((next, Vec::new())),
        Outcome::Failed {
            http_status,
            error_class,
            ..
        } => {
            if *http_status == 301 || *http_status == 302 {
                let location = observation.redirect_location().unwrap_or("").to_owned();
                return Ok((
                    next,
                    vec![ClassifiedEvent::RepoRedirected {
                        http_status: *http_status,
                        location,
                    }],
                ));
            }
            if *http_status == 404 {
                return Ok((
                    next,
                    vec![ClassifiedEvent::RepoUnavailable {
                        http_status: 404,
                        error_class: *error_class,
                    }],
                ));
            }
            Ok((next, Vec::new()))
        }
    }
}

fn classify_ok(
    state: &mut RepoState,
    refs: &[ObservedRef],
    at: OffsetDateTime,
    detecting_id: Ulid,
    enrichment: &Enrichment,
) -> Result<(RepoState, Vec<ClassifiedEvent>), ClassifyError> {
    let prior_id = state.last_ok_observation_id.unwrap_or(detecting_id);
    let sources = vec![prior_id, detecting_id];
    let mut events = Vec::new();
    let mut seen = BTreeMap::<String, &ObservedRef>::new();
    for r in refs {
        seen.insert(r.name().to_owned(), r);
    }

    // Deletions: present in state, absent in this Ok set.
    let deleted_names: Vec<String> = state
        .bindings
        .keys()
        .filter(|k| !seen.contains_key(k.as_str()))
        .cloned()
        .collect();
    for name in deleted_names {
        let live = state.bindings.remove(&name).expect("just checked");
        let window = window_secs(live.snap.last_observed, at)?;
        let tomb_to = BindingSnapshot {
            last_observed: at,
            first_observed: at,
            observation_count: 0,
            ..live.snap.clone()
        };
        events.push(ClassifiedEvent::Deletion {
            ref_name: name.clone(),
            form: RefForm::parse(&name),
            severity: Severity::Info,
            from: live.snap.clone(),
            to: tomb_to,
            observation_window_seconds: window,
            source_observations: sources.clone(),
        });
        state.tombstones.insert(
            name,
            Tombstone {
                deleted_at: at,
                last: live.snap,
                prior_observation_id: prior_id,
            },
        );
    }

    // Moves, recreations, creations.
    let mut sweep_moves: Vec<SweepMove> = Vec::new();

    for (name, r) in &seen {
        let Some(new_snap) = binding_from_ref(r, at, at, 1)? else {
            continue;
        };
        let form = RefForm::parse(name);

        if let Some(tomb) = state.tombstones.remove(name) {
            let same_target = tomb.last.target_sha == new_snap.target_sha;
            let gap = at - tomb.deleted_at;
            let window = window_secs(tomb.last.last_observed, at)?;
            let mut restored = new_snap.clone();
            restored.first_observed = tomb.last.first_observed;
            restored.observation_count = tomb.last.observation_count.saturating_add(1);
            restored.last_observed = at;
            let severity = if same_target {
                Severity::Info
            } else {
                Severity::Medium
            };
            events.push(ClassifiedEvent::Recreation {
                ref_name: name.clone(),
                form,
                severity,
                from: tomb.last.clone(),
                to: restored.clone(),
                gap,
                same_target,
                observation_window_seconds: window,
                source_observations: vec![tomb.prior_observation_id, detecting_id],
            });
            state
                .bindings
                .insert(name.clone(), LiveBinding { snap: restored });
            continue;
        }

        match state.bindings.get(name) {
            None => {
                state
                    .bindings
                    .insert(name.clone(), LiveBinding { snap: new_snap });
            }
            Some(live) if live.snap.target_sha == new_snap.target_sha => {
                let mut updated = live.snap.clone();
                updated.last_observed = at;
                updated.observation_count = updated.observation_count.saturating_add(1);
                updated.action_yml_sha = new_snap.action_yml_sha.clone();
                state
                    .bindings
                    .insert(name.clone(), LiveBinding { snap: updated });
            }
            Some(live) => {
                let from = live.snap.clone();
                let mut to = new_snap.clone();
                to.first_observed = at;
                to.last_observed = at;
                to.observation_count = 1;
                let kind = move_kind(&from, &to);
                let ancestry = enrichment.ancestry(&from.commit_sha, &to.commit_sha);
                sweep_moves.push((
                    name.clone(),
                    form,
                    to.target_sha.clone(),
                    from,
                    to,
                    kind,
                    ancestry,
                ));
            }
        }
    }

    let correlated_targets = find_correlations(state, &sweep_moves, at);

    for (name, form, target, from, to, kind, ancestry) in sweep_moves {
        let window = window_secs(from.last_observed, to.first_observed)?;
        let correlation = correlated_targets.get(&target).and_then(|c| {
            if form == RefForm::Exact && c.refs_moved_together.iter().any(|r| r == &name) {
                Some(c.clone())
            } else {
                None
            }
        });
        let severity = severity_for(form, &kind, ancestry, correlation.is_some());
        events.push(ClassifiedEvent::Move {
            ref_name: name.clone(),
            form,
            kind,
            severity,
            from: from.clone(),
            to: to.clone(),
            observation_window_seconds: window,
            ancestry,
            correlation,
            source_observations: sources.clone(),
        });
        state
            .bindings
            .insert(name.clone(), LiveBinding { snap: to });
        if form == RefForm::Exact {
            state.move_buffer.push(BufferedMove {
                ref_name: name,
                form,
                new_target: target,
                at,
            });
        }
    }

    prune_move_buffer(state, at);
    state.last_ok_observation_id = Some(detecting_id);
    events.sort_by_key(event_sort_key);
    Ok((state.clone(), events))
}

fn event_sort_key(e: &ClassifiedEvent) -> (u8, String) {
    match e {
        ClassifiedEvent::RepoUnavailable { .. } => (0, String::new()),
        ClassifiedEvent::RepoRedirected { .. } => (1, String::new()),
        ClassifiedEvent::Deletion { ref_name, .. } => (2, ref_name.clone()),
        ClassifiedEvent::Recreation { ref_name, .. } => (3, ref_name.clone()),
        ClassifiedEvent::Move { ref_name, .. } => (4, ref_name.clone()),
    }
}

fn find_correlations(
    state: &RepoState,
    sweep_moves: &[SweepMove],
    at: OffsetDateTime,
) -> BTreeMap<String, BatchCorrelation> {
    let mut by_target: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let window_start = at - state.correlation_window;
    for m in &state.move_buffer {
        if m.at >= window_start && m.form == RefForm::Exact {
            by_target
                .entry(m.new_target.clone())
                .or_default()
                .push(m.ref_name.clone());
        }
    }
    for (name, form, target, ..) in sweep_moves {
        if *form == RefForm::Exact {
            by_target
                .entry(target.clone())
                .or_default()
                .push(name.clone());
        }
    }

    let mut out = BTreeMap::new();
    for (target, mut refs) in by_target {
        refs.sort();
        refs.dedup();
        if refs.len() >= CORRELATION_MIN_EXACT {
            // batch_id must be stable as the batch grows across sweeps so a
            // second Correlation entry can reuse it (never edit the first).
            let mut earliest = at;
            for m in &state.move_buffer {
                if m.new_target == target && m.at >= window_start && m.at < earliest {
                    earliest = m.at;
                }
            }
            let prefix = &target[..8.min(target.len())];
            let batch_id = format!("corr-{prefix}-{}", earliest.unix_timestamp());
            out.insert(
                target,
                BatchCorrelation {
                    batch_id,
                    refs_moved_together: refs,
                    all_to_same_target: true,
                    note: Some(CORRELATION_NOTE.to_owned()),
                },
            );
        }
    }
    out
}

fn prune_move_buffer(state: &mut RepoState, at: OffsetDateTime) {
    let start = at - state.correlation_window;
    state.move_buffer.retain(|m| m.at >= start);
}

fn move_kind(from: &BindingSnapshot, to: &BindingSnapshot) -> MoveKind {
    if from.commit_sha == to.commit_sha && from.tree_sha == to.tree_sha {
        let sub = match (from.ref_type, to.ref_type) {
            (RefType::Lightweight, RefType::Annotated) => ReleaseLevelSub::LightweightToAnnotated,
            (RefType::Annotated, RefType::Lightweight) => ReleaseLevelSub::AnnotatedToLightweight,
            _ => ReleaseLevelSub::UnchangedRefType,
        };
        MoveKind::ReleaseLevelOnly { sub }
    } else if from.tree_sha == to.tree_sha {
        MoveKind::CommitMetadataOnly
    } else {
        MoveKind::ContentChange
    }
}

fn severity_for(
    form: RefForm,
    kind: &MoveKind,
    ancestry: Option<Ancestry>,
    correlated: bool,
) -> Severity {
    if correlated {
        return Severity::High;
    }
    match kind {
        MoveKind::ReleaseLevelOnly { .. } => Severity::Info,
        MoveKind::CommitMetadataOnly => match form {
            RefForm::Exact => Severity::Medium,
            RefForm::NamedChannel => Severity::Info,
            RefForm::FloatingMajor | RefForm::FloatingMinor => Severity::Low,
            RefForm::Other => Severity::Info,
        },
        MoveKind::ContentChange => match form {
            RefForm::Exact => Severity::High,
            RefForm::NamedChannel => Severity::Info,
            RefForm::FloatingMajor | RefForm::FloatingMinor => match ancestry {
                Some(Ancestry::Behind) | Some(Ancestry::Diverged) => Severity::High,
                Some(Ancestry::Ahead) => Severity::Low,
                Some(Ancestry::Identical) | None => Severity::Low,
            },
            RefForm::Other => Severity::Info,
        },
    }
}

fn window_secs(from: OffsetDateTime, to: OffsetDateTime) -> Result<u64, ClassifyError> {
    let delta = to - from;
    let secs = delta.whole_seconds();
    if secs <= 0 {
        return Err(ClassifyError::ZeroObservationWindow);
    }
    Ok(secs as u64)
}

fn binding_from_ref(
    r: &ObservedRef,
    first: OffsetDateTime,
    last: OffsetDateTime,
    count: u64,
) -> Result<Option<BindingSnapshot>, ClassifyError> {
    let (Some(commit), Some(tree)) = (r.commit_sha(), r.tree_sha()) else {
        // Non-commit peels / listing-only stubs: classification skips them.
        return Ok(None);
    };
    // Pre-fix listing invented these SHAs for unpeeled annotated tags. They
    // are not git objects; treating them as bindings produced mass false Moves
    // and fatal compare 404s (2026-10-01 outage).
    if crate::enrich::is_invented_placeholder(commit)
        || crate::enrich::is_invented_placeholder(tree)
    {
        return Ok(None);
    }
    Ok(Some(BindingSnapshot {
        target_sha: r.target_sha().to_owned(),
        commit_sha: commit.to_owned(),
        tree_sha: tree.to_owned(),
        ref_type: r.ref_type(),
        first_observed: first,
        last_observed: last,
        observation_count: count,
        action_yml_sha: r.action_yml_sha().map(|s| s.to_owned()),
    }))
}

// Timestamp kept available for future wire adapters.
#[allow(dead_code)]
fn _ts(t: OffsetDateTime) -> Result<Timestamp, ClassifyError> {
    Timestamp::from_offset_datetime(t).map_err(|e| ClassifyError::Observation(e.to_string()))
}
