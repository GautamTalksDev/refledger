//! Pure derivation: classified events → unhashed log entries.
//!
//! # Contract
//!
//! [`derive`] is a pure function of [`ChainTip`] and `&[ClassifiedEvent]`.
//! It never reads the clock, the network, or a random source.
//! `recorded_at` on every emitted entry is the detecting observation's
//! `observed_at` carried in the tip — never wall-clock time.
//!
//! Correlation across sweeps is expressed as a **separate** [`Event::Correlation`]
//! entry. Earlier Move entries are never edited. When a batch grows after its
//! first Correlation, a second Correlation is appended with the same `batch_id`
//! and the enlarged `member_seqs`.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use time::OffsetDateTime;
use ulid::Ulid;

use refledger_log::chain::UnhashedEntry;
use refledger_log::entry::{
    Ancestry as LogAncestry, Binding, Classification, Correlation, Diff, Entry, Event,
    ObservationDigest, ObservationFileDigest, RefForm as LogRefForm, RefType as LogRefType,
    Severity as LogSeverity,
};

use crate::classify::{Ancestry, BindingSnapshot, ClassifiedEvent, MoveKind, RefForm, Severity};
use crate::observation::RefType as ObsRefType;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeriveError {
    #[error("source_observations must have length ≥ 2")]
    SourceObservations,
    #[error("binding: {0}")]
    Binding(String),
    #[error("correlation member ref {0} has no Move seq on the tip")]
    MissingMemberSeq(String),
    #[error("observation digest: {0}")]
    Digest(String),
}

/// Chain state visible to derive. Built from prior entries + the detecting
/// observation's timestamp and (optional) compare-cache diffs.
#[derive(Debug, Clone)]
pub struct ChainTip {
    pub next_seq: u64,
    pub repo: String,
    /// Detecting observation's `observed_at`. Never the wall clock.
    pub recorded_at: OffsetDateTime,
    /// Most recent Move/Deletion/Recreation seq per ref for this repo.
    pub move_seqs_by_ref: BTreeMap<String, u64>,
    /// Compare results keyed by `(old_commit, new_commit)`.
    pub diffs: BTreeMap<(String, String), Diff>,
}

impl ChainTip {
    pub fn from_entries(
        entries: &[Entry],
        repo: &str,
        recorded_at: OffsetDateTime,
        diffs: BTreeMap<(String, String), Diff>,
    ) -> Self {
        let mut move_seqs_by_ref = BTreeMap::new();
        for e in entries {
            if e.repo.as_deref() != Some(repo) {
                continue;
            }
            match e.event {
                Event::Move | Event::Deletion | Event::Recreation => {
                    if let Some(r) = &e.r#ref {
                        move_seqs_by_ref.insert(r.clone(), e.seq);
                    }
                }
                _ => {}
            }
        }
        Self {
            next_seq: entries.len() as u64,
            repo: repo.to_owned(),
            recorded_at,
            move_seqs_by_ref,
            diffs,
        }
    }
}

/// Daily coverage / stability commitment under the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationDayStats {
    pub date: String,
    pub repos_polled: u64,
    pub ok: u64,
    pub not_modified: u64,
    pub failed: u64,
    pub skipped: u64,
    /// `(path, sha256 hex)` — sorted by path inside [`derive_observation_digest`].
    pub files: Vec<(String, String)>,
    /// Torn-write recovery note. Omitted from the entry when `None`.
    pub note: Option<String>,
}

/// Map classified events to unhashed log entries. Exhaustive over
/// [`ClassifiedEvent`] — a new variant without an arm fails to compile.
pub fn derive(
    tip: &ChainTip,
    events: &[ClassifiedEvent],
) -> Result<Vec<UnhashedEntry>, DeriveError> {
    let mut out = Vec::new();
    let mut next_seq = tip.next_seq;
    let mut seqs = tip.move_seqs_by_ref.clone();
    // batch_id → (payload, set of member refs we have seqs for)
    let mut pending_corr: BTreeMap<String, crate::classify::BatchCorrelation> = BTreeMap::new();

    for event in events {
        match event {
            ClassifiedEvent::Move {
                ref_name,
                form,
                kind,
                severity,
                from,
                to,
                observation_window_seconds,
                ancestry,
                correlation,
                source_observations,
            } => {
                ensure_sources(source_observations)?;
                let diff = tip
                    .diffs
                    .get(&(from.commit_sha.clone(), to.commit_sha.clone()))
                    .cloned();
                let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::Move);
                entry.classification = Some(map_kind(kind));
                entry.severity = Some(map_severity(*severity));
                entry.ref_form = Some(map_ref_form(*form));
                entry.ancestry = ancestry.map(map_ancestry);
                entry.repo = Some(tip.repo.clone());
                entry.r#ref = Some(ref_name.clone());
                entry.ref_type_before = Some(map_ref_type(from.ref_type));
                entry.ref_type_after = Some(map_ref_type(to.ref_type));
                entry.from = Some(binding_from(from)?);
                entry.to = Some(binding_to(to)?);
                entry.diff = diff;
                entry.source_observations = Some(ulids_to_strings(source_observations));
                entry.observation_window_seconds = Some(*observation_window_seconds);
                seqs.insert(ref_name.clone(), next_seq);
                next_seq += 1;
                out.push(entry);

                if let Some(c) = correlation {
                    pending_corr.insert(c.batch_id.clone(), c.clone());
                }
            }
            ClassifiedEvent::Deletion {
                ref_name,
                form,
                severity,
                from,
                to,
                observation_window_seconds,
                source_observations,
            } => {
                ensure_sources(source_observations)?;
                let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::Deletion);
                // LOG-FORMAT v1 requires classification on deletions; there is no
                // deletion-specific value. ContentChange is the published-vector
                // convention (see tests/vectors). It does not mean a tree diff
                // was computed — see Correction on seq 40 for the live misread.
                entry.classification = Some(Classification::ContentChange);
                entry.severity = Some(map_severity(*severity));
                entry.ref_form = Some(map_ref_form(*form));
                entry.repo = Some(tip.repo.clone());
                entry.r#ref = Some(ref_name.clone());
                entry.ref_type_before = Some(map_ref_type(from.ref_type));
                entry.ref_type_after = Some(map_ref_type(to.ref_type));
                entry.from = Some(binding_from(from)?);
                entry.to = Some(binding_to(to)?);
                entry.source_observations = Some(ulids_to_strings(source_observations));
                entry.observation_window_seconds = Some(*observation_window_seconds);
                seqs.insert(ref_name.clone(), next_seq);
                next_seq += 1;
                out.push(entry);
            }
            ClassifiedEvent::Recreation {
                ref_name,
                form,
                severity,
                from,
                to,
                gap,
                same_target,
                observation_window_seconds,
                source_observations,
            } => {
                ensure_sources(source_observations)?;
                let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::Recreation);
                entry.classification = Some(if *same_target {
                    Classification::ReleaseLevelOnly
                } else {
                    Classification::ContentChange
                });
                entry.severity = Some(map_severity(*severity));
                entry.ref_form = Some(map_ref_form(*form));
                entry.repo = Some(tip.repo.clone());
                entry.r#ref = Some(ref_name.clone());
                entry.ref_type_before = Some(map_ref_type(from.ref_type));
                entry.ref_type_after = Some(map_ref_type(to.ref_type));
                entry.from = Some(binding_from(from)?);
                entry.to = Some(binding_to(to)?);
                entry.gap_seconds = Some(gap.whole_seconds().max(0) as u64);
                entry.source_observations = Some(ulids_to_strings(source_observations));
                entry.observation_window_seconds = Some(*observation_window_seconds);
                seqs.insert(ref_name.clone(), next_seq);
                next_seq += 1;
                out.push(entry);
            }
            ClassifiedEvent::RepoUnavailable { http_status, .. } => {
                let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::RepoUnavailable);
                entry.repo = Some(tip.repo.clone());
                entry.http_status = Some(*http_status);
                next_seq += 1;
                out.push(entry);
            }
            ClassifiedEvent::RepoRedirected {
                http_status,
                location,
            } => {
                let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::RepoRedirected);
                entry.repo = Some(tip.repo.clone());
                entry.http_status = Some(*http_status);
                entry.redirect_location = Some(location.clone());
                next_seq += 1;
                out.push(entry);
            }
            ClassifiedEvent::PendingMoveDeferred { .. } => {
                // Visibility is via RepoState + ObservationDigest note, not the chain.
            }
        }
    }

    // One Correlation per unique batch_id seen in this derive call.
    let mut batch_ids: Vec<String> = pending_corr.keys().cloned().collect();
    batch_ids.sort();
    for batch_id in batch_ids {
        let c = pending_corr.get(&batch_id).expect("just keyed");
        let mut member_seqs = Vec::new();
        for r in &c.refs_moved_together {
            let seq = seqs
                .get(r)
                .copied()
                .ok_or_else(|| DeriveError::MissingMemberSeq(r.clone()))?;
            member_seqs.push(seq);
        }
        member_seqs.sort_unstable();
        member_seqs.dedup();

        let corr_seq = next_seq;
        if member_seqs.iter().any(|s| *s >= corr_seq) {
            return Err(DeriveError::MissingMemberSeq(format!(
                "member_seq >= correlation seq {corr_seq}"
            )));
        }

        let mut entry = UnhashedEntry::empty(tip.recorded_at, Event::Correlation);
        entry.repo = Some(tip.repo.clone());
        entry.correlation = Some(Correlation {
            batch_id: c.batch_id.clone(),
            member_seqs,
            refs_moved_together: c.refs_moved_together.clone(),
            all_to_same_target: c.all_to_same_target,
            note: c.note.clone(),
        });
        next_seq += 1;
        out.push(entry);
    }

    let _ = next_seq;
    Ok(out)
}

/// Build a daily ObservationDigest entry. Separate from [`derive`] because
/// digests are not classified events.
pub fn derive_observation_digest(
    recorded_at: OffsetDateTime,
    stats: ObservationDayStats,
) -> Result<UnhashedEntry, DeriveError> {
    if stats.date.len() != 10 {
        return Err(DeriveError::Digest("date must be YYYY-MM-DD".into()));
    }
    let mut files: Vec<ObservationFileDigest> = stats
        .files
        .into_iter()
        .map(|(path, sha256)| ObservationFileDigest { path, sha256 })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = BTreeSet::new();
    for f in &files {
        if !seen.insert(f.path.clone()) {
            return Err(DeriveError::Digest(format!("duplicate path {}", f.path)));
        }
    }
    let mut entry = UnhashedEntry::empty(recorded_at, Event::ObservationDigest);
    entry.observation_digest = Some(ObservationDigest {
        date: stats.date,
        repos_polled: stats.repos_polled,
        ok: stats.ok,
        not_modified: stats.not_modified,
        failed: stats.failed,
        skipped: stats.skipped,
        files,
        note: stats.note,
    });
    Ok(entry)
}

fn ensure_sources(ids: &[Ulid]) -> Result<(), DeriveError> {
    if ids.len() < 2 {
        return Err(DeriveError::SourceObservations);
    }
    Ok(())
}

fn ulids_to_strings(ids: &[Ulid]) -> Vec<String> {
    ids.iter().map(|u| u.to_string()).collect()
}

fn binding_from(snap: &BindingSnapshot) -> Result<Binding, DeriveError> {
    let mut b = Binding::builder()
        .target_sha(&snap.target_sha)
        .commit_sha(&snap.commit_sha)
        .tree_sha(&snap.tree_sha)
        .first_observed(snap.first_observed)
        .last_observed(snap.last_observed)
        .observation_count(snap.observation_count);
    if let Some(a) = &snap.action_yml_sha {
        b = b.action_yml_sha(a);
    }
    b.build().map_err(|e| DeriveError::Binding(e.to_string()))
}

fn binding_to(snap: &BindingSnapshot) -> Result<Binding, DeriveError> {
    // `to` carries first_observed only — last_observed is omitted (stable_days=0).
    let mut b = Binding::builder()
        .target_sha(&snap.target_sha)
        .commit_sha(&snap.commit_sha)
        .tree_sha(&snap.tree_sha)
        .first_observed(snap.first_observed)
        .observation_count(snap.observation_count);
    if let Some(a) = &snap.action_yml_sha {
        b = b.action_yml_sha(a);
    }
    b.build().map_err(|e| DeriveError::Binding(e.to_string()))
}

fn map_kind(kind: &MoveKind) -> Classification {
    match kind {
        MoveKind::ContentChange => Classification::ContentChange,
        MoveKind::CommitMetadataOnly => Classification::CommitMetadataOnly,
        MoveKind::ReleaseLevelOnly { .. } => Classification::ReleaseLevelOnly,
    }
}

fn map_severity(s: Severity) -> LogSeverity {
    match s {
        Severity::High => LogSeverity::High,
        Severity::Medium => LogSeverity::Medium,
        Severity::Low => LogSeverity::Low,
        Severity::Info => LogSeverity::Info,
    }
}

fn map_ref_form(f: RefForm) -> LogRefForm {
    match f {
        RefForm::FloatingMajor => LogRefForm::FloatingMajor,
        RefForm::FloatingMinor => LogRefForm::FloatingMinor,
        RefForm::Exact => LogRefForm::Exact,
        RefForm::NamedChannel => LogRefForm::NamedChannel,
        RefForm::Other => LogRefForm::Other,
    }
}

fn map_ancestry(a: Ancestry) -> LogAncestry {
    match a {
        Ancestry::Ahead => LogAncestry::Ahead,
        Ancestry::Behind => LogAncestry::Behind,
        Ancestry::Diverged => LogAncestry::Diverged,
        Ancestry::Identical => LogAncestry::Identical,
    }
}

fn map_ref_type(t: ObsRefType) -> LogRefType {
    match t {
        ObsRefType::Lightweight => LogRefType::Lightweight,
        ObsRefType::Annotated => LogRefType::Annotated,
    }
}
