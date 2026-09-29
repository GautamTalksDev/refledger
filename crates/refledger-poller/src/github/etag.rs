//! A 304 exempts the primary rate limit only when the request carried a valid Authorization header.
//! The secondary limit is real, costs one point per 304, and is unobservable from response headers.
//!
//! The store is one fsynced JSONL journal, not sled or redb. Observations already persist that
//! way, so a crash has one durability story. We parse the journal ourselves and refuse to open it
//! when any record is bad. An embedded database that recreates an empty file after a checksum
//! failure would look like a healthy poller and spend a full primary budget. The keyspace is one
//! row per repository, route template, and query string (a page is a different query). Append,
//! fsync, and replay are enough: the latest record for a key wins.
//!
//! [`EndpointKey`] is built only by [`ConditionalRequest::endpoint`]. The store key and the
//! request target are that same value, so a caller cannot file an ETag under a key the wire
//! request did not use. There is no unauthenticated builder. [`ConditionalRequest::build`]
//! returns [`ETagError::NoAuthToken`] when the token is absent.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use refledger_log::normalize_to_utc_millis;

use crate::observation::{
    ETag, Observation, ObservationError, Outcome, RefreshReason, RepoSlug, SecondaryLimitEvent,
    Timestamp,
};

/// Route template plus the exact query string that will be sent.
///
/// Fields are private. The only constructor is [`ConditionalRequest::endpoint`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointKey {
    template: String,
    query: String,
}

impl EndpointKey {
    /// Path and query as they are sent. The store key is this pair, not a normalised form.
    pub fn request_target(&self) -> String {
        if self.query.is_empty() {
            self.template.clone()
        } else {
            format!("{}?{}", self.template, self.query)
        }
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn query(&self) -> &str {
        &self.query
    }
}

/// GitHub token. Empty and whitespace tokens are rejected so they cannot be
/// smuggled through as "present".
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    pub fn new(token: impl Into<String>) -> Result<Self, ETagError> {
        let token = token.into();
        if token.is_empty() || token.chars().any(char::is_whitespace) {
            return Err(ETagError::NoAuthToken);
        }
        Ok(Self(token))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken([redacted])")
    }
}

/// One conditional or forced-unconditional GET. Always carries Authorization.
///
/// Constructed only by [`ConditionalRequest::build`]. There is no
/// `build_unauthenticated`.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    repo: RepoSlug,
    endpoint: EndpointKey,
    authorization: String,
    if_none_match: Option<String>,
    refresh_reason: Option<RefreshReason>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("repo", &self.repo)
            .field("endpoint", &self.endpoint)
            .field("authorization", &"Bearer [redacted]")
            .field("if_none_match", &self.if_none_match)
            .field("refresh_reason", &self.refresh_reason)
            .finish()
    }
}

impl Request {
    pub fn http_method(&self) -> &'static str {
        "GET"
    }

    pub fn request_target(&self) -> String {
        self.endpoint.request_target()
    }

    pub fn authorization_header(&self) -> &str {
        &self.authorization
    }

    /// `If-None-Match` exactly as stored, including a `W/` weak-validator prefix.
    pub fn if_none_match_header(&self) -> Option<&str> {
        self.if_none_match.as_deref()
    }

    /// Parse `If-None-Match` back into an [`ETag`] without adding or stripping quotes.
    pub fn parsed_validator(&self) -> Option<ETag> {
        self.if_none_match
            .as_ref()
            .map(|raw| ETag::new(raw.clone()))
    }

    pub fn refresh_reason(&self) -> Option<RefreshReason> {
        self.refresh_reason
    }

    pub fn endpoint(&self) -> &EndpointKey {
        &self.endpoint
    }

    /// Record this request's method and, when the validator was expired, the refresh reason.
    pub fn to_observation(
        &self,
        observed_at: OffsetDateTime,
        outcome: Outcome,
        rate_limit_remaining: Option<u32>,
        secondary_limit_observed: Option<SecondaryLimitEvent>,
    ) -> Result<Observation, ObservationError> {
        use crate::observation::Method;

        let mut built = Observation::builder()
            .repo(self.repo.as_str())?
            .observed_at(observed_at)?
            .method(Method::Rest);
        if let Some(reason) = self.refresh_reason {
            built = built.refresh_reason(reason);
        }
        if let Some(remaining) = rate_limit_remaining {
            built = built.rate_limit_remaining(remaining);
        }
        if let Some(event) = secondary_limit_observed {
            built = built.secondary_limit_observed(event);
        }
        built.outcome(outcome).build()
    }
}

/// Builds [`EndpointKey`] values and conditional requests for one repository at one instant.
#[derive(Debug, Clone)]
pub struct ConditionalRequest {
    repo: RepoSlug,
    now: OffsetDateTime,
}

impl ConditionalRequest {
    pub fn at(repo: RepoSlug, now: OffsetDateTime) -> Self {
        Self { repo, now }
    }

    /// Template plus the full query string. This is the only way to obtain an [`EndpointKey`].
    pub fn endpoint(
        template: impl Into<String>,
        query: impl Into<String>,
    ) -> Result<EndpointKey, ETagError> {
        let template = template.into();
        let query = query.into();
        if template.is_empty()
            || template.contains('?')
            || template.chars().any(char::is_whitespace)
        {
            return Err(ETagError::InvalidEndpoint);
        }
        if query.chars().any(|c| c == '\n' || c == '\r') {
            return Err(ETagError::InvalidEndpoint);
        }
        Ok(EndpointKey { template, query })
    }

    /// Conditional GET when a fresh validator exists. Unconditional GET when it does not.
    ///
    /// `token == None` is [`ETagError::NoAuthToken`]. The successful [`Request`] always
    /// carries `Authorization`. A stored ETag older than the store's max age is discarded
    /// and the request records [`RefreshReason::EtagMaxAge`].
    pub fn build(
        &self,
        endpoint: &EndpointKey,
        token: Option<&AuthToken>,
        store: &ETagStore,
    ) -> Result<Request, ETagError> {
        let token = token.ok_or(ETagError::NoAuthToken)?;
        let (if_none_match, refresh_reason) = match store.lookup(&self.repo, endpoint, self.now) {
            Lookup::Fresh(etag) => (Some(etag.as_str().to_owned()), None),
            Lookup::Expired => (None, Some(RefreshReason::EtagMaxAge)),
            Lookup::Missing => (None, None),
        };
        Ok(Request {
            repo: self.repo.clone(),
            endpoint: endpoint.clone(),
            authorization: format!("Bearer {}", token.as_str()),
            if_none_match,
            refresh_reason,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    Fresh(ETag),
    Expired,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StoreKey {
    repo: String,
    template: String,
    query: String,
}

#[derive(Debug)]
struct StoredETag {
    etag: ETag,
    stored_at: OffsetDateTime,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalRecord {
    repo: RepoSlug,
    template: String,
    query: String,
    etag: ETag,
    stored_at: Timestamp,
}

/// Persisted `(repo, endpoint) -> validator` map.
///
/// `get` returns `None` for a missing key and for a validator older than `max_age`.
/// Pages are not a special case: `page=2` is a different query string from `page=1`.
#[derive(Debug)]
pub struct ETagStore {
    path: PathBuf,
    max_age: Duration,
    entries: BTreeMap<StoreKey, StoredETag>,
}

impl ETagStore {
    /// Open a journal. A missing file is an empty store. A file that cannot be
    /// replayed is [`ETagError::CorruptStore`] — it is not replaced with an empty map.
    pub fn open(path: impl AsRef<Path>, max_age: Duration) -> Result<Self, ETagError> {
        if max_age.is_negative() {
            return Err(ETagError::NegativeMaxAge);
        }
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| ETagError::Io(e.to_string()))?;
            }
        }
        let mut store = Self {
            path,
            max_age,
            entries: BTreeMap::new(),
        };
        store.load()?;
        Ok(store)
    }

    pub fn get(
        &self,
        repo: &RepoSlug,
        endpoint: &EndpointKey,
        now: OffsetDateTime,
    ) -> Option<ETag> {
        match self.lookup(repo, endpoint, now) {
            Lookup::Fresh(etag) => Some(etag),
            Lookup::Expired | Lookup::Missing => None,
        }
    }

    pub fn put(
        &mut self,
        repo: &RepoSlug,
        endpoint: &EndpointKey,
        etag: &ETag,
        now: OffsetDateTime,
    ) -> Result<(), ETagError> {
        let stored_at = stamp(now)?;
        let record = JournalRecord {
            repo: repo.clone(),
            template: endpoint.template.clone(),
            query: endpoint.query.clone(),
            etag: etag.clone(),
            stored_at,
        };
        self.append(&record)?;
        self.entries.insert(
            StoreKey {
                repo: repo.as_str().to_owned(),
                template: endpoint.template.clone(),
                query: endpoint.query.clone(),
            },
            StoredETag {
                etag: etag.clone(),
                stored_at: stored_at.as_offset_datetime(),
            },
        );
        Ok(())
    }

    /// `200` with an ETag replaces the stored validator. `304` does not, even when
    /// the 304 carries no `ETag` header and even when it carries a different one.
    pub fn apply_response(
        &mut self,
        repo: &RepoSlug,
        endpoint: &EndpointKey,
        status: u16,
        response_etag: Option<&ETag>,
        now: OffsetDateTime,
    ) -> Result<(), ETagError> {
        match status {
            304 => Ok(()),
            200 => {
                if let Some(etag) = response_etag {
                    self.put(repo, endpoint, etag, now)?;
                }
                Ok(())
            }
            other => Err(ETagError::UnexpectedStatus(other)),
        }
    }

    fn lookup(&self, repo: &RepoSlug, endpoint: &EndpointKey, now: OffsetDateTime) -> Lookup {
        let key = StoreKey {
            repo: repo.as_str().to_owned(),
            template: endpoint.template.clone(),
            query: endpoint.query.clone(),
        };
        match self.entries.get(&key) {
            None => Lookup::Missing,
            Some(stored) => {
                let age = now - stored.stored_at;
                if age > self.max_age {
                    Lookup::Expired
                } else {
                    Lookup::Fresh(stored.etag.clone())
                }
            }
        }
    }

    fn load(&mut self) -> Result<(), ETagError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) if err.kind() == ErrorKind::InvalidData => {
                return Err(ETagError::CorruptStore(err.to_string()));
            }
            Err(err) => return Err(ETagError::Io(err.to_string())),
        };
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: JournalRecord = serde_json::from_str(line)
                .map_err(|err| ETagError::CorruptStore(format!("line {}: {err}", idx + 1)))?;
            self.entries.insert(
                StoreKey {
                    repo: record.repo.as_str().to_owned(),
                    template: record.template,
                    query: record.query,
                },
                StoredETag {
                    etag: record.etag,
                    stored_at: record.stored_at.as_offset_datetime(),
                },
            );
        }
        Ok(())
    }

    fn append(&self, record: &JournalRecord) -> Result<(), ETagError> {
        let mut line =
            serde_json::to_vec(record).map_err(|err| ETagError::Serde(err.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|err| ETagError::Io(err.to_string()))?;
        file.write_all(&line)
            .map_err(|err| ETagError::Io(err.to_string()))?;
        file.sync_all()
            .map_err(|err| ETagError::Io(err.to_string()))?;
        Ok(())
    }
}

fn stamp(now: OffsetDateTime) -> Result<Timestamp, ETagError> {
    Timestamp::from_offset_datetime(normalize_to_utc_millis(now))
        .map_err(|err| ETagError::Timestamp(err.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ETagError {
    #[error(
        "conditional request requires Authorization; an unauthenticated conditional request is not representable"
    )]
    NoAuthToken,
    #[error("etag store is corrupt and was not opened: {0}")]
    CorruptStore(String),
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
    #[error("timestamp: {0}")]
    Timestamp(String),
    #[error("etag max age must not be negative")]
    NegativeMaxAge,
    #[error("endpoint template and query must describe a single request target")]
    InvalidEndpoint,
    #[error("status {0} does not update the etag store")]
    UnexpectedStatus(u16),
}
