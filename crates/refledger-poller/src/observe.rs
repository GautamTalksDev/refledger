//! Observation model (spec §9.1).
//!
//! An observation records the **conditions** of a poll — including polls that
//! returned 304 Not Modified and polls that failed or were skipped — not merely
//! a successful refs payload.
//!
//! Why 304s and failures are stored: a binding observed unchanged across 400
//! polls over 148 days is the evidence that makes a later move significant, and
//! a gap in coverage must be visible in the data rather than inferred from
//! absence. Reuse of this crate without that rule produces a record that cannot
//! support its own claims.
//!
//! Observations are the raw material and are **not** signed. Only derived log
//! entries enter the hash chain. Do not attempt to hash-chain observations —
//! the volume makes it impractical, and that is intentional.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::marker::PhantomData;
use std::path::Path;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::SerializeStruct;
use serde::{Deserialize as DeserializeDerive, Serialize, Serializer};
use thiserror::Error;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

use refledger_log::normalize_to_utc_millis;

/// Errors from constructing or persisting observations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ObservationError {
    #[error("SHA must be exactly 40 lowercase hex characters")]
    InvalidSha40,
    #[error("annotated tag requires target_sha (tag object) != commit_sha")]
    AnnotatedShaCollision,
    #[error("repo slug must be owner/name")]
    InvalidRepoSlug,
    #[error("Failed outcome with status 403 or 429 requires secondary_limit_observed")]
    MissingSecondaryLimit,
    #[error("missing required builder field: {0}")]
    MissingField(&'static str),
    #[error("timestamp must be UTC with exact millisecond precision")]
    Timestamp,
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
}

/// 40-character lowercase hex git object id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, DeserializeDerive)]
#[serde(transparent)]
pub struct Sha40(String);

impl Sha40 {
    pub fn parse(s: impl AsRef<str>) -> Result<Self, ObservationError> {
        let s = s.as_ref();
        if s.len() != 40 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(ObservationError::InvalidSha40);
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

/// `owner/name` repository slug.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, DeserializeDerive)]
#[serde(transparent)]
pub struct RepoSlug(String);

impl RepoSlug {
    pub fn parse(s: impl AsRef<str>) -> Result<Self, ObservationError> {
        let s = s.as_ref();
        let mut parts = s.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty() => Ok(Self(s.to_owned())),
            _ => Err(ObservationError::InvalidRepoSlug),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Filesystem-safe form (`owner--name`) for observation JSONL paths.
    pub fn path_segment(&self) -> String {
        self.0.replace('/', "--")
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// RFC 3339 UTC timestamp with exact millisecond precision (`…sssZ`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    pub fn from_offset_datetime(t: OffsetDateTime) -> Result<Self, ObservationError> {
        if t.offset() != time::UtcOffset::UTC {
            return Err(ObservationError::Timestamp);
        }
        if t.nanosecond() % 1_000_000 != 0 {
            return Err(ObservationError::Timestamp);
        }
        Ok(Self(t))
    }

    /// Normalise an external instant then wrap. Prefer this at clock/env/HTTP
    /// boundaries; keep [`Self::from_offset_datetime`] strict for wire forms.
    pub fn from_external(t: OffsetDateTime) -> Result<Self, ObservationError> {
        Self::from_offset_datetime(normalize_to_utc_millis(t))
    }

    pub fn as_offset_datetime(self) -> OffsetDateTime {
        self.0
    }

    fn format(self) -> String {
        let t = self.0;
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
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.format())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if !s.ends_with('Z') || s.len() != "YYYY-MM-DDTHH:MM:SS.sssZ".len() {
            return Err(de::Error::custom("canonical …sssZ required"));
        }
        let t = OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
            .map_err(de::Error::custom)?;
        let ts = Timestamp::from_offset_datetime(t).map_err(de::Error::custom)?;
        if ts.format() != s {
            return Err(de::Error::custom("non-canonical timestamp"));
        }
        Ok(ts)
    }
}

/// Crate version stamped at compile time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(transparent)]
pub struct Version(String);

/// First poller version whose observations trust commit trees only from the
/// object cache and refuse a same-commit `content_change`.
///
/// Older observations keep the pre-fix classifier so a replay of the signed
/// chain stays byte-for-byte identical.
pub const TREE_TRUST_POLLER_VERSION: &str = "0.1.1";

impl Version {
    pub fn crate_version() -> Self {
        Self(env!("CARGO_PKG_VERSION").to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Observations stamped at or after [`TREE_TRUST_POLLER_VERSION`].
    pub fn trusts_cached_trees(&self) -> bool {
        version_at_least(&self.0, TREE_TRUST_POLLER_VERSION)
    }
}

/// Numeric dotted-version compare (`0.1.10` > `0.1.1`). Non-numeric suffixes
/// are ignored so a dirty local version still orders by its numeric prefix.
fn version_at_least(version: &str, minimum: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    }
    let have = parts(version);
    let need = parts(minimum);
    let n = have.len().max(need.len());
    for i in 0..n {
        let a = have.get(i).copied().unwrap_or(0);
        let b = need.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

/// HTTP API family used for the poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Rest,
    GraphQl,
}

/// Why a poll was sent with no `If-None-Match`.
///
/// A 200 where a 304 was expected is only visible to a coverage audit if the
/// reason is stored on the observation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum RefreshReason {
    /// The stored validator was older than the configured max age.
    EtagMaxAge,
}

/// Git ref kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum RefType {
    Lightweight,
    Annotated,
}

/// Ultimate object type after peeling tag objects.
///
/// A tag may legally point at a tree or blob. Those are recorded with this
/// type and never fed to commit→tree resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum PeeledType {
    #[default]
    Commit,
    Tree,
    Blob,
}

impl PeeledType {
    fn is_commit(&self) -> bool {
        matches!(self, Self::Commit)
    }
}

/// Opaque ETag string from GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(transparent)]
pub struct ETag(String);

impl ETag {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    SecondaryRateLimit,
    PrimaryRateLimit,
    /// Legacy HTTP-upstream label kept for deserialising older observations.
    Upstream,
    /// GitHub returned a 4xx client error (bad request, not found, etc.).
    ApiClient,
    /// GitHub returned a 5xx server error.
    ApiServer,
    /// Transport failed before a usable HTTP status (DNS, TLS, timeout, budget).
    Network,
    Protocol,
}

/// Map an HTTP status to the observation error class.
///
/// Transport failures (no status) stay [`ErrorClass::Network`]. Rate-limit
/// statuses keep their dedicated classes when the caller already classified
/// them; this helper covers ordinary 4xx/5xx responses.
pub fn error_class_for_http(status: u16) -> ErrorClass {
    match status {
        403 | 429 => ErrorClass::SecondaryRateLimit, // callers may refine
        400..=499 => ErrorClass::ApiClient,
        500..=599 => ErrorClass::ApiServer,
        _ => ErrorClass::Upstream,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    BudgetExhausted,
    SecondaryLimitBackoff,
    ShutdownMidSweep,
    /// Sweep missed its window by more than twice the poll interval.
    /// `scheduled` is when the run was supposed to start; `actual` is when it did.
    SchedulerLag {
        scheduled: Timestamp,
        actual: Timestamp,
    },
    /// Poller was down long enough that the gap would otherwise be silent.
    /// `from` is the latest prior `observed_at` for the group; `to` is restart.
    PollerDown {
        from: Timestamp,
        to: Timestamp,
    },
}

/// Captured when GitHub refuses a request under secondary-limit pressure.
/// There is no header for this state — recording refusals is how we learn our ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
pub struct SecondaryLimitEvent {
    pub status: u16,
    #[serde(with = "duration_secs_option")]
    pub retry_after: Option<Duration>,
    pub request_rate_rpm: u32,
}

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(d.whole_seconds())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = i64::deserialize(d)?;
        Ok(Duration::seconds(secs))
    }
}

mod duration_secs_option {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(dur) => s.serialize_some(&dur.whole_seconds()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let secs: Option<i64> = Option::deserialize(d)?;
        Ok(secs.map(Duration::seconds))
    }
}

/// Result of a single poll attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        http_status: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        etag: Option<ETag>,
        refs: Vec<ObservedRef>,
    },
    NotModified {
        http_status: u16,
        etag: ETag,
    },
    Failed {
        http_status: u16,
        error_class: ErrorClass,
        #[serde(with = "duration_secs")]
        backoff_applied: Duration,
    },
    Skipped {
        reason: SkipReason,
    },
}

impl Outcome {
    pub fn is_not_modified(&self) -> bool {
        matches!(self, Self::NotModified { .. })
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Ok { http_status, .. }
            | Self::NotModified { http_status, .. }
            | Self::Failed { http_status, .. } => Some(*http_status),
            Self::Skipped { .. } => None,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// One ref as resolved during an observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, DeserializeDerive)]
pub struct ObservedRef {
    name: String,
    ref_type: RefType,
    target_sha: Sha40,
    /// Peeled commit. Absent when [`peeled_type`] is tree or blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<Sha40>,
    /// Tree of the peeled commit. Absent when the peel is not a commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    tree_sha: Option<Sha40>,
    #[serde(default, skip_serializing_if = "PeeledType::is_commit")]
    peeled_type: PeeledType,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_yml_sha: Option<Sha40>,
}

impl ObservedRef {
    /// Lightweight tag: one object SHA; `target_sha == commit_sha`.
    pub fn new_lightweight(
        name: impl Into<String>,
        sha: impl AsRef<str>,
        tree_sha: impl AsRef<str>,
    ) -> Result<Self, ObservationError> {
        let sha = Sha40::parse(sha)?;
        Ok(Self {
            name: name.into(),
            ref_type: RefType::Lightweight,
            commit_sha: Some(sha.clone()),
            target_sha: sha,
            tree_sha: Some(Sha40::parse(tree_sha)?),
            peeled_type: PeeledType::Commit,
            action_yml_sha: None,
        })
    }

    /// Annotated tag: tag-object SHA and commit SHA must differ.
    pub fn new_annotated(
        name: impl Into<String>,
        tag_object_sha: impl AsRef<str>,
        commit_sha: impl AsRef<str>,
        tree_sha: impl AsRef<str>,
    ) -> Result<Self, ObservationError> {
        let target_sha = Sha40::parse(tag_object_sha)?;
        let commit_sha = Sha40::parse(commit_sha)?;
        if target_sha == commit_sha {
            return Err(ObservationError::AnnotatedShaCollision);
        }
        Ok(Self {
            name: name.into(),
            ref_type: RefType::Annotated,
            target_sha,
            commit_sha: Some(commit_sha),
            tree_sha: Some(Sha40::parse(tree_sha)?),
            peeled_type: PeeledType::Commit,
            action_yml_sha: None,
        })
    }

    /// Listing-only ref: target known, peel not yet resolved.
    ///
    /// Used when phase-1 tag listing has the tip SHA but the object cache has
    /// no commit tree yet. Never invents a `tree_sha` (a commit's tree is
    /// immutable; using the commit SHA as a stand-in corrupts classification).
    pub fn new_unpeeled(
        name: impl Into<String>,
        ref_type: RefType,
        target_sha: impl AsRef<str>,
    ) -> Result<Self, ObservationError> {
        Ok(Self {
            name: name.into(),
            ref_type,
            target_sha: Sha40::parse(target_sha)?,
            commit_sha: None,
            tree_sha: None,
            peeled_type: PeeledType::Commit,
            action_yml_sha: None,
        })
    }

    /// Tag whose peel is a tree or blob — legal git, not a poller error.
    ///
    /// The peeled object oid is stored in `tree_sha` and exposed via
    /// [`Self::peeled_object_sha`]. Commit resolution is not attempted.
    pub fn new_non_commit(
        name: impl Into<String>,
        ref_type: RefType,
        target_sha: impl AsRef<str>,
        peeled_type: PeeledType,
        object_sha: impl AsRef<str>,
    ) -> Result<Self, ObservationError> {
        if matches!(peeled_type, PeeledType::Commit) {
            return Err(ObservationError::MissingField("non_commit_peeled_type"));
        }
        Ok(Self {
            name: name.into(),
            ref_type,
            target_sha: Sha40::parse(target_sha)?,
            commit_sha: None,
            tree_sha: Some(Sha40::parse(object_sha)?),
            peeled_type,
            action_yml_sha: None,
        })
    }

    pub fn with_action_yml_sha(mut self, sha: impl AsRef<str>) -> Result<Self, ObservationError> {
        self.action_yml_sha = Some(Sha40::parse(sha)?);
        Ok(self)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ref_type(&self) -> RefType {
        self.ref_type
    }

    pub fn target_sha(&self) -> &str {
        self.target_sha.as_str()
    }

    pub fn commit_sha(&self) -> Option<&str> {
        self.commit_sha.as_ref().map(|s| s.as_str())
    }

    pub fn tree_sha(&self) -> Option<&str> {
        self.tree_sha.as_ref().map(|s| s.as_str())
    }

    pub fn peeled_type(&self) -> PeeledType {
        self.peeled_type
    }

    pub fn action_yml_sha(&self) -> Option<&str> {
        self.action_yml_sha.as_ref().map(|s| s.as_str())
    }

    /// Peeled tree/blob oid when [`peeled_type`] is not commit.
    pub fn peeled_object_sha(&self) -> Option<&str> {
        match self.peeled_type {
            PeeledType::Commit => None,
            PeeledType::Tree | PeeledType::Blob => self.tree_sha(),
        }
    }
}

/// One poll of one repository. No public fields; no `Default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    observation_id: Ulid,
    repo: RepoSlug,
    observed_at: Timestamp,
    poller_version: Version,
    method: Method,
    refresh_reason: Option<RefreshReason>,
    outcome: Outcome,
    rate_limit_remaining: Option<u32>,
    secondary_limit_observed: Option<SecondaryLimitEvent>,
    /// Set when GitHub answers with a rename/transfer redirect. Never followed.
    redirect_location: Option<String>,
    /// From `GET /repos/{owner}/{repo}` when known. Archived repos stay in the set.
    archived: Option<bool>,
    /// Subdirectory action path (`owner/repo/path@ref`). Population must key on
    /// `(repo, path)`, not repo alone.
    action_path: Option<String>,
    /// Workflow scheduled time for this run (GitHub Actions `scheduled` event).
    scheduled_at: Option<Timestamp>,
    /// When this workflow run actually began (schedule lag is measured data).
    actual_start: Option<Timestamp>,
}

impl Observation {
    pub fn builder(
    ) -> ObservationBuilder<MissingRepo, MissingObservedAt, MissingMethod, MissingOutcome> {
        ObservationBuilder {
            repo: None,
            observed_at: None,
            method: None,
            outcome: None,
            refresh_reason: None,
            rate_limit_remaining: None,
            secondary_limit_observed: None,
            redirect_location: None,
            archived: None,
            action_path: None,
            scheduled_at: None,
            actual_start: None,
            _repo: PhantomData,
            _observed_at: PhantomData,
            _method: PhantomData,
            _outcome: PhantomData,
        }
    }

    pub fn observation_id(&self) -> Ulid {
        self.observation_id
    }

    /// Replace the auto-assigned id (recovery re-derive must cite the archive row).
    pub fn with_observation_id(mut self, id: Ulid) -> Self {
        self.observation_id = id;
        self
    }

    /// Keep the archived `poller_version` when a row is rebuilt for replay.
    ///
    /// The builder always stamps [`Version::crate_version`]. Recovery must not
    /// upgrade an old observation onto the tree-trust rules, or the historic
    /// chain would not replay.
    pub(crate) fn with_archived_poller_version(mut self, version: Version) -> Self {
        self.poller_version = version;
        self
    }

    pub fn repo(&self) -> &RepoSlug {
        &self.repo
    }

    pub fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub fn poller_version(&self) -> &Version {
        &self.poller_version
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn refresh_reason(&self) -> Option<RefreshReason> {
        self.refresh_reason
    }

    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    pub fn rate_limit_remaining(&self) -> Option<u32> {
        self.rate_limit_remaining
    }

    pub fn secondary_limit_observed(&self) -> Option<&SecondaryLimitEvent> {
        self.secondary_limit_observed.as_ref()
    }

    pub fn redirect_location(&self) -> Option<&str> {
        self.redirect_location.as_deref()
    }

    pub fn archived(&self) -> Option<bool> {
        self.archived
    }

    pub fn action_path(&self) -> Option<&str> {
        self.action_path.as_deref()
    }

    pub fn scheduled_at(&self) -> Option<Timestamp> {
        self.scheduled_at
    }

    pub fn actual_start(&self) -> Option<Timestamp> {
        self.actual_start
    }

    /// Stamp schedule lag fields after build (workflow once-path).
    pub fn stamp_schedule(&mut self, scheduled: Timestamp, actual: Timestamp) {
        self.scheduled_at = Some(scheduled);
        self.actual_start = Some(actual);
    }

    pub fn refs(&self) -> &[ObservedRef] {
        match &self.outcome {
            Outcome::Ok { refs, .. } => refs,
            _ => &[],
        }
    }

    pub fn etag(&self) -> Option<&str> {
        match &self.outcome {
            Outcome::Ok { etag: Some(e), .. } | Outcome::NotModified { etag: e, .. } => {
                Some(e.as_str())
            }
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        self.outcome.is_error()
    }

    pub fn http_status(&self) -> Option<u16> {
        self.outcome.http_status()
    }
}

impl Serialize for Observation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut count = 6; // id, repo, observed_at, poller_version, method, outcome
        if self.refresh_reason.is_some() {
            count += 1;
        }
        if self.rate_limit_remaining.is_some() {
            count += 1;
        }
        if self.secondary_limit_observed.is_some() {
            count += 1;
        }
        if self.redirect_location.is_some() {
            count += 1;
        }
        if self.archived.is_some() {
            count += 1;
        }
        if self.action_path.is_some() {
            count += 1;
        }
        if self.scheduled_at.is_some() {
            count += 1;
        }
        if self.actual_start.is_some() {
            count += 1;
        }
        let mut st = serializer.serialize_struct("Observation", count)?;
        st.serialize_field("observation_id", &self.observation_id.to_string())?;
        st.serialize_field("repo", &self.repo)?;
        st.serialize_field("observed_at", &self.observed_at)?;
        st.serialize_field("poller_version", &self.poller_version)?;
        st.serialize_field("method", &self.method)?;
        if let Some(reason) = self.refresh_reason {
            st.serialize_field("refresh_reason", &reason)?;
        }
        st.serialize_field("outcome", &self.outcome)?;
        if let Some(r) = self.rate_limit_remaining {
            st.serialize_field("rate_limit_remaining", &r)?;
        }
        if let Some(s) = &self.secondary_limit_observed {
            st.serialize_field("secondary_limit_observed", s)?;
        }
        if let Some(loc) = &self.redirect_location {
            st.serialize_field("redirect_location", loc)?;
        }
        if let Some(archived) = self.archived {
            st.serialize_field("archived", &archived)?;
        }
        if let Some(path) = &self.action_path {
            st.serialize_field("action_path", path)?;
        }
        if let Some(t) = self.scheduled_at {
            st.serialize_field("scheduled_at", &t)?;
        }
        if let Some(t) = self.actual_start {
            st.serialize_field("actual_start", &t)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for Observation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(DeserializeDerive)]
        struct Raw {
            observation_id: String,
            repo: RepoSlug,
            observed_at: Timestamp,
            poller_version: Version,
            method: Method,
            #[serde(default)]
            refresh_reason: Option<RefreshReason>,
            outcome: Outcome,
            #[serde(default)]
            rate_limit_remaining: Option<u32>,
            #[serde(default)]
            secondary_limit_observed: Option<SecondaryLimitEvent>,
            #[serde(default)]
            redirect_location: Option<String>,
            #[serde(default)]
            archived: Option<bool>,
            #[serde(default)]
            action_path: Option<String>,
            #[serde(default)]
            scheduled_at: Option<Timestamp>,
            #[serde(default)]
            actual_start: Option<Timestamp>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let observation_id =
            Ulid::from_string(&raw.observation_id).map_err(|e| de::Error::custom(e.to_string()))?;
        let obs = Observation {
            observation_id,
            repo: raw.repo,
            observed_at: raw.observed_at,
            poller_version: raw.poller_version,
            method: raw.method,
            refresh_reason: raw.refresh_reason,
            outcome: raw.outcome,
            rate_limit_remaining: raw.rate_limit_remaining,
            secondary_limit_observed: raw.secondary_limit_observed,
            redirect_location: raw.redirect_location,
            archived: raw.archived,
            action_path: raw.action_path,
            scheduled_at: raw.scheduled_at,
            actual_start: raw.actual_start,
        };
        validate_secondary(&obs).map_err(de::Error::custom)?;
        Ok(obs)
    }
}

fn validate_secondary(obs: &Observation) -> Result<(), ObservationError> {
    if let Outcome::Failed { http_status, .. } = obs.outcome {
        if matches!(http_status, 403 | 429) && obs.secondary_limit_observed.is_none() {
            return Err(ObservationError::MissingSecondaryLimit);
        }
    }
    Ok(())
}

// --- Type-state builder -------------------------------------------------------

pub struct MissingRepo;
pub struct HasRepo;
pub struct MissingObservedAt;
pub struct HasObservedAt;
pub struct MissingMethod;
pub struct HasMethod;
pub struct MissingOutcome;
pub struct HasOutcome;

pub struct ObservationBuilder<R, Oa, M, Oc> {
    repo: Option<RepoSlug>,
    observed_at: Option<Timestamp>,
    method: Option<Method>,
    outcome: Option<Outcome>,
    refresh_reason: Option<RefreshReason>,
    rate_limit_remaining: Option<u32>,
    secondary_limit_observed: Option<SecondaryLimitEvent>,
    redirect_location: Option<String>,
    archived: Option<bool>,
    action_path: Option<String>,
    scheduled_at: Option<Timestamp>,
    actual_start: Option<Timestamp>,
    _repo: PhantomData<R>,
    _observed_at: PhantomData<Oa>,
    _method: PhantomData<M>,
    _outcome: PhantomData<Oc>,
}

impl ObservationBuilder<MissingRepo, MissingObservedAt, MissingMethod, MissingOutcome> {
    pub fn new() -> Self {
        Observation::builder()
    }
}

impl Default for ObservationBuilder<MissingRepo, MissingObservedAt, MissingMethod, MissingOutcome> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, Oa, M, Oc> ObservationBuilder<R, Oa, M, Oc> {
    pub fn refresh_reason(mut self, reason: RefreshReason) -> Self {
        self.refresh_reason = Some(reason);
        self
    }

    pub fn rate_limit_remaining(mut self, n: u32) -> Self {
        self.rate_limit_remaining = Some(n);
        self
    }

    pub fn secondary_limit_observed(mut self, ev: SecondaryLimitEvent) -> Self {
        self.secondary_limit_observed = Some(ev);
        self
    }

    pub fn redirect_location(mut self, location: impl Into<String>) -> Self {
        self.redirect_location = Some(location.into());
        self
    }

    pub fn archived(mut self, archived: bool) -> Self {
        self.archived = Some(archived);
        self
    }

    pub fn action_path(mut self, path: impl Into<String>) -> Self {
        self.action_path = Some(path.into());
        self
    }

    pub fn scheduled_at(mut self, t: Timestamp) -> Self {
        self.scheduled_at = Some(t);
        self
    }

    pub fn actual_start(mut self, t: Timestamp) -> Self {
        self.actual_start = Some(t);
        self
    }
}

impl<Oa, M, Oc> ObservationBuilder<MissingRepo, Oa, M, Oc> {
    pub fn repo(
        self,
        slug: impl AsRef<str>,
    ) -> Result<ObservationBuilder<HasRepo, Oa, M, Oc>, ObservationError> {
        Ok(ObservationBuilder {
            repo: Some(RepoSlug::parse(slug)?),
            observed_at: self.observed_at,
            method: self.method,
            outcome: self.outcome,
            refresh_reason: self.refresh_reason,
            rate_limit_remaining: self.rate_limit_remaining,
            secondary_limit_observed: self.secondary_limit_observed,
            redirect_location: self.redirect_location,
            archived: self.archived,
            action_path: self.action_path,
            scheduled_at: self.scheduled_at,
            actual_start: self.actual_start,
            _repo: PhantomData,
            _observed_at: PhantomData,
            _method: PhantomData,
            _outcome: PhantomData,
        })
    }
}

impl<R, M, Oc> ObservationBuilder<R, MissingObservedAt, M, Oc> {
    pub fn observed_at(
        self,
        t: OffsetDateTime,
    ) -> Result<ObservationBuilder<R, HasObservedAt, M, Oc>, ObservationError> {
        Ok(ObservationBuilder {
            repo: self.repo,
            observed_at: Some(Timestamp::from_external(t)?),
            method: self.method,
            outcome: self.outcome,
            refresh_reason: self.refresh_reason,
            rate_limit_remaining: self.rate_limit_remaining,
            secondary_limit_observed: self.secondary_limit_observed,
            redirect_location: self.redirect_location,
            archived: self.archived,
            action_path: self.action_path,
            scheduled_at: self.scheduled_at,
            actual_start: self.actual_start,
            _repo: PhantomData,
            _observed_at: PhantomData,
            _method: PhantomData,
            _outcome: PhantomData,
        })
    }
}

impl<R, Oa, Oc> ObservationBuilder<R, Oa, MissingMethod, Oc> {
    pub fn method(self, method: Method) -> ObservationBuilder<R, Oa, HasMethod, Oc> {
        ObservationBuilder {
            repo: self.repo,
            observed_at: self.observed_at,
            method: Some(method),
            outcome: self.outcome,
            refresh_reason: self.refresh_reason,
            rate_limit_remaining: self.rate_limit_remaining,
            secondary_limit_observed: self.secondary_limit_observed,
            redirect_location: self.redirect_location,
            archived: self.archived,
            action_path: self.action_path,
            scheduled_at: self.scheduled_at,
            actual_start: self.actual_start,
            _repo: PhantomData,
            _observed_at: PhantomData,
            _method: PhantomData,
            _outcome: PhantomData,
        }
    }
}

impl<R, Oa, M> ObservationBuilder<R, Oa, M, MissingOutcome> {
    pub fn outcome(self, outcome: Outcome) -> ObservationBuilder<R, Oa, M, HasOutcome> {
        ObservationBuilder {
            repo: self.repo,
            observed_at: self.observed_at,
            method: self.method,
            outcome: Some(outcome),
            refresh_reason: self.refresh_reason,
            rate_limit_remaining: self.rate_limit_remaining,
            secondary_limit_observed: self.secondary_limit_observed,
            redirect_location: self.redirect_location,
            archived: self.archived,
            action_path: self.action_path,
            scheduled_at: self.scheduled_at,
            actual_start: self.actual_start,
            _repo: PhantomData,
            _observed_at: PhantomData,
            _method: PhantomData,
            _outcome: PhantomData,
        }
    }
}

impl ObservationBuilder<HasRepo, HasObservedAt, HasMethod, HasOutcome> {
    pub fn build(self) -> Result<Observation, ObservationError> {
        let obs = Observation {
            observation_id: Ulid::new(),
            repo: self.repo.ok_or(ObservationError::MissingField("repo"))?,
            observed_at: self
                .observed_at
                .ok_or(ObservationError::MissingField("observed_at"))?,
            // Stamped from the crate version at compile time — never a literal setter.
            poller_version: Version::crate_version(),
            method: self
                .method
                .ok_or(ObservationError::MissingField("method"))?,
            refresh_reason: self.refresh_reason,
            outcome: self
                .outcome
                .ok_or(ObservationError::MissingField("outcome"))?,
            rate_limit_remaining: self.rate_limit_remaining,
            secondary_limit_observed: self.secondary_limit_observed,
            redirect_location: self.redirect_location,
            archived: self.archived,
            action_path: self.action_path,
            scheduled_at: self.scheduled_at,
            actual_start: self.actual_start,
        };
        validate_secondary(&obs)?;
        Ok(obs)
    }
}

/// Append one observation to
/// `data/observations/YYYY/MM/DD/<repo-slug>.jsonl` with `O_APPEND` + `fsync`.
///
/// A log that loses its last observation on power failure is a log that will
/// one day be missing exactly the observation that mattered.
pub fn store_observation(obs: &Observation) -> Result<bool, ObservationError> {
    store_observation_at(obs, Path::new("data/observations"))
}

pub fn store_observation_at(obs: &Observation, root: &Path) -> Result<bool, ObservationError> {
    let t = obs.observed_at.as_offset_datetime();
    let dir = root.join(format!(
        "{:04}/{:02}/{:02}",
        t.year(),
        u8::from(t.month()),
        t.day()
    ));
    fs::create_dir_all(&dir).map_err(|e| ObservationError::Io(e.to_string()))?;
    let path = dir.join(format!("{}.jsonl", obs.repo.path_segment()));
    let line = serde_json::to_vec(obs).map_err(|e| ObservationError::Serde(e.to_string()))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ObservationError::Io(e.to_string()))?;
    file.write_all(&line)
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| ObservationError::Io(e.to_string()))?;
    file.sync_all()
        .map_err(|e| ObservationError::Io(e.to_string()))?;
    Ok(true)
}

/// Binding reconstructed from a sequence of observations of one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconstructedBinding {
    observation_count: u64,
    last_observed: Timestamp,
    target_sha: Sha40,
    commit_sha: Sha40,
    tree_sha: Sha40,
    ref_type: RefType,
}

impl ReconstructedBinding {
    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    pub fn last_observed(&self) -> Timestamp {
        self.last_observed
    }
}

/// Deterministically rebuild per-ref bindings. `NotModified` (304) counts toward
/// `observation_count` and extends `last_observed` for every ref currently known.
pub fn reconstruct_bindings(
    observations: &[Observation],
) -> BTreeMap<String, ReconstructedBinding> {
    let mut map: BTreeMap<String, ReconstructedBinding> = BTreeMap::new();
    let mut known: Vec<String> = Vec::new();

    let mut ordered: Vec<&Observation> = observations.iter().collect();
    ordered.sort_by_key(|o| o.observed_at);

    for obs in ordered {
        match &obs.outcome {
            Outcome::Ok { refs, .. } => {
                // One observation contributes at most once per ref name.
                let mut seen = std::collections::BTreeSet::new();
                known.clear();
                for r in refs {
                    if r.peeled_type != PeeledType::Commit {
                        continue;
                    }
                    let (Some(commit_sha), Some(tree_sha)) =
                        (r.commit_sha.clone(), r.tree_sha.clone())
                    else {
                        continue;
                    };
                    // tree == commit is not a git tree (pre-fix listing). Do not
                    // carry it forward as a reconstructed binding tree.
                    if tree_sha.as_str() == commit_sha.as_str() {
                        eprintln!(
                            "tree_sha equals commit_sha {} while reconstructing bindings; treating as unknown",
                            commit_sha.as_str()
                        );
                        continue;
                    }
                    if !seen.insert(r.name.clone()) {
                        continue;
                    }
                    known.push(r.name.clone());
                    let entry = map
                        .entry(r.name.clone())
                        .or_insert_with(|| ReconstructedBinding {
                            observation_count: 0,
                            last_observed: obs.observed_at,
                            target_sha: r.target_sha.clone(),
                            commit_sha: commit_sha.clone(),
                            tree_sha: tree_sha.clone(),
                            ref_type: r.ref_type,
                        });
                    entry.observation_count += 1;
                    entry.last_observed = obs.observed_at;
                    entry.target_sha = r.target_sha.clone();
                    entry.commit_sha = commit_sha;
                    entry.tree_sha = tree_sha;
                    entry.ref_type = r.ref_type;
                }
            }
            Outcome::NotModified { .. } => {
                for name in &known {
                    if let Some(entry) = map.get_mut(name) {
                        entry.observation_count += 1;
                        entry.last_observed = obs.observed_at;
                    }
                }
            }
            Outcome::Failed { .. } | Outcome::Skipped { .. } => {}
        }
    }
    map
}
