//! Log entry schema (spec §9.2).
//!
//! Construction rules that matter for the hash chain live in the type
//! system and constructors — not in comments.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::SerializeStruct;
use serde::{Deserialize as DeserializeDerive, Serialize, Serializer};
use time::OffsetDateTime;

use crate::canonical::{format_timestamp, CanonError, CanonicalValue};

/// Errors produced while constructing or validating entry types.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    #[error("SHA must be exactly 40 lowercase hex characters")]
    InvalidSha40,
    #[error("hash must be \"sha256:\" followed by 64 lowercase hex characters")]
    InvalidHashRef,
    #[error("stable_days is inconsistent with first_observed/last_observed")]
    InconsistentStableDays,
    #[error("observation_window_seconds must be > 0 (a zero-width window is a bug)")]
    NonPositiveObservationWindow,
    #[error("correction requires corrects_seq and reason")]
    CorrectionIncomplete,
    #[error("correction must not carry a diff block")]
    CorrectionForbidsDiff,
    #[error("move/deletion/recreation requires from, to, classification, severity, observation_window_seconds, and source_observations (len >= 2)")]
    MoveIncomplete,
    #[error("missing required binding field")]
    BindingIncomplete,
    #[error("format_version must be 1 (v1 is frozen at genesis)")]
    InvalidFormatVersion,
    #[error("correlation member_seq must be strictly less than this entry's seq")]
    CorrelationMemberSeq,
    #[error("correlation entry is incomplete")]
    CorrelationIncomplete,
    #[error("observation_digest entry is incomplete or files are unsorted")]
    ObservationDigestIncomplete,
    #[error("repo_unavailable/repo_redirected entry is incomplete")]
    RepoIdentityIncomplete,
    #[error("recreation requires gap_seconds")]
    RecreationIncomplete,
    #[error("population_change entry is incomplete")]
    PopulationChangeIncomplete,
    #[error("timestamp error: {0}")]
    Timestamp(String),
    #[error("entry shape error: {0}")]
    Shape(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha40(String);

impl Sha40 {
    pub fn parse(s: impl AsRef<str>) -> Result<Self, EntryError> {
        let s = s.as_ref();
        if s.len() != 40 || !is_lowercase_hex(s) {
            return Err(EntryError::InvalidSha40);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha40 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Sha40 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha40 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Sha40::parse(s).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HashRef(String);

impl HashRef {
    pub const GENESIS: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    pub fn parse(s: impl AsRef<str>) -> Result<Self, EntryError> {
        let s = s.as_ref();
        let rest = s
            .strip_prefix("sha256:")
            .ok_or(EntryError::InvalidHashRef)?;
        if rest.len() != 64 || !is_lowercase_hex(rest) {
            return Err(EntryError::InvalidHashRef);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn genesis() -> Self {
        Self::parse(Self::GENESIS).expect("genesis hash is valid")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HashRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for HashRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HashRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        HashRef::parse(s).map_err(de::Error::custom)
    }
}

fn is_lowercase_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// RFC 3339 UTC timestamp with exact millisecond precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    pub fn from_offset_datetime(t: OffsetDateTime) -> Result<Self, EntryError> {
        format_timestamp(t).map_err(|e| EntryError::Timestamp(e.to_string()))?;
        Ok(Self(t))
    }

    pub fn as_offset_datetime(self) -> OffsetDateTime {
        self.0
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = format_timestamp(self.0).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&s)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        // Reject non-canonical forms (e.g. lowercase "z") so a single-byte
        // wire mutation cannot parse-and-rehash to the same digest.
        if !s.ends_with('Z') || s.len() != "YYYY-MM-DDTHH:MM:SS.sssZ".len() {
            return Err(de::Error::custom(
                "timestamp must be YYYY-MM-DDTHH:MM:SS.sssZ with uppercase Z",
            ));
        }
        let t = OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
            .map_err(de::Error::custom)?;
        let ts = Timestamp::from_offset_datetime(t).map_err(de::Error::custom)?;
        let canonical = format_timestamp(ts.0).map_err(de::Error::custom)?;
        if canonical != s {
            return Err(de::Error::custom(
                "timestamp is not in canonical RFC 3339 millisecond UTC form",
            ));
        }
        Ok(ts)
    }
}

/// Frozen log format version. Any later change is a new version, never an edit.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Event {
    Move,
    Deletion,
    Recreation,
    RepoUnavailable,
    RepoRedirected,
    Correlation,
    ObservationDigest,
    PopulationChange,
    Correction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Classification {
    ContentChange,
    CommitMetadataOnly,
    ReleaseLevelOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Severity {
    High,
    Medium,
    Low,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum RefType {
    Lightweight,
    Annotated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum RefForm {
    FloatingMajor,
    FloatingMinor,
    Exact,
    NamedChannel,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Ancestry {
    Ahead,
    Behind,
    Diverged,
    Identical,
}

/// Observed ref binding. `stable_days` is always derived from the
/// observation span — never trusted from an independent write path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub target_sha: Sha40,
    pub commit_sha: Sha40,
    pub tree_sha: Sha40,
    pub first_observed: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_observed: Option<Timestamp>,
    pub observation_count: u64,
    pub stable_days: u64,
    /// Digest of `action.yml` at this commit. Needed to resolve transitive
    /// action references (Trivy, March 2026).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_yml_sha: Option<Sha40>,
}

impl Binding {
    pub fn builder() -> BindingBuilder<MissingObservationCount> {
        BindingBuilder {
            target_sha: None,
            commit_sha: None,
            tree_sha: None,
            first_observed: None,
            last_observed: None,
            action_yml_sha: None,
            observation_count: MissingObservationCount,
        }
    }

    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    pub fn stable_days(&self) -> u64 {
        self.stable_days
    }

    fn derive_stable_days(first: Timestamp, last: Option<Timestamp>) -> u64 {
        match last {
            Some(last) => {
                let delta = last.as_offset_datetime() - first.as_offset_datetime();
                delta.whole_days().max(0) as u64
            }
            None => 0,
        }
    }

    fn validate_stable_days(&self) -> Result<(), EntryError> {
        let expected = Self::derive_stable_days(self.first_observed, self.last_observed);
        if self.stable_days != expected {
            return Err(EntryError::InconsistentStableDays);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(DeserializeDerive)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            target_sha: Sha40,
            commit_sha: Sha40,
            tree_sha: Sha40,
            first_observed: Timestamp,
            #[serde(default)]
            last_observed: Option<Timestamp>,
            observation_count: u64,
            stable_days: u64,
            #[serde(default)]
            action_yml_sha: Option<Sha40>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let binding = Binding {
            target_sha: raw.target_sha,
            commit_sha: raw.commit_sha,
            tree_sha: raw.tree_sha,
            first_observed: raw.first_observed,
            last_observed: raw.last_observed,
            observation_count: raw.observation_count,
            stable_days: raw.stable_days,
            action_yml_sha: raw.action_yml_sha,
        };
        binding.validate_stable_days().map_err(de::Error::custom)?;
        Ok(binding)
    }
}

/// Type-state marker: `observation_count` not yet supplied.
#[derive(Debug)]
pub struct MissingObservationCount;

/// Type-state marker: `observation_count` has been supplied.
#[derive(Debug)]
pub struct HasObservationCount(u64);

#[derive(Debug)]
pub struct BindingBuilder<Oc> {
    target_sha: Option<String>,
    commit_sha: Option<String>,
    tree_sha: Option<String>,
    first_observed: Option<OffsetDateTime>,
    last_observed: Option<OffsetDateTime>,
    action_yml_sha: Option<String>,
    observation_count: Oc,
}

impl<Oc> BindingBuilder<Oc> {
    pub fn target_sha(mut self, sha: impl Into<String>) -> Self {
        self.target_sha = Some(sha.into());
        self
    }

    pub fn commit_sha(mut self, sha: impl Into<String>) -> Self {
        self.commit_sha = Some(sha.into());
        self
    }

    pub fn tree_sha(mut self, sha: impl Into<String>) -> Self {
        self.tree_sha = Some(sha.into());
        self
    }

    pub fn first_observed(mut self, t: OffsetDateTime) -> Self {
        self.first_observed = Some(t);
        self
    }

    pub fn last_observed(mut self, t: OffsetDateTime) -> Self {
        self.last_observed = Some(t);
        self
    }

    pub fn action_yml_sha(mut self, sha: impl Into<String>) -> Self {
        self.action_yml_sha = Some(sha.into());
        self
    }
}

impl BindingBuilder<MissingObservationCount> {
    /// Required field — supplied via type state, not `Option`.
    pub fn observation_count(self, n: u64) -> BindingBuilder<HasObservationCount> {
        BindingBuilder {
            target_sha: self.target_sha,
            commit_sha: self.commit_sha,
            tree_sha: self.tree_sha,
            first_observed: self.first_observed,
            last_observed: self.last_observed,
            action_yml_sha: self.action_yml_sha,
            observation_count: HasObservationCount(n),
        }
    }
}

impl BindingBuilder<HasObservationCount> {
    pub fn build(self) -> Result<Binding, EntryError> {
        let first_observed = Timestamp::from_offset_datetime(
            self.first_observed.ok_or(EntryError::BindingIncomplete)?,
        )?;
        let last_observed = self
            .last_observed
            .map(Timestamp::from_offset_datetime)
            .transpose()?;
        let stable_days = Binding::derive_stable_days(first_observed, last_observed);
        Ok(Binding {
            target_sha: Sha40::parse(self.target_sha.ok_or(EntryError::BindingIncomplete)?)?,
            commit_sha: Sha40::parse(self.commit_sha.ok_or(EntryError::BindingIncomplete)?)?,
            tree_sha: Sha40::parse(self.tree_sha.ok_or(EntryError::BindingIncomplete)?)?,
            first_observed,
            last_observed,
            observation_count: self.observation_count.0,
            stable_days,
            action_yml_sha: self.action_yml_sha.map(Sha40::parse).transpose()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    pub files_added: u64,
    pub files_removed: u64,
    pub files_modified: u64,
    pub files_renamed: u64,
    pub paths: Vec<String>,
    /// True when `files.len()` met GitHub's compare cap — we cannot know if
    /// the list is complete. Never present a partial path list as exhaustive.
    pub diff_possibly_truncated: bool,
}

/// Payload for a standalone `correlation` event (never inlined on a Move).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    pub batch_id: String,
    pub member_seqs: Vec<u64>,
    pub refs_moved_together: Vec<String>,
    pub all_to_same_target: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(deny_unknown_fields)]
pub struct ObservationFileDigest {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(deny_unknown_fields)]
pub struct ObservationDigest {
    /// UTC calendar date `YYYY-MM-DD`.
    pub date: String,
    pub repos_polled: u64,
    pub ok: u64,
    pub not_modified: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Sorted by `path` ascending.
    pub files: Vec<ObservationFileDigest>,
    /// Recovery annotation (torn write preserved beside the log). Omitted when
    /// there is nothing to record — never serialised as null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Added or removed from the watched population. Removal never deletes history —
/// it records that observation of this `(repo, path)` stops here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum PopulationChangeKind {
    Added,
    Removed,
}

/// Why a `(repo, path)` entered or left the population.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum PopulationReason {
    Seed,
    /// `via` is `owner/repo[@path]@commit` of the parent action that referenced it.
    Transitive {
        via: String,
    },
    Manual,
    Restored,
}

/// Payload for a standalone `population_change` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(deny_unknown_fields)]
pub struct PopulationChange {
    /// Subdirectory action path within `repo`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub change: PopulationChangeKind,
    pub reason: PopulationReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Append-only log entry. Hash via [`Entry::unhashed`] — never by clearing
/// `entry_hash` in place.
///
/// `format_version` is always [`FORMAT_VERSION`]. v1 is frozen at the genesis
/// entry — edits before genesis are free; after genesis, any change is a new
/// `format_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub format_version: u32,
    pub seq: u64,
    pub prev_hash: HashRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_hash: Option<HashRef>,
    pub recorded_at: Timestamp,
    pub event: Event,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_form: Option<RefForm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestry: Option<Ancestry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_type_before: Option<RefType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_type_after: Option<RefType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Binding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Binding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<Diff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_observations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation: Option<Correlation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_digest: Option<ObservationDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub population_change: Option<PopulationChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_window_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detection_latency_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrects_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Entry {
    pub fn builder() -> EntryBuilder {
        EntryBuilder::default()
    }

    /// View used for hashing: identical wire fields with `entry_hash` omitted.
    pub fn unhashed(&self) -> Unhashed<'_> {
        Unhashed { entry: self }
    }

    pub fn from_canonical(value: &CanonicalValue) -> Result<Self, CanonError> {
        let bytes = crate::canonical::canonicalise(value)?;
        serde_json::from_slice(&bytes).map_err(|e| CanonError::EntryShape(e.to_string()))
    }

    pub fn observation_window_seconds(&self) -> Option<u64> {
        self.observation_window_seconds
    }

    pub(crate) fn validate(&self) -> Result<(), EntryError> {
        if self.format_version != FORMAT_VERSION {
            return Err(EntryError::InvalidFormatVersion);
        }
        match self.event {
            Event::Move | Event::Deletion | Event::Recreation => {
                if self.from.is_none() || self.to.is_none() {
                    return Err(EntryError::MoveIncomplete);
                }
                match self.observation_window_seconds {
                    Some(0) | None => return Err(EntryError::NonPositiveObservationWindow),
                    Some(_) => {}
                }
                if self.classification.is_none() || self.severity.is_none() {
                    return Err(EntryError::MoveIncomplete);
                }
                match &self.source_observations {
                    Some(ids) if ids.len() >= 2 => {}
                    _ => return Err(EntryError::MoveIncomplete),
                }
                if self.event == Event::Recreation && self.gap_seconds.is_none() {
                    return Err(EntryError::RecreationIncomplete);
                }
            }
            Event::Correlation => {
                let Some(c) = &self.correlation else {
                    return Err(EntryError::CorrelationIncomplete);
                };
                if c.batch_id.is_empty() || c.member_seqs.is_empty() {
                    return Err(EntryError::CorrelationIncomplete);
                }
                if c.member_seqs.iter().any(|s| *s >= self.seq) {
                    return Err(EntryError::CorrelationMemberSeq);
                }
            }
            Event::ObservationDigest => {
                let Some(d) = &self.observation_digest else {
                    return Err(EntryError::ObservationDigestIncomplete);
                };
                let mut sorted = d.files.clone();
                sorted.sort_by(|a, b| a.path.cmp(&b.path));
                if sorted != d.files {
                    return Err(EntryError::ObservationDigestIncomplete);
                }
                if d.date.len() != 10 {
                    return Err(EntryError::ObservationDigestIncomplete);
                }
            }
            Event::RepoUnavailable => {
                if self.http_status.is_none() {
                    return Err(EntryError::RepoIdentityIncomplete);
                }
            }
            Event::RepoRedirected => {
                if self.http_status.is_none() || self.redirect_location.is_none() {
                    return Err(EntryError::RepoIdentityIncomplete);
                }
            }
            Event::PopulationChange => {
                if self.repo.is_none() || self.population_change.is_none() {
                    return Err(EntryError::PopulationChangeIncomplete);
                }
                if let Some(PopulationReason::Transitive { via }) =
                    self.population_change.as_ref().map(|p| &p.reason)
                {
                    if via.is_empty() || !via.contains('@') {
                        return Err(EntryError::PopulationChangeIncomplete);
                    }
                }
            }
            Event::Correction => {
                if self.corrects_seq.is_none() || self.reason.is_none() {
                    return Err(EntryError::CorrectionIncomplete);
                }
                if self.diff.is_some() {
                    return Err(EntryError::CorrectionForbidsDiff);
                }
            }
        }
        Ok(())
    }
}

/// Serialize-only view of an [`Entry`] with `entry_hash` always omitted.
#[derive(Debug, Clone, Copy)]
pub struct Unhashed<'a> {
    entry: &'a Entry,
}

impl Serialize for Unhashed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let e = self.entry;
        let mut count = 5; // format_version, seq, prev_hash, recorded_at, event
        macro_rules! bump {
            ($($field:ident),*) => {$(
                if e.$field.is_some() {
                    count += 1;
                }
            )*};
        }
        bump!(
            classification,
            severity,
            ref_form,
            ancestry,
            repo,
            r#ref,
            ref_type_before,
            ref_type_after,
            from,
            to,
            diff,
            gap_seconds,
            source_observations,
            correlation,
            observation_digest,
            population_change,
            http_status,
            redirect_location,
            observation_window_seconds,
            detection_latency_note,
            corrects_seq,
            reason
        );

        let mut st = serializer.serialize_struct("Entry", count)?;
        st.serialize_field("format_version", &e.format_version)?;
        st.serialize_field("seq", &e.seq)?;
        st.serialize_field("prev_hash", &e.prev_hash)?;
        st.serialize_field("recorded_at", &e.recorded_at)?;
        st.serialize_field("event", &e.event)?;
        if let Some(v) = &e.classification {
            st.serialize_field("classification", v)?;
        }
        if let Some(v) = &e.severity {
            st.serialize_field("severity", v)?;
        }
        if let Some(v) = &e.ref_form {
            st.serialize_field("ref_form", v)?;
        }
        if let Some(v) = &e.ancestry {
            st.serialize_field("ancestry", v)?;
        }
        if let Some(v) = &e.repo {
            st.serialize_field("repo", v)?;
        }
        if let Some(v) = &e.r#ref {
            st.serialize_field("ref", v)?;
        }
        if let Some(v) = &e.ref_type_before {
            st.serialize_field("ref_type_before", v)?;
        }
        if let Some(v) = &e.ref_type_after {
            st.serialize_field("ref_type_after", v)?;
        }
        if let Some(v) = &e.from {
            st.serialize_field("from", v)?;
        }
        if let Some(v) = &e.to {
            st.serialize_field("to", v)?;
        }
        if let Some(v) = &e.diff {
            st.serialize_field("diff", v)?;
        }
        if let Some(v) = &e.gap_seconds {
            st.serialize_field("gap_seconds", v)?;
        }
        if let Some(v) = &e.source_observations {
            st.serialize_field("source_observations", v)?;
        }
        if let Some(v) = &e.correlation {
            st.serialize_field("correlation", v)?;
        }
        if let Some(v) = &e.observation_digest {
            st.serialize_field("observation_digest", v)?;
        }
        if let Some(v) = &e.population_change {
            st.serialize_field("population_change", v)?;
        }
        if let Some(v) = &e.http_status {
            st.serialize_field("http_status", v)?;
        }
        if let Some(v) = &e.redirect_location {
            st.serialize_field("redirect_location", v)?;
        }
        if let Some(v) = &e.observation_window_seconds {
            st.serialize_field("observation_window_seconds", v)?;
        }
        if let Some(v) = &e.detection_latency_note {
            st.serialize_field("detection_latency_note", v)?;
        }
        if let Some(v) = &e.corrects_seq {
            st.serialize_field("corrects_seq", v)?;
        }
        if let Some(v) = &e.reason {
            st.serialize_field("reason", v)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(DeserializeDerive)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            format_version: u32,
            seq: u64,
            prev_hash: HashRef,
            #[serde(default)]
            entry_hash: Option<HashRef>,
            recorded_at: Timestamp,
            event: Event,
            #[serde(default)]
            classification: Option<Classification>,
            #[serde(default)]
            severity: Option<Severity>,
            #[serde(default)]
            ref_form: Option<RefForm>,
            #[serde(default)]
            ancestry: Option<Ancestry>,
            #[serde(default)]
            repo: Option<String>,
            #[serde(default, rename = "ref")]
            r#ref: Option<String>,
            #[serde(default)]
            ref_type_before: Option<RefType>,
            #[serde(default)]
            ref_type_after: Option<RefType>,
            #[serde(default)]
            from: Option<Binding>,
            #[serde(default)]
            to: Option<Binding>,
            #[serde(default)]
            diff: Option<Diff>,
            #[serde(default)]
            gap_seconds: Option<u64>,
            #[serde(default)]
            source_observations: Option<Vec<String>>,
            #[serde(default)]
            correlation: Option<Correlation>,
            #[serde(default)]
            observation_digest: Option<ObservationDigest>,
            #[serde(default)]
            population_change: Option<PopulationChange>,
            #[serde(default)]
            http_status: Option<u16>,
            #[serde(default)]
            redirect_location: Option<String>,
            #[serde(default)]
            observation_window_seconds: Option<u64>,
            #[serde(default)]
            detection_latency_note: Option<String>,
            #[serde(default)]
            corrects_seq: Option<u64>,
            #[serde(default)]
            reason: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let entry = Entry {
            format_version: raw.format_version,
            seq: raw.seq,
            prev_hash: raw.prev_hash,
            entry_hash: raw.entry_hash,
            recorded_at: raw.recorded_at,
            event: raw.event,
            classification: raw.classification,
            severity: raw.severity,
            ref_form: raw.ref_form,
            ancestry: raw.ancestry,
            repo: raw.repo,
            r#ref: raw.r#ref,
            ref_type_before: raw.ref_type_before,
            ref_type_after: raw.ref_type_after,
            from: raw.from,
            to: raw.to,
            diff: raw.diff,
            gap_seconds: raw.gap_seconds,
            source_observations: raw.source_observations,
            correlation: raw.correlation,
            observation_digest: raw.observation_digest,
            population_change: raw.population_change,
            http_status: raw.http_status,
            redirect_location: raw.redirect_location,
            observation_window_seconds: raw.observation_window_seconds,
            detection_latency_note: raw.detection_latency_note,
            corrects_seq: raw.corrects_seq,
            reason: raw.reason,
        };
        entry.validate().map_err(de::Error::custom)?;
        Ok(entry)
    }
}

#[derive(Debug, Default)]
pub struct EntryBuilder {
    seq: Option<u64>,
    prev_hash: Option<String>,
    entry_hash: Option<String>,
    recorded_at: Option<OffsetDateTime>,
    event: Option<Event>,
    classification: Option<Classification>,
    severity: Option<Severity>,
    ref_form: Option<RefForm>,
    ancestry: Option<Ancestry>,
    repo: Option<String>,
    r#ref: Option<String>,
    ref_type_before: Option<RefType>,
    ref_type_after: Option<RefType>,
    from: Option<Binding>,
    to: Option<Binding>,
    diff: Option<Diff>,
    gap_seconds: Option<u64>,
    source_observations: Option<Vec<String>>,
    correlation: Option<Correlation>,
    observation_digest: Option<ObservationDigest>,
    population_change: Option<PopulationChange>,
    http_status: Option<u16>,
    redirect_location: Option<String>,
    observation_window_seconds: Option<u64>,
    detection_latency_note: Option<String>,
    corrects_seq: Option<u64>,
    reason: Option<String>,
}

impl EntryBuilder {
    pub fn seq(mut self, seq: u64) -> Self {
        self.seq = Some(seq);
        self
    }
    pub fn prev_hash(mut self, h: impl Into<String>) -> Self {
        self.prev_hash = Some(h.into());
        self
    }
    pub fn entry_hash(mut self, h: impl Into<String>) -> Self {
        self.entry_hash = Some(h.into());
        self
    }
    pub fn recorded_at(mut self, t: OffsetDateTime) -> Self {
        self.recorded_at = Some(t);
        self
    }
    pub fn event(mut self, event: Event) -> Self {
        self.event = Some(event);
        self
    }
    pub fn classification(mut self, c: Classification) -> Self {
        self.classification = Some(c);
        self
    }
    pub fn severity(mut self, s: Severity) -> Self {
        self.severity = Some(s);
        self
    }
    pub fn ref_form(mut self, f: RefForm) -> Self {
        self.ref_form = Some(f);
        self
    }
    pub fn ancestry(mut self, a: Ancestry) -> Self {
        self.ancestry = Some(a);
        self
    }
    pub fn repo(mut self, repo: impl Into<String>) -> Self {
        self.repo = Some(repo.into());
        self
    }
    pub fn ref_name(mut self, name: impl Into<String>) -> Self {
        self.r#ref = Some(name.into());
        self
    }
    pub fn ref_type_before(mut self, t: RefType) -> Self {
        self.ref_type_before = Some(t);
        self
    }
    pub fn ref_type_after(mut self, t: RefType) -> Self {
        self.ref_type_after = Some(t);
        self
    }
    pub fn from(mut self, b: Binding) -> Self {
        self.from = Some(b);
        self
    }
    pub fn to(mut self, b: Binding) -> Self {
        self.to = Some(b);
        self
    }
    pub fn diff(mut self, d: Diff) -> Self {
        self.diff = Some(d);
        self
    }
    pub fn gap_seconds(mut self, secs: u64) -> Self {
        self.gap_seconds = Some(secs);
        self
    }
    pub fn source_observations(mut self, ids: Vec<String>) -> Self {
        self.source_observations = Some(ids);
        self
    }
    pub fn correlation(mut self, c: Correlation) -> Self {
        self.correlation = Some(c);
        self
    }
    pub fn observation_digest(mut self, d: ObservationDigest) -> Self {
        self.observation_digest = Some(d);
        self
    }
    pub fn population_change(mut self, p: PopulationChange) -> Self {
        self.population_change = Some(p);
        self
    }
    pub fn http_status(mut self, s: u16) -> Self {
        self.http_status = Some(s);
        self
    }
    pub fn redirect_location(mut self, loc: impl Into<String>) -> Self {
        self.redirect_location = Some(loc.into());
        self
    }
    pub fn observation_window_seconds(mut self, secs: u64) -> Self {
        self.observation_window_seconds = Some(secs);
        self
    }
    pub fn detection_latency_note(mut self, note: impl Into<String>) -> Self {
        self.detection_latency_note = Some(note.into());
        self
    }
    pub fn corrects_seq(mut self, seq: u64) -> Self {
        self.corrects_seq = Some(seq);
        self
    }
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn build(self) -> Result<Entry, EntryError> {
        let entry = Entry {
            format_version: FORMAT_VERSION,
            seq: self.seq.ok_or(EntryError::Shape("seq required".into()))?,
            prev_hash: HashRef::parse(
                self.prev_hash
                    .ok_or(EntryError::Shape("prev_hash required".into()))?,
            )?,
            entry_hash: self.entry_hash.map(HashRef::parse).transpose()?,
            recorded_at: Timestamp::from_offset_datetime(
                self.recorded_at
                    .ok_or(EntryError::Shape("recorded_at required".into()))?,
            )?,
            event: self
                .event
                .ok_or(EntryError::Shape("event required".into()))?,
            classification: self.classification,
            severity: self.severity,
            ref_form: self.ref_form,
            ancestry: self.ancestry,
            repo: self.repo,
            r#ref: self.r#ref,
            ref_type_before: self.ref_type_before,
            ref_type_after: self.ref_type_after,
            from: self.from,
            to: self.to,
            diff: self.diff,
            gap_seconds: self.gap_seconds,
            source_observations: self.source_observations,
            correlation: self.correlation,
            observation_digest: self.observation_digest,
            population_change: self.population_change,
            http_status: self.http_status,
            redirect_location: self.redirect_location,
            observation_window_seconds: self.observation_window_seconds,
            detection_latency_note: self.detection_latency_note,
            corrects_seq: self.corrects_seq,
            reason: self.reason,
        };
        entry.validate()?;
        Ok(entry)
    }
}
