//! Durable store for the log, observations, and daily signed heads.
//!
//! This module owns every path under the data directory. Callers hand it
//! values; they do not open those files.
//!
//! Day D's [`Event::ObservationDigest`] is always the first entry of day
//! D+1's log file, with `recorded_at` = D+1 00:00:00.000Z. Events from D+1
//! detected before that seal are buffered and appended after it. Nothing
//! here reads the wall clock — timestamps come from the caller, and the
//! digest timestamp is that fixed boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time};

use refledger_log::canonical_json;
use refledger_log::chain::{Chain, UnhashedEntry};
use refledger_log::entry::{Entry, Event};
use refledger_log::{
    key_id, normalize_to_utc_millis, public_key_pkix_pem, sign_ed25519ph, sign_head, Head,
    SigningKey,
};

use crate::archive::{
    format_archive_failure_note, ArchiveFailure, DayArchive, NoopArchive, ObservationArchive,
};
use crate::derive::{derive_observation_digest, ObservationDayStats};
use crate::identity::{user_agent, DEFAULT_LOG_ID};
use crate::observation::{Observation, ObservationError, Outcome, SkipReason, Timestamp};
use crate::population::PollGroup;
use crate::publish::{
    format_publish_failure_note, format_publish_success_note, is_deferred_publish,
    LedgerPublishPayload, LedgerPublisher, NoopPublisher, PublishFailure, PublishSuccess,
    DEFERRED_PUBLISH_MARKER,
};
use crate::scheduler::skip_observation;

const REKOR_KIND: &str = "hashedrekord";
const REKOR_VERSION: &str = "0.0.1";
const REKOR_PRODUCTION: &str = "https://rekor.sigstore.dev";
/// A head without a Rekor `log_index` older than this is a witness backlog.
/// Recorded in the next digest's note and fails `refledger-verify --strict`.
pub const WITNESS_BACKLOG_HOURS: i64 = 48;

/// Non-chain poller state lives under `state/`, never under `log/`.
/// `log/` holds only day files (`YYYY/MM/DD.jsonl`) and `heads.jsonl`.
const IDENTITY_WARNINGS_PATH: &str = "state/identity_warnings.jsonl";
const PUBLISH_FAILURES_PATH: &str = "state/publish_failures.jsonl";
const PUBLISH_SUCCESSES_PATH: &str = "state/publish_successes.jsonl";
const ARCHIVE_FAILURES_PATH: &str = "state/archive_upload_failures.jsonl";
/// Derived events held until prior days seal. Survives process exit.
const ENTRY_BUFFER_PATH: &str = "state/entry_buffer.jsonl";
const LEGACY_IDENTITY_WARNINGS: &str = "log/identity_warnings.jsonl";
const LEGACY_PUBLISH_FAILURES: &str = "log/publish_failures.jsonl";
const LEGACY_ARCHIVE_FAILURES: &str = "log/archive_upload_failures.jsonl";

/// True when the workflow asks `once` not to push `data/log` to main yet.
pub fn ledger_publish_deferred() -> bool {
    std::env::var("REFLEDGER_DEFER_LEDGER_PUBLISH")
        .ok()
        .as_deref()
        == Some("1")
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("data directory is already locked")]
    AlreadyLocked,
    #[error("log is corrupt and will not be appended to: {0}")]
    Corrupt(String),
    #[error("durability: {0}")]
    Durability(String),
    #[error("simulated crash before the append completed")]
    SimulatedCrash,
    #[error("dispatch for {0} is closed")]
    DispatchClosed(String),
    #[error("{0} in-flight requests have not drained")]
    InflightRemaining(u64),
    #[error("day {0} is already sealed")]
    AlreadySealed(String),
    #[error("day {0} is still unsealed")]
    UnsealedPrior(String),
    #[error("recorded_at went backwards: {0}")]
    NonMonotonic(String),
    #[error("unknown request ticket")]
    UnknownTicket,
    #[error("observation: {0}")]
    Observation(#[from] ObservationError),
    #[error("io: {0}")]
    Io(String),
    #[error("chain: {0}")]
    Chain(String),
    #[error("{0}")]
    Message(String),
}

/// What [`Store::append_entry`] did with a derived event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Appended {
    /// Boxed: `Entry` is large; keeping the idle `Buffered` variant small.
    Written(Box<Entry>),
    Buffered,
}

/// Proof that [`Store::begin_request`] accepted a dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTicket(u64);

/// Parameters for [`Store::open`]. No field is a wall clock read.
pub struct StoreOptions {
    pub log_id: String,
    pub signing_key: SigningKey,
    /// When a torn tail is moved aside, the sidecar name uses this timestamp.
    pub recovered_at: OffsetDateTime,
    pub rekor: Box<dyn RekorClient>,
    /// Optional extra mirror of sealed observation files. Defaults to [`NoopArchive`]
    /// (observations are published on the data branch).
    pub archive: Box<dyn ObservationArchive>,
    /// Dedicated publishing clone for `data/log/` on the public repo.
    /// Defaults to [`NoopPublisher`].
    pub publisher: Box<dyn LedgerPublisher>,
    /// When true, digests note that observation JSONL lives on the `data` branch.
    pub observations_on_data_branch: bool,
    /// When true, skip acquiring `.store.lock` (Actions concurrency is the lock).
    pub skip_lock: bool,
}

impl StoreOptions {
    /// Production defaults: `log_id = "refledger"`, no-op archive/publish until configured.
    pub fn new(signing_key: SigningKey, recovered_at: OffsetDateTime) -> Self {
        Self {
            log_id: DEFAULT_LOG_ID.to_owned(),
            signing_key,
            recovered_at: normalize_to_utc_millis(recovered_at),
            rekor: Box::new(HttpRekor::production()),
            archive: Box::new(NoopArchive),
            publisher: Box::new(NoopPublisher),
            observations_on_data_branch: false,
            skip_lock: false,
        }
    }
}

/// Rekor `POST /api/v1/log/entries` acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RekorAcceptance {
    pub log_index: u64,
    pub uuid: String,
    pub log_id: Option<String>,
    pub integrated_time: Option<u64>,
}

/// Witness client. A failure is recorded and retried; it is not fatal.
pub trait RekorClient: Send + Sync {
    fn submit(&self, proposed: &Value) -> Result<RekorAcceptance, String>;

    /// Look up an existing hashedrekord by artifact hash (`sha512:…`).
    /// Used when submit returns 409 (already in the log).
    fn lookup_by_hash(&self, artifact_hash: &str) -> Result<Option<RekorAcceptance>, String> {
        let _ = artifact_hash;
        Ok(None)
    }
}

/// Production client for `hashedrekord` 0.0.1.
///
/// Checked against Rekor's OpenAPI (`createLogEntry`, `POST /api/v1/log/entries`,
/// 201 body is a map of UUID → `{ logIndex, logID, integratedTime }`) and
/// `pkg/types/hashedrekord/v0.0.1`. That validator loads Ed25519 signatures
/// with `WithED25519ph`. `ed25519ph` only accepts a SHA-512 prehash
/// (`ComputeDigestForVerifying` rejects SHA-256 once `WithCryptoSignerOpts`
/// selects it). The artifact hash is therefore SHA-512 of the canonical head
/// bytes — the same bytes §4 of LOG-FORMAT signs with pure Ed25519. The
/// public key is PKIX PEM (`x509.ParsePKIXPublicKey`).
pub struct HttpRekor {
    base: String,
}

impl HttpRekor {
    pub fn production() -> Self {
        Self::new(REKOR_PRODUCTION)
    }

    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .build()
    }

    fn parse_entry_map(value: Value) -> Result<RekorAcceptance, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "rekor response is not an object".to_owned())?;
        let (uuid, entry) = obj
            .iter()
            .next()
            .ok_or_else(|| "rekor response contained no entry".to_owned())?;
        let log_index = entry
            .get("logIndex")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "rekor response missing logIndex".to_owned())?;
        Ok(RekorAcceptance {
            log_index,
            uuid: uuid.clone(),
            log_id: entry
                .get("logID")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            integrated_time: entry.get("integratedTime").and_then(|v| v.as_u64()),
        })
    }
}

impl RekorClient for HttpRekor {
    fn submit(&self, proposed: &Value) -> Result<RekorAcceptance, String> {
        let url = format!("{}/api/v1/log/entries", self.base);
        let bytes = serde_json::to_vec(proposed).map_err(|e| e.to_string())?;
        let agent = Self::agent();
        match agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set("User-Agent", &user_agent())
            .send_bytes(&bytes)
        {
            Ok(response) => {
                let text = response.into_string().map_err(|e| e.to_string())?;
                let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                Self::parse_entry_map(value)
            }
            Err(ureq::Error::Status(409, _response)) => {
                let hex = proposed
                    .pointer("/spec/data/hash/value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "rekor 409 but proposed body has no hash".to_owned())?;
                let artifact_hash = format!("sha512:{hex}");
                match self.lookup_by_hash(&artifact_hash)? {
                    Some(accepted) => Ok(accepted),
                    None => Err(format!(
                        "{url}: status code 409; lookup found no entry for {artifact_hash}"
                    )),
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn lookup_by_hash(&self, artifact_hash: &str) -> Result<Option<RekorAcceptance>, String> {
        let index_url = format!("{}/api/v1/index/retrieve", self.base);
        let body = serde_json::json!({ "hash": artifact_hash });
        let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let agent = Self::agent();
        let response = agent
            .post(&index_url)
            .set("Content-Type", "application/json")
            .set("User-Agent", &user_agent())
            .send_bytes(&bytes)
            .map_err(|e| e.to_string())?;
        let text = response.into_string().map_err(|e| e.to_string())?;
        let uuids: Vec<String> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let Some(uuid) = uuids.first() else {
            return Ok(None);
        };
        let entry_url = format!("{}/api/v1/log/entries/{uuid}", self.base);
        let entry_resp = agent
            .get(&entry_url)
            .set("User-Agent", &user_agent())
            .call()
            .map_err(|e| e.to_string())?;
        let entry_text = entry_resp.into_string().map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&entry_text).map_err(|e| e.to_string())?;
        // Confirm the stored hashedrekord is for this artifact hash.
        let entry = value
            .as_object()
            .and_then(|o| o.values().next())
            .ok_or_else(|| "rekor lookup entry missing body".to_owned())?;
        if let Some(b64) = entry.get("body").and_then(|v| v.as_str()) {
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) {
                if let Ok(inner) = serde_json::from_slice::<Value>(&raw) {
                    let stored = inner
                        .pointer("/spec/data/hash/value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let want = artifact_hash
                        .strip_prefix("sha512:")
                        .unwrap_or(artifact_hash);
                    if stored != want {
                        return Err(format!(
                            "rekor lookup hash mismatch: stored={stored} want={want}"
                        ));
                    }
                }
            }
        }
        Ok(Some(Self::parse_entry_map(value)?))
    }
}

/// In-memory stand-in that records a fixed log index. Tests use this so
/// replay never touches the network.
pub struct StaticRekor {
    pub log_index: u64,
}

impl RekorClient for StaticRekor {
    fn submit(&self, proposed: &Value) -> Result<RekorAcceptance, String> {
        let _ = proposed;
        Ok(RekorAcceptance {
            log_index: self.log_index,
            uuid: format!("test-{}", self.log_index),
            log_id: None,
            integrated_time: None,
        })
    }
}

/// Always fails. The poller records the error and keeps going.
pub struct FailingRekor {
    pub message: String,
}

impl RekorClient for FailingRekor {
    fn submit(&self, _proposed: &Value) -> Result<RekorAcceptance, String> {
        Err(self.message.clone())
    }
}

/// Filesystem the store writes through. Tests substitute [`FaultVolume`].
pub trait Volume: Send {
    fn lock(&mut self) -> Result<(), StoreError>;
    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>, StoreError>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
    fn append_record(&mut self, rel: &str, line: &[u8]) -> Result<(), StoreError>;
    fn write_exact(&mut self, rel: &str, bytes: &[u8]) -> Result<(), StoreError>;
    fn remove(&mut self, rel: &str) -> Result<(), StoreError>;
    fn unlock(&mut self);
}

pub struct OsVolume {
    root: PathBuf,
    lock: Option<File>,
}

impl OsVolume {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock: None,
        }
    }

    fn safe(&self, rel: &str) -> Result<PathBuf, StoreError> {
        if rel.is_empty() || Path::new(rel).is_absolute() {
            return Err(StoreError::Io(format!("refusing path {rel}")));
        }
        if Path::new(rel)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(StoreError::Io(format!("refusing path {rel}")));
        }
        Ok(self.root.join(rel))
    }

    fn fsync_dir(&self, dir: &Path) -> Result<(), StoreError> {
        let file = File::open(dir).map_err(|e| StoreError::Durability(format!("open dir: {e}")))?;
        file.sync_all()
            .map_err(|e| StoreError::Durability(format!("directory fsync: {e}")))
    }

    /// Release `.store.lock` with an explicit `LOCK_UN` before closing the fd.
    ///
    /// Under parallel `cargo test` threads, relying on close-alone to drop an
    /// `flock` was nondeterministic: a reopen of the same store root could see
    /// `AlreadyLocked` after the previous `Store` had been dropped. Explicit
    /// unlock makes release synchronous with `Drop`/`unlock`.
    fn release_lock(&mut self) {
        if let Some(file) = self.lock.take() {
            let fd = file.as_raw_fd();
            // SAFETY: `fd` is still open; LOCK_UN is best-effort before close.
            let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
            drop(file);
        }
    }
}

impl Volume for OsVolume {
    fn lock(&mut self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.root).map_err(|e| StoreError::Io(e.to_string()))?;
        let path = self.root.join(".store.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        // SAFETY: `file` is an open fd. LOCK_NB fails instead of blocking so
        // a second poller cannot append onto the same chain.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock
                || err.raw_os_error() == Some(libc::EAGAIN)
                || err.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                return Err(StoreError::AlreadyLocked);
            }
            return Err(StoreError::Io(err.to_string()));
        }
        self.lock = Some(file);
        Ok(())
    }

    fn unlock(&mut self) {
        self.release_lock();
    }

    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.safe(rel)?;
        if !path.exists() {
            return Ok(None);
        }
        fs::read(&path)
            .map(Some)
            .map_err(|e| StoreError::Io(e.to_string()))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let base = self.safe(prefix)?;
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        walk_files(&self.root, &base, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn append_record(&mut self, rel: &str, line: &[u8]) -> Result<(), StoreError> {
        let path = self.safe(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        file.write_all(line)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| StoreError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| StoreError::Durability(format!("file fsync: {e}")))?;
        if !existed {
            if let Some(parent) = path.parent() {
                if let Err(e) = self.fsync_dir(parent) {
                    let _ = fs::remove_file(&path);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn write_exact(&mut self, rel: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let path = self.safe(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        file.write_all(bytes)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| StoreError::Durability(format!("file fsync: {e}")))?;
        if !existed {
            if let Some(parent) = path.parent() {
                self.fsync_dir(parent)?;
            }
        }
        Ok(())
    }

    fn remove(&mut self, rel: &str) -> Result<(), StoreError> {
        let path = self.safe(rel)?;
        if path.exists() {
            fs::remove_file(&path).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        Ok(())
    }
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), StoreError> {
    for ent in fs::read_dir(dir).map_err(|e| StoreError::Io(e.to_string()))? {
        let ent = ent.map_err(|e| StoreError::Io(e.to_string()))?;
        let path = ent.path();
        if path.is_dir() {
            walk_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| StoreError::Io(e.to_string()))?;
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

impl Drop for OsVolume {
    fn drop(&mut self) {
        self.release_lock();
    }
}

/// Fault-injecting volume. Counts fsyncs and can fail a directory fsync or
/// kill an append mid-write (bytes kept, no newline, no fsync).
#[derive(Debug, Default)]
pub struct FaultVolume {
    files: std::collections::BTreeMap<String, Vec<u8>>,
    locked: bool,
    pub file_fsyncs: u32,
    pub dir_fsyncs: u32,
    fail_dir_fsync: bool,
    kill_next: bool,
}

impl FaultVolume {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next_dir_fsync(&mut self) {
        self.fail_dir_fsync = true;
    }

    pub fn kill_next_append(&mut self) {
        self.kill_next = true;
    }
}

impl Volume for FaultVolume {
    fn lock(&mut self) -> Result<(), StoreError> {
        if self.locked {
            return Err(StoreError::AlreadyLocked);
        }
        self.locked = true;
        Ok(())
    }

    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.files.get(rel).cloned())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let mut out: Vec<String> = self
            .files
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }

    fn append_record(&mut self, rel: &str, line: &[u8]) -> Result<(), StoreError> {
        if self.kill_next {
            self.kill_next = false;
            let mut partial = line.to_vec();
            partial.pop();
            self.files
                .entry(rel.to_owned())
                .or_default()
                .extend(partial);
            return Err(StoreError::SimulatedCrash);
        }
        let created = !self.files.contains_key(rel);
        {
            let buf = self.files.entry(rel.to_owned()).or_default();
            buf.extend_from_slice(line);
            buf.push(b'\n');
        }
        self.file_fsyncs += 1;
        if created {
            if self.fail_dir_fsync {
                self.fail_dir_fsync = false;
                if let Some(buf) = self.files.get_mut(rel) {
                    let keep = buf.len().saturating_sub(line.len() + 1);
                    buf.truncate(keep);
                    if buf.is_empty() {
                        self.files.remove(rel);
                    }
                }
                return Err(StoreError::Durability("directory fsync failed".to_owned()));
            }
            self.dir_fsyncs += 1;
        }
        Ok(())
    }

    fn write_exact(&mut self, rel: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let created = !self.files.contains_key(rel);
        self.files.insert(rel.to_owned(), bytes.to_vec());
        self.file_fsyncs += 1;
        if created {
            self.dir_fsyncs += 1;
        }
        Ok(())
    }

    fn remove(&mut self, rel: &str) -> Result<(), StoreError> {
        self.files.remove(rel);
        Ok(())
    }

    fn unlock(&mut self) {
        self.locked = false;
    }
}

pub struct Store<V: Volume> {
    vol: V,
    log_id: String,
    key: SigningKey,
    rekor: Box<dyn RekorClient>,
    archive: Box<dyn ObservationArchive>,
    publisher: Box<dyn LedgerPublisher>,
    /// Archive upload failures waiting for the next digest note.
    pending_archive_failures: Vec<ArchiveFailure>,
    /// Contact-URL / identity warnings waiting for the next digest note.
    pending_identity_warnings: Vec<String>,
    /// Ledger publish failures waiting for retry (and a digest note until then).
    pending_publish_failures: Vec<PublishFailure>,
    /// Successful publish retries waiting for the next digest note.
    pending_publish_successes: Vec<PublishSuccess>,
    chain: Chain,
    /// Days that have observations, non-digest log entries, or a closed dispatch.
    activity: BTreeSet<Date>,
    sealed: BTreeSet<Date>,
    dispatch_closed: BTreeSet<Date>,
    buffer: Vec<UnhashedEntry>,
    inflight: BTreeSet<u64>,
    next_ticket: u64,
    observations_on_data_branch: bool,
}

impl Store<OsVolume> {
    pub fn open(root: impl AsRef<Path>, opts: StoreOptions) -> Result<Self, StoreError> {
        Self::open_with(OsVolume::new(root.as_ref()), opts)
    }
}

impl<V: Volume> Store<V> {
    pub fn open_with(mut vol: V, opts: StoreOptions) -> Result<Self, StoreError> {
        if !opts.skip_lock {
            vol.lock()?;
        }
        migrate_log_sidecars(&mut vol)?;
        let (chain, activity, sealed) = load_chain(&mut vol, opts.recovered_at, &opts.log_id)?;
        let pending_archive_failures = load_archive_failures(&mut vol)?;
        let pending_identity_warnings = load_identity_warnings(&mut vol)?;
        let pending_publish_failures = load_publish_failures(&mut vol)?;
        let pending_publish_successes = load_publish_successes(&mut vol)?;
        let buffer = load_entry_buffer(&mut vol)?;
        Ok(Self {
            vol,
            log_id: opts.log_id,
            key: opts.signing_key,
            rekor: opts.rekor,
            archive: opts.archive,
            publisher: opts.publisher,
            pending_archive_failures,
            pending_identity_warnings,
            pending_publish_failures,
            pending_publish_successes,
            chain,
            activity,
            sealed,
            dispatch_closed: BTreeSet::new(),
            buffer,
            inflight: BTreeSet::new(),
            next_ticket: 1,
            observations_on_data_branch: opts.observations_on_data_branch,
        })
    }

    pub fn into_volume(mut self) -> V {
        self.vol.unlock();
        self.vol
    }

    pub fn set_rekor(&mut self, rekor: Box<dyn RekorClient>) {
        self.rekor = rekor;
    }

    pub fn set_archive(&mut self, archive: Box<dyn ObservationArchive>) {
        self.archive = archive;
    }

    pub fn set_publisher(&mut self, publisher: Box<dyn LedgerPublisher>) {
        self.publisher = publisher;
    }

    pub fn entries(&self) -> &[Entry] {
        self.chain.entries()
    }

    pub fn dispatch_open(&self, day: Date) -> bool {
        !self.dispatch_closed.contains(&day)
    }

    /// Stop dispatching new requests whose start stamp falls on `day`.
    /// In-flight requests may still complete.
    pub fn stop_dispatch(&mut self, day: Date) {
        self.dispatch_closed.insert(day);
        self.activity.insert(day);
    }

    pub fn begin_request(
        &mut self,
        started_at: OffsetDateTime,
    ) -> Result<RequestTicket, StoreError> {
        let day = started_at.date();
        if self.dispatch_closed.contains(&day) {
            return Err(StoreError::DispatchClosed(fmt_day(day)));
        }
        let id = self.next_ticket;
        self.next_ticket += 1;
        self.inflight.insert(id);
        Ok(RequestTicket(id))
    }

    /// `obs.observed_at` is the completion stamp. That date is the
    /// observation's day, not the day the request started.
    pub fn complete_observation(
        &mut self,
        ticket: RequestTicket,
        obs: &Observation,
    ) -> Result<(), StoreError> {
        if !self.inflight.remove(&ticket.0) {
            return Err(StoreError::UnknownTicket);
        }
        self.append_observation(obs)
    }

    pub fn append_observation(&mut self, obs: &Observation) -> Result<(), StoreError> {
        let day = obs.observed_at().as_offset_datetime().date();
        self.activity.insert(day);
        let rel = observation_rel(day, obs.repo().path_segment());
        let line = serde_json::to_vec(obs).map_err(|e| StoreError::Message(e.to_string()))?;
        self.vol.append_record(&rel, &line)
    }

    /// On open/restart: for each poll group whose latest `observed_at` is more
    /// than `2 × interval` before `now`, write one Skipped `PollerDown{from,to}`
    /// observation before the first new poll. Without this, a VM reboot during
    /// a contact-URL blip leaves a silent gap the exit condition cannot see.
    pub fn record_startup_downtime(
        &mut self,
        groups: &[PollGroup],
        interval: Duration,
        now: OffsetDateTime,
    ) -> Result<Vec<Observation>, StoreError> {
        let now = normalize_to_utc_millis(now);
        let threshold = interval * 2;
        let mut written = Vec::new();
        for g in groups {
            let Some(latest) = self.latest_observed_at(&g.repo)? else {
                continue;
            };
            if now - latest <= threshold {
                continue;
            }
            let from = Timestamp::from_offset_datetime(latest)?;
            let to = Timestamp::from_offset_datetime(now)?;
            let obs = skip_observation(&g.repo, now, SkipReason::PollerDown { from, to })?;
            self.append_observation(&obs)?;
            written.push(obs);
        }
        Ok(written)
    }

    /// Record schedule gaps for Actions `once` runs using [`SkipReason::SchedulerLag`].
    ///
    /// When the latest observation is older than `2 × interval`, one Skipped
    /// observation is written per poll group with the real scheduled vs actual times.
    pub fn record_schedule_gaps(
        &mut self,
        groups: &[PollGroup],
        interval: Duration,
        scheduled: OffsetDateTime,
        actual: OffsetDateTime,
    ) -> Result<Vec<Observation>, StoreError> {
        let threshold = interval * 2;
        let mut written = Vec::new();
        let scheduled = normalize_to_utc_millis(scheduled);
        let actual = normalize_to_utc_millis(actual);
        let scheduled_ts = Timestamp::from_offset_datetime(scheduled)?;
        let actual_ts = Timestamp::from_offset_datetime(actual)?;
        for g in groups {
            let Some(latest) = self.latest_observed_at(&g.repo)? else {
                continue;
            };
            if actual - latest <= threshold {
                continue;
            }
            let obs = skip_observation(
                &g.repo,
                actual,
                SkipReason::SchedulerLag {
                    scheduled: scheduled_ts,
                    actual: actual_ts,
                },
            )?;
            self.append_observation(&obs)?;
            written.push(obs);
        }
        Ok(written)
    }

    /// Seal every finished UTC day since the last observation, in order.
    /// Empty days receive a zero digest. Does not seal `now`'s calendar day.
    pub fn seal_missed_days_before(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<Vec<Date>, StoreError> {
        let today = now.date();
        // Walk every calendar day from the earliest activity day up to (but
        // not including) today. Empty intermediate days still get a digest
        // (LOG-FORMAT: quiet days are recorded, not omitted). Do not start
        // from latest_any_observed_at(): record_schedule_gaps may already
        // have written today's PollerDown rows, which would make
        // `latest == today` and skip sealing yesterday.
        let Some(mut day) = self.activity.iter().min().copied() else {
            return Ok(Vec::new());
        };
        let mut sealed_now = Vec::new();
        while day < today {
            if !self.sealed.contains(&day) {
                self.seal_day(day)?;
                sealed_now.push(day);
            }
            day = day
                .next_day()
                .ok_or_else(|| StoreError::Message(format!("no day after {day}")))?;
        }
        Ok(sealed_now)
    }

    /// Latest `observed_at` across all observation files, if any.
    pub fn latest_any_observed_at(&self) -> Result<Option<OffsetDateTime>, StoreError> {
        let mut latest: Option<OffsetDateTime> = None;
        for path in self.vol.list("observations")? {
            if !path.ends_with(".jsonl") || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                let at = obs.observed_at().as_offset_datetime();
                latest = Some(match latest {
                    Some(prev) if prev >= at => prev,
                    _ => at,
                });
            }
        }
        Ok(latest)
    }

    /// Tip sequence of the hash chain (0-based), or 0 when empty.
    pub fn tip_seq(&self) -> u64 {
        self.chain.entries().last().map(|e| e.seq).unwrap_or(0)
    }

    /// Record a post-genesis contact-URL warning for the next digest note.
    pub fn record_identity_warning(&mut self, warning: &str) -> Result<(), StoreError> {
        let line = serde_json::json!({ "warning": warning });
        let bytes = serde_json::to_vec(&line).map_err(|e| StoreError::Message(e.to_string()))?;
        self.vol.append_record(IDENTITY_WARNINGS_PATH, &bytes)?;
        self.pending_identity_warnings.push(warning.to_owned());
        Ok(())
    }

    /// Target bindings (`name:sha`) from the latest Ok observation for `repo`.
    pub fn latest_ok_targets(&self, repo: &str) -> Result<BTreeSet<String>, StoreError> {
        let segment = crate::observation::RepoSlug::parse(repo)
            .map_err(StoreError::Observation)?
            .path_segment();
        let suffix = format!("/{segment}.jsonl");
        let mut best: Option<(OffsetDateTime, BTreeSet<String>)> = None;
        for path in self.vol.list("observations")? {
            if !path.ends_with(&suffix) || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                let Outcome::Ok { refs, .. } = obs.outcome() else {
                    continue;
                };
                let at = obs.observed_at().as_offset_datetime();
                if best.as_ref().is_some_and(|(prev, _)| *prev >= at) {
                    continue;
                }
                let mut targets = BTreeSet::new();
                for r in refs {
                    targets.insert(format!("{}:{}", r.name(), r.target_sha()));
                }
                best = Some((at, targets));
            }
        }
        Ok(best.map(|(_, t)| t).unwrap_or_default())
    }

    /// `observed_at` of the latest Ok observation for `repo`, if any.
    pub fn latest_ok_observed_at(&self, repo: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        let segment = crate::observation::RepoSlug::parse(repo)
            .map_err(StoreError::Observation)?
            .path_segment();
        let suffix = format!("/{segment}.jsonl");
        let mut best: Option<OffsetDateTime> = None;
        for path in self.vol.list("observations")? {
            if !path.ends_with(&suffix) || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                if !matches!(obs.outcome(), Outcome::Ok { .. }) {
                    continue;
                }
                let at = obs.observed_at().as_offset_datetime();
                if best.map(|prev| at > prev).unwrap_or(true) {
                    best = Some(at);
                }
            }
        }
        Ok(best)
    }

    /// Latest `observed_at` for `repo` across all observation files, if any.
    pub fn latest_observed_at(&self, repo: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        let segment = crate::observation::RepoSlug::parse(repo)
            .map_err(StoreError::Observation)?
            .path_segment();
        let suffix = format!("/{segment}.jsonl");
        let mut latest: Option<OffsetDateTime> = None;
        for path in self.vol.list("observations")? {
            if !path.ends_with(&suffix) || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                let at = obs.observed_at().as_offset_datetime();
                latest = Some(match latest {
                    Some(prev) if prev >= at => prev,
                    _ => at,
                });
            }
        }
        Ok(latest)
    }

    pub fn append_entry(&mut self, entry: UnhashedEntry) -> Result<Appended, StoreError> {
        let day = entry.recorded_at.date();
        if self
            .activity
            .iter()
            .any(|d| *d < day && !self.sealed.contains(d))
        {
            self.buffer.push(entry);
            self.persist_entry_buffer()?;
            return Ok(Appended::Buffered);
        }
        self.commit(entry).map(|e| Appended::Written(Box::new(e)))
    }

    /// Keys that already have a `PopulationChange::Added` entry in the chain.
    pub fn population_added_keys(&self) -> BTreeSet<crate::population::WatchedKey> {
        use crate::population::WatchedKey;
        use refledger_log::entry::PopulationChangeKind;
        let mut out = BTreeSet::new();
        for e in self.chain.entries() {
            if e.event != Event::PopulationChange {
                continue;
            }
            let Some(pc) = &e.population_change else {
                continue;
            };
            if pc.change != PopulationChangeKind::Added {
                continue;
            }
            let Some(repo) = &e.repo else {
                continue;
            };
            out.insert(WatchedKey::new(repo.clone(), pc.path.clone()));
        }
        out
    }

    /// Earliest observation per `(repo, path)` across all observation files.
    pub fn earliest_observations(
        &self,
    ) -> Result<
        BTreeMap<crate::population::WatchedKey, crate::population::EarliestObservation>,
        StoreError,
    > {
        use crate::population::{EarliestObservation, WatchedKey};
        let mut best: BTreeMap<WatchedKey, EarliestObservation> = BTreeMap::new();
        for path in self.vol.list("observations")? {
            if !path.ends_with(".jsonl") || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                let key = WatchedKey::new(
                    obs.repo().as_str().to_owned(),
                    obs.action_path().map(|s| s.to_owned()),
                );
                let at = obs.observed_at().as_offset_datetime();
                let id = obs.observation_id().to_string();
                match best.get(&key) {
                    Some(prev) if prev.observed_at <= at => {}
                    _ => {
                        best.insert(
                            key,
                            EarliestObservation {
                                observed_at: at,
                                observation_id: id,
                            },
                        );
                    }
                }
            }
        }
        Ok(best)
    }

    /// Emit missing genesis `PopulationChange::Added` rows for watched keys.
    ///
    /// Entries are written before any other new log activity for this open, in
    /// `(recorded_at, repo, path)` order. Idempotent under replay.
    ///
    /// `run_now` is used as `recorded_at` for late registrations (keys missed
    /// by an earlier per-group derivation bug) so the chain stays monotonic.
    pub fn emit_genesis_population_adds(
        &mut self,
        watched: &[crate::population::WatchedEntry],
        run_now: OffsetDateTime,
    ) -> Result<usize, StoreError> {
        use crate::population::genesis_added_entries;
        let earliest = self.earliest_observations()?;
        let already = self.population_added_keys();
        let entries = genesis_added_entries(watched, &earliest, &already, run_now);
        let n = entries.len();
        for entry in entries {
            self.append_entry(entry)?;
        }
        Ok(n)
    }

    /// Queue a digest note about the fabricated 422 on run #2 before day
    /// 2026-09-29 seals. Idempotent: skips if the note is already pending or
    /// already present in a sealed ObservationDigest.
    pub fn ensure_false_422_digest_note(&mut self) -> Result<(), StoreError> {
        const NOTE: &str = "observation 01M3Q896RH64XBABMKK8AXKNJ1 (tj-actions/changed-files at 2026-09-29T18:53:44.590Z): recorded http_status 422 was fabricated by a poller bug (budget exhaustion misreported as network/422); fixed in e30f6c7";
        if self
            .pending_identity_warnings
            .iter()
            .any(|w| w.contains("01M3Q896RH64XBABMKK8AXKNJ1"))
        {
            return Ok(());
        }
        for entry in self.chain.entries() {
            if entry.event != Event::ObservationDigest {
                continue;
            }
            if let Some(d) = &entry.observation_digest {
                if d.note
                    .as_deref()
                    .is_some_and(|n| n.contains("01M3Q896RH64XBABMKK8AXKNJ1"))
                {
                    return Ok(());
                }
            }
        }
        self.record_identity_warning(NOTE)
    }

    /// Named gap: from genesis until the derive-wiring fix, `once` stored
    /// observations but appended no Move/Deletion/Recreation. Archive replay
    /// found 0 ecosystem tag moves in that window; canary patterns 3 and 4
    /// were not observable. Idempotent.
    pub fn ensure_gap_no_derive_digest_note(&mut self) -> Result<(), StoreError> {
        const MARKER: &str = "gap-no-derive-2026-09-29";
        // Placeholder filled at commit time via docs; the note text is stable.
        const NOTE: &str = "gap-no-derive-2026-09-29: from genesis until fix commit 07c049c the runner stored observations but appended no Move, Deletion or Recreation entries; replay of the archive found 0 ecosystem tag moves in that window; canary patterns 3 and 4 were not observable";
        self.ensure_digest_note_once(MARKER, NOTE)
    }

    /// Append a Correction for seq 40 (Deletion mis-labelled content_change).
    /// Idempotent: skips if a Correction with corrects_seq=40 already exists.
    pub fn ensure_seq_40_deletion_classification_correction(
        &mut self,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        const SEQ: u64 = 40;
        const REASON: &str = "seq 40 Deletion carried classification content_change, which is meaningless for a deletion (no content to compare); LOG-FORMAT v1 requires a classification field on deletions and has no deletion-specific value — content_change was the vector convention, not a computed tree diff";
        for entry in self.chain.entries() {
            if entry.event == Event::Correction && entry.corrects_seq == Some(SEQ) {
                return Ok(());
            }
        }
        // Only correct if seq 40 exists and is a Deletion.
        let Some(target) = self.chain.entries().iter().find(|e| e.seq == SEQ) else {
            return Ok(());
        };
        if target.event != Event::Deletion {
            return Ok(());
        }
        self.append_entry(UnhashedEntry::correction(recorded_at, SEQ, REASON))?;
        Ok(())
    }

    /// 32 listing observations recorded incorrect tree_sha values (commit SHA
    /// or annotated placeholders). Listed in docs/tree-sha-affected-observations.txt;
    /// observations are unchanged; those tree_sha fields must not be relied on.
    /// Fixed in 07c049c. Idempotent.
    pub fn ensure_tree_sha_digest_note(&mut self) -> Result<(), StoreError> {
        const MARKER: &str = "tree-sha-affected-observations";
        const NOTE: &str = "docs/tree-sha-affected-observations.txt: 32 observations recorded incorrect tree_sha values due to a listing bug fixed in 07c049c; the observations are unchanged; their tree_sha fields should not be relied on";
        self.ensure_digest_note_once(MARKER, NOTE)
    }

    /// Same Sep 29 listing path also invented `commit_sha=000…001` /
    /// `tree_sha=000…002` for unpeeled annotated tags. Those are not git
    /// objects. Covered by docs/tree-sha-affected-observations.txt (placeholder
    /// column); queued separately so the note names commit invent explicitly.
    /// Idempotent.
    pub fn ensure_invented_placeholder_digest_note(&mut self) -> Result<(), StoreError> {
        const MARKER: &str = "invented-placeholder-shas";
        const NOTE: &str = "invented-placeholder-shas: Sep 29 listing observations also carry invented commit_sha=000…001 and tree_sha=000…002 for unpeeled annotated tags (see docs/tree-sha-affected-observations.txt placeholder column); those values are not git objects and must not be relied on";
        self.ensure_digest_note_once(MARKER, NOTE)
    }

    /// Disclose the 2026-10-01 main heads.jsonl line rewrite. Idempotent.
    pub fn ensure_heads_line_rewrite_digest_note(&mut self) -> Result<(), StoreError> {
        const MARKER: &str = "heads-line-rewrite-2026-10-01";
        const NOTE: &str = "heads-line-rewrite-2026-10-01: on main, commit ff75e915 replaced the seq 42 line written in 0800c1f5 (which had log_index 3027764712) with a 409 error line; the head and signature bytes were identical in both; the Rekor entry was never lost; commit 93211084 re-appended the index; both versions remain in git history; publishing is now prefix-checked";
        self.ensure_digest_note_once(MARKER, NOTE)
    }

    fn ensure_digest_note_once(&mut self, marker: &str, note: &str) -> Result<(), StoreError> {
        if self
            .pending_identity_warnings
            .iter()
            .any(|w| w.contains(marker))
        {
            return Ok(());
        }
        for entry in self.chain.entries() {
            if entry.event != Event::ObservationDigest {
                continue;
            }
            if let Some(d) = &entry.observation_digest {
                if d.note.as_deref().is_some_and(|n| n.contains(marker)) {
                    return Ok(());
                }
            }
        }
        self.record_identity_warning(note)
    }

    /// Ok observations for `repo`, oldest first.
    pub fn ok_observations_chronological(
        &self,
        repo: &str,
    ) -> Result<Vec<Observation>, StoreError> {
        let segment = crate::observation::RepoSlug::parse(repo)
            .map_err(StoreError::Observation)?
            .path_segment();
        let suffix = format!("/{segment}.jsonl");
        let mut out = Vec::new();
        for path in self.vol.list("observations")? {
            if !path.ends_with(&suffix) || path.contains(".torn.") {
                continue;
            }
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                if matches!(obs.outcome(), Outcome::Ok { .. }) {
                    out.push(obs);
                }
            }
        }
        out.sort_by_key(|o| o.observed_at().as_offset_datetime());
        Ok(out)
    }

    /// Seal day D. The digest is the first entry of day D+1, then the buffer drains.
    ///
    /// After the digest is committed, an optional extra observation mirror may
    /// run, then the sealed `log/` (and observations when configured) is copied
    /// into a dedicated publishing clone and fast-forward pushed onto the data
    /// branch. Mirror or publish failures are queued and appear in the *next*
    /// ObservationDigest note — never fatal to the seal.
    pub fn seal_day(&mut self, day: Date) -> Result<Entry, StoreError> {
        let label = fmt_day(day);
        if self.sealed.contains(&day) {
            return Err(StoreError::AlreadySealed(label));
        }
        if !self.inflight.is_empty() {
            return Err(StoreError::InflightRemaining(self.inflight.len() as u64));
        }
        if let Some(prior) = self
            .activity
            .iter()
            .copied()
            .find(|d| *d < day && !self.sealed.contains(d))
        {
            return Err(StoreError::UnsealedPrior(fmt_day(prior)));
        }
        // Retry any prior publish failure before sealing so today's digest can
        // record recovery instead of waiting another 24 hours.
        self.retry_pending_publishes()?;
        let next = day
            .next_day()
            .ok_or_else(|| StoreError::Message(format!("no day after {label}")))?;
        let as_of = start_of(next);
        let stats = self.day_stats(day, as_of)?;
        let day_files = stats.files.clone();
        let unhashed = derive_observation_digest(as_of, stats)
            .map_err(|e| StoreError::Message(e.to_string()))?;
        let entry = self.commit(unhashed)?;
        self.sealed.insert(day);
        self.flush_buffer()?;
        self.publish_head(&entry)?;
        // Clear pending notes now that today's digest has absorbed them.
        self.clear_consumed_archive_failures()?;
        self.clear_consumed_identity_warnings()?;
        self.clear_consumed_publish_successes()?;
        // Optional extra observation mirror. Failure is non-fatal to the seal
        // but must surface in the next digest's note.
        self.upload_sealed_day(day, &day_files)?;
        // Copy sealed log bytes into the publishing clone and FF-push. Never
        // force. Failure is non-fatal and retried on every subsequent poll.
        self.publish_sealed_log(day, entry.seq)?;
        Ok(entry)
    }

    /// If `state/publish_failures.jsonl` has pending entries, retry the FF
    /// publish once (never force). On success, clear the pending file and
    /// queue a success note for the next ObservationDigest.
    pub fn retry_pending_publishes(&mut self) -> Result<bool, StoreError> {
        if self.pending_publish_failures.is_empty() {
            return Ok(false);
        }
        // During `once`, main publish is deferred until after the data commit.
        if ledger_publish_deferred() {
            return Ok(false);
        }
        let first = self.pending_publish_failures[0].clone();
        let day = parse_day_label(&first.day)?;
        let seq = first
            .seq
            .or_else(|| self.digest_seq_for_sealed_day(&first.day))
            .unwrap_or_else(|| self.tip_seq());
        let files = self.log_files_for_publish()?;
        let payload = LedgerPublishPayload { day, seq, files };
        match self.publisher.publish_seal(&payload) {
            Ok(()) => {
                if self
                    .pending_publish_failures
                    .iter()
                    .all(is_deferred_publish)
                {
                    // Deferred queue clearing is silent — not a failure recovery.
                    self.pending_publish_failures.clear();
                    self.vol.write_exact(PUBLISH_FAILURES_PATH, b"")?;
                } else {
                    self.finish_publish_recovery()?;
                }
                Ok(true)
            }
            Err(err) => {
                // Replace the queue with the freshest error; keep retrying next poll.
                self.pending_publish_failures.clear();
                self.vol.write_exact(PUBLISH_FAILURES_PATH, b"")?;
                self.record_publish_failure(&PublishFailure {
                    day: first.day,
                    seq: Some(seq),
                    error: err,
                })?;
                Ok(false)
            }
        }
    }

    fn publish_sealed_log(&mut self, day: Date, seq: u64) -> Result<(), StoreError> {
        if ledger_publish_deferred() {
            let day_label = fmt_day(day);
            if !self
                .pending_publish_failures
                .iter()
                .any(|f| f.day == day_label)
            {
                self.record_publish_failure(&PublishFailure {
                    day: day_label,
                    seq: Some(seq),
                    error: DEFERRED_PUBLISH_MARKER.to_owned(),
                })?;
            }
            return Ok(());
        }
        let had_pending = !self.pending_publish_failures.is_empty();
        let files = self.log_files_for_publish()?;
        let payload = LedgerPublishPayload { day, seq, files };
        match self.publisher.publish_seal(&payload) {
            Ok(()) => {
                if had_pending {
                    self.finish_publish_recovery()?;
                }
            }
            Err(err) => {
                let failure = PublishFailure {
                    day: fmt_day(day),
                    seq: Some(seq),
                    error: err,
                };
                self.record_publish_failure(&failure)?;
            }
        }
        Ok(())
    }

    fn finish_publish_recovery(&mut self) -> Result<(), StoreError> {
        let recovered: Vec<PublishSuccess> = self
            .pending_publish_failures
            .iter()
            .map(|f| {
                let seq = f.seq.or_else(|| self.digest_seq_for_sealed_day(&f.day));
                PublishSuccess {
                    day: f.day.clone(),
                    seq,
                }
            })
            .collect();
        self.pending_publish_failures.clear();
        self.vol.write_exact(PUBLISH_FAILURES_PATH, b"")?;
        for success in &recovered {
            self.record_publish_success(success)?;
        }
        Ok(())
    }

    fn log_files_for_publish(&self) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let mut out = Vec::new();
        for rel in self.vol.list("log")? {
            let publishable = rel.ends_with("heads.jsonl") || is_day_log(&rel);
            if !publishable || rel.contains(".torn.") {
                continue;
            }
            if let Some(bytes) = self.vol.read(&rel)? {
                out.push((rel, bytes));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn record_publish_failure(&mut self, failure: &PublishFailure) -> Result<(), StoreError> {
        let mut line = serde_json::json!({
            "day": failure.day,
            "error": failure.error,
        });
        if let Some(seq) = failure.seq {
            line.as_object_mut()
                .expect("json object")
                .insert("seq".to_owned(), serde_json::json!(seq));
        }
        let bytes = serde_json::to_vec(&line).map_err(|e| StoreError::Message(e.to_string()))?;
        self.vol.append_record(PUBLISH_FAILURES_PATH, &bytes)?;
        self.pending_publish_failures.push(failure.clone());
        Ok(())
    }

    fn record_publish_success(&mut self, success: &PublishSuccess) -> Result<(), StoreError> {
        let mut line = serde_json::json!({
            "day": success.day,
        });
        if let Some(seq) = success.seq {
            line.as_object_mut()
                .expect("json object")
                .insert("seq".to_owned(), serde_json::json!(seq));
        }
        let bytes = serde_json::to_vec(&line).map_err(|e| StoreError::Message(e.to_string()))?;
        self.vol.append_record(PUBLISH_SUCCESSES_PATH, &bytes)?;
        self.pending_publish_successes.push(success.clone());
        Ok(())
    }

    fn clear_consumed_publish_successes(&mut self) -> Result<(), StoreError> {
        if self.pending_publish_successes.is_empty() {
            return Ok(());
        }
        self.pending_publish_successes.clear();
        self.vol.write_exact(PUBLISH_SUCCESSES_PATH, b"")?;
        Ok(())
    }

    /// Seq of the ObservationDigest that sealed `day` (`YYYY-MM-DD`), if present.
    fn digest_seq_for_sealed_day(&self, day: &str) -> Option<u64> {
        self.chain.entries().iter().rev().find_map(|e| {
            let d = e.observation_digest.as_ref()?;
            if d.date == day {
                Some(e.seq)
            } else {
                None
            }
        })
    }

    /// Resubmit heads whose latest line has no Rekor `log_index`.
    /// Disk failure is returned. A rejected submission is appended and is not an error.
    pub fn retry_witnesses(&mut self) -> Result<u32, StoreError> {
        let lines = self.head_lines()?;
        let mut latest: std::collections::BTreeMap<u64, &Value> = std::collections::BTreeMap::new();
        for line in &lines {
            if let Some(seq) = line
                .get("head")
                .and_then(|h| h.get("seq"))
                .and_then(|v| v.as_u64())
            {
                latest.insert(seq, line);
            }
        }
        let mut appended = 0u32;
        let pending: Vec<Value> = latest
            .into_values()
            .filter(|v| {
                v.get("rekor")
                    .and_then(|r| r.get("log_index"))
                    .and_then(|i| i.as_u64())
                    .is_none()
            })
            .cloned()
            .collect();
        for line in pending {
            let head: Head =
                serde_json::from_value(line.get("head").cloned().unwrap_or(Value::Null))
                    .map_err(|e| StoreError::Message(e.to_string()))?;
            let canonical = canonical_json(
                &serde_json::to_value(&head).map_err(|e| StoreError::Message(e.to_string()))?,
            )
            .map_err(|e| StoreError::Message(e.to_string()))?;
            let (body, artifact_hash) = hashedrekord_body(&canonical, &self.key)?;
            let result = witness_submit(self.rekor.as_ref(), &body, &artifact_hash);
            let attempts = line
                .get("rekor")
                .and_then(|r| r.get("attempts"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                + 1;
            let record = head_record_from_existing(&line, &artifact_hash, &result, attempts)?;
            self.vol.append_record("log/heads.jsonl", &record)?;
            appended += 1;
        }
        Ok(appended)
    }

    /// Fast-forward push the current sealed `log/` (including latest heads) to
    /// main. Used by `publish-pending` so a witness backfill that did not
    /// accompany a new seal still reaches the public branch.
    pub fn republish_sealed_tip(&mut self) -> Result<bool, StoreError> {
        let Some(entry) = self
            .chain
            .entries()
            .iter()
            .rev()
            .find(|e| e.event == Event::ObservationDigest)
        else {
            return Ok(false);
        };
        let Some(digest) = &entry.observation_digest else {
            return Ok(false);
        };
        let day = parse_day_label(&digest.date)?;
        let seq = entry.seq;
        let before = self.pending_publish_failures.len();
        self.publish_sealed_log(day, seq)?;
        let after = self.pending_publish_failures.len();
        // True when we did not newly fail (pending count did not grow).
        Ok(after <= before)
    }

    pub fn chain_bytes(&self) -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::new();
        for rel in self.day_logs()? {
            if let Some(bytes) = self.vol.read(&rel)? {
                out.extend(bytes);
            }
        }
        Ok(out)
    }

    pub fn read_rel(&self, rel: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.vol.read(rel)
    }

    pub fn day_log(&self, day: Date) -> Result<Option<Vec<u8>>, StoreError> {
        self.vol.read(&log_rel(day))
    }

    pub fn torn_files(&self) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let mut out = Vec::new();
        for rel in self.vol.list("log")? {
            if rel.contains(".torn.") {
                if let Some(bytes) = self.vol.read(&rel)? {
                    out.push((rel, bytes));
                }
            }
        }
        Ok(out)
    }

    fn flush_buffer(&mut self) -> Result<(), StoreError> {
        let pending = std::mem::take(&mut self.buffer);
        // Clear durable copy first so a crash mid-flush does not double-apply
        // after a successful commit of some entries (those are already chained).
        self.vol.write_exact(ENTRY_BUFFER_PATH, b"")?;
        for entry in pending {
            let _ = self.append_entry(entry)?;
        }
        self.persist_entry_buffer()?;
        Ok(())
    }

    fn persist_entry_buffer(&mut self) -> Result<(), StoreError> {
        let mut out = Vec::new();
        for entry in &self.buffer {
            let line = serde_json::to_vec(entry).map_err(|e| StoreError::Message(e.to_string()))?;
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        self.vol.write_exact(ENTRY_BUFFER_PATH, &out)
    }

    /// Observation ids already cited by Move/Deletion/Recreation on the chain.
    pub fn sourced_observation_ids(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for e in self.chain.entries() {
            if !matches!(e.event, Event::Move | Event::Deletion | Event::Recreation) {
                continue;
            }
            if let Some(ids) = &e.source_observations {
                out.extend(ids.iter().cloned());
            }
        }
        out
    }

    /// `recorded_at` of the chain tip, if any.
    pub fn tip_recorded_at(&self) -> Option<OffsetDateTime> {
        self.chain
            .entries()
            .last()
            .map(|e| e.recorded_at.as_offset_datetime())
    }

    /// How many derived entries are waiting for a prior-day seal.
    pub fn buffered_entry_count(&self) -> usize {
        self.buffer.len()
    }

    fn commit(&mut self, unhashed: UnhashedEntry) -> Result<Entry, StoreError> {
        if let Some(prev) = self.chain.entries().last() {
            let prev_at = prev.recorded_at.as_offset_datetime();
            if unhashed.recorded_at < prev_at {
                return Err(StoreError::NonMonotonic(format!(
                    "{prev_at} then {}",
                    unhashed.recorded_at
                )));
            }
        }
        let is_digest = unhashed.event == Event::ObservationDigest;
        let day = unhashed.recorded_at.date();
        let rel = log_rel(day);
        let existing = self.vol.read(&rel)?.unwrap_or_default();
        if existing.is_empty() && !is_digest {
            let prior =
                self.activity.iter().any(|d| *d < day) || self.sealed.iter().any(|d| *d < day);
            if prior {
                return Err(StoreError::Message(format!(
                    "{rel} must start with the previous day's observation digest"
                )));
            }
        }
        let appended = self
            .chain
            .append(unhashed)
            .map_err(|e| StoreError::Chain(e.to_string()))?
            .clone();
        let value =
            serde_json::to_value(&appended).map_err(|e| StoreError::Message(e.to_string()))?;
        let line = canonical_json(&value).map_err(|e| StoreError::Message(e.to_string()))?;
        if let Err(err) = self.vol.append_record(&rel, &line) {
            self.chain.rollback_last();
            return Err(err);
        }
        if !is_digest {
            self.activity.insert(day);
        }
        Ok(appended)
    }

    fn upload_sealed_day(
        &mut self,
        day: Date,
        files: &[(String, String)],
    ) -> Result<(), StoreError> {
        let mut payloads = Vec::new();
        for (rel, _sha) in files {
            let bytes = self.vol.read(rel)?.unwrap_or_default();
            payloads.push((rel.clone(), bytes));
        }
        let archive = DayArchive {
            day,
            files: payloads,
        };
        if let Err(err) = self.archive.upload_day(&archive) {
            let failure = ArchiveFailure {
                day: fmt_day(day),
                error: err,
            };
            self.record_archive_failure(&failure)?;
        }
        Ok(())
    }

    fn record_archive_failure(&mut self, failure: &ArchiveFailure) -> Result<(), StoreError> {
        let line = serde_json::json!({
            "day": failure.day,
            "error": failure.error,
        });
        let bytes = serde_json::to_vec(&line).map_err(|e| StoreError::Message(e.to_string()))?;
        self.vol.append_record(ARCHIVE_FAILURES_PATH, &bytes)?;
        self.pending_archive_failures.push(failure.clone());
        Ok(())
    }

    fn clear_consumed_archive_failures(&mut self) -> Result<(), StoreError> {
        // Failures present at the start of this seal were written into today's
        // digest note. Drop them from the pending file so they are not repeated.
        if self.pending_archive_failures.is_empty() {
            return Ok(());
        }
        // Anything recorded during upload_sealed_day this call must remain.
        // clear_consumed runs *before* upload, so the whole vec is consumed.
        self.pending_archive_failures.clear();
        self.vol.write_exact(ARCHIVE_FAILURES_PATH, b"")?;
        Ok(())
    }

    fn clear_consumed_identity_warnings(&mut self) -> Result<(), StoreError> {
        if self.pending_identity_warnings.is_empty() {
            return Ok(());
        }
        self.pending_identity_warnings.clear();
        self.vol.write_exact(IDENTITY_WARNINGS_PATH, b"")?;
        Ok(())
    }

    fn day_stats(
        &self,
        day: Date,
        as_of: OffsetDateTime,
    ) -> Result<ObservationDayStats, StoreError> {
        let prefix = format!(
            "observations/{:04}/{:02}/{:02}/",
            day.year(),
            u8::from(day.month()),
            day.day()
        );
        let mut files = self.vol.list("observations")?;
        files.retain(|p| p.starts_with(&prefix) && p.ends_with(".jsonl"));
        files.sort();
        let mut repos = BTreeSet::new();
        let mut ok = 0u64;
        let mut not_modified = 0u64;
        let mut failed = 0u64;
        let mut skipped = 0u64;
        let mut digests = Vec::new();
        for path in files {
            let bytes = self.vol.read(&path)?.unwrap_or_default();
            let sha = hex::encode(Sha256::digest(&bytes));
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let obs: Observation = serde_json::from_slice(line)
                    .map_err(|e| StoreError::Corrupt(format!("{path}: {e}")))?;
                repos.insert(obs.repo().as_str().to_owned());
                match obs.outcome() {
                    Outcome::Ok { .. } => ok += 1,
                    Outcome::NotModified { .. } => not_modified += 1,
                    Outcome::Failed { .. } => failed += 1,
                    Outcome::Skipped { .. } => skipped += 1,
                }
            }
            digests.push((path, sha));
        }
        Ok(ObservationDayStats {
            date: fmt_day(day),
            repos_polled: repos.len() as u64,
            ok,
            not_modified,
            failed,
            skipped,
            files: digests,
            note: self.recovery_note(as_of)?,
        })
    }

    fn recovery_note(&self, as_of: OffsetDateTime) -> Result<Option<String>, StoreError> {
        let mut mentioned = String::new();
        for entry in self.chain.entries() {
            if let Some(note) = entry
                .observation_digest
                .as_ref()
                .and_then(|d| d.note.as_ref())
            {
                mentioned.push_str(note);
                mentioned.push('\n');
            }
        }
        let mut notes = Vec::new();
        for (rel, bytes) in self.torn_files()? {
            if mentioned.contains(&rel) {
                continue;
            }
            notes.push(format!(
                "torn write preserved: {rel} ({} bytes)",
                bytes.len()
            ));
        }
        for seq in self.witness_backlog(as_of)? {
            let text = format!(
                "witness backlog: head seq {seq} has no Rekor log_index after {WITNESS_BACKLOG_HOURS}h"
            );
            if !mentioned.contains(&text) {
                notes.push(text);
            }
        }
        // Prior seal's archive failures — appear in *this* digest (the next one).
        for failure in &self.pending_archive_failures {
            let text = format_archive_failure_note(failure);
            if !mentioned.contains(&text) {
                notes.push(text);
            }
        }
        for warning in &self.pending_identity_warnings {
            if !mentioned.contains(warning) {
                notes.push(warning.clone());
            }
        }
        for failure in &self.pending_publish_failures {
            if is_deferred_publish(failure) {
                continue;
            }
            let text = format_publish_failure_note(failure);
            if !mentioned.contains(&text) {
                notes.push(text);
            }
        }
        for success in &self.pending_publish_successes {
            let text = format_publish_success_note(success);
            if !mentioned.contains(&text) {
                notes.push(text);
            }
        }
        if self.observations_on_data_branch {
            let text = "observation files published on the data branch";
            if !mentioned.contains(text) {
                notes.push(text.to_owned());
            }
        }
        notes.sort();
        if notes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(notes.join("; ")))
        }
    }

    /// Seqs whose latest heads.jsonl line still lacks `rekor.log_index` and
    /// whose `head.recorded_at` is older than [`WITNESS_BACKLOG_HOURS`].
    pub fn witness_backlog(&self, as_of: OffsetDateTime) -> Result<Vec<u64>, StoreError> {
        let lines = self.head_lines()?;
        let mut latest: std::collections::BTreeMap<u64, &Value> = std::collections::BTreeMap::new();
        for line in &lines {
            if let Some(seq) = line
                .get("head")
                .and_then(|h| h.get("seq"))
                .and_then(|v| v.as_u64())
            {
                latest.insert(seq, line);
            }
        }
        let cutoff = as_of - time::Duration::hours(WITNESS_BACKLOG_HOURS);
        let mut out = Vec::new();
        for (seq, line) in latest {
            let has_index = line
                .get("rekor")
                .and_then(|r| r.get("log_index"))
                .and_then(|v| v.as_u64())
                .is_some();
            if has_index {
                continue;
            }
            let Some(recorded) = line
                .get("head")
                .and_then(|h| h.get("recorded_at"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let Ok(ts) =
                OffsetDateTime::parse(recorded, &time::format_description::well_known::Rfc3339)
            else {
                continue;
            };
            if ts <= cutoff {
                out.push(seq);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    fn publish_head(&mut self, entry: &Entry) -> Result<(), StoreError> {
        let entry_hash = entry
            .entry_hash
            .clone()
            .ok_or_else(|| StoreError::Chain("digest entry has no hash".into()))?;
        let head = Head {
            seq: entry.seq,
            entry_hash,
            recorded_at: entry.recorded_at,
            log_id: self.log_id.clone(),
        };
        let signed = sign_head(&head, &self.key).map_err(|e| StoreError::Message(e.to_string()))?;
        let canonical = canonical_json(
            &serde_json::to_value(&head).map_err(|e| StoreError::Message(e.to_string()))?,
        )
        .map_err(|e| StoreError::Message(e.to_string()))?;
        let (body, artifact_hash) = hashedrekord_body(&canonical, &self.key)?;
        let result = witness_submit(self.rekor.as_ref(), &body, &artifact_hash);
        let line = head_record(&signed, &self.key, &artifact_hash, &result, 1)?;
        self.vol.append_record("log/heads.jsonl", &line)?;
        Ok(())
    }

    fn head_lines(&self) -> Result<Vec<Value>, StoreError> {
        let Some(bytes) = self.vol.read("log/heads.jsonl")? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let value = serde_json::from_slice(line)
                .map_err(|e| StoreError::Corrupt(format!("heads.jsonl:{}: {e}", i + 1)))?;
            out.push(value);
        }
        Ok(out)
    }

    fn day_logs(&self) -> Result<Vec<String>, StoreError> {
        let mut files = self.vol.list("log")?;
        files.retain(|p| is_day_log(p));
        files.sort();
        Ok(files)
    }
}

impl Store<FaultVolume> {
    pub fn fault_mut(&mut self) -> &mut FaultVolume {
        &mut self.vol
    }
}

fn load_entry_buffer<V: Volume>(vol: &mut V) -> Result<Vec<UnhashedEntry>, StoreError> {
    let Some(bytes) = vol.read(ENTRY_BUFFER_PATH)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let entry: UnhashedEntry = serde_json::from_slice(line)
            .map_err(|e| StoreError::Corrupt(format!("entry_buffer.jsonl:{}: {e}", i + 1)))?;
        out.push(entry);
    }
    Ok(out)
}

/// Submit to Rekor; on conflict (409 / already exists), look up the existing entry.
fn witness_submit(
    rekor: &dyn RekorClient,
    body: &Value,
    artifact_hash: &str,
) -> Result<RekorAcceptance, String> {
    match rekor.submit(body) {
        Ok(accepted) => Ok(accepted),
        Err(err) if is_rekor_already_exists(&err) => match rekor.lookup_by_hash(artifact_hash)? {
            Some(accepted) => Ok(accepted),
            None => Err(format!("{err}; lookup found no entry for {artifact_hash}")),
        },
        Err(err) => Err(err),
    }
}

fn is_rekor_already_exists(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    err.contains("409") || lower.contains("already exists") || lower.contains("entry already")
}

fn load_archive_failures<V: Volume>(vol: &mut V) -> Result<Vec<ArchiveFailure>, StoreError> {
    let Some(bytes) = vol.read(ARCHIVE_FAILURES_PATH)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(line).map_err(|e| {
            StoreError::Corrupt(format!("archive_upload_failures.jsonl:{}: {e}", i + 1))
        })?;
        let day = value
            .get("day")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "archive_upload_failures.jsonl:{}: missing day",
                    i + 1
                ))
            })?
            .to_owned();
        let error = value
            .get("error")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "archive_upload_failures.jsonl:{}: missing error",
                    i + 1
                ))
            })?
            .to_owned();
        out.push(ArchiveFailure { day, error });
    }
    Ok(out)
}

fn load_identity_warnings<V: Volume>(vol: &mut V) -> Result<Vec<String>, StoreError> {
    let Some(bytes) = vol.read(IDENTITY_WARNINGS_PATH)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|e| StoreError::Corrupt(format!("identity_warnings.jsonl:{}: {e}", i + 1)))?;
        let warning = value
            .get("warning")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "identity_warnings.jsonl:{}: missing warning",
                    i + 1
                ))
            })?
            .to_owned();
        out.push(warning);
    }
    Ok(out)
}

fn load_publish_failures<V: Volume>(vol: &mut V) -> Result<Vec<PublishFailure>, StoreError> {
    let Some(bytes) = vol.read(PUBLISH_FAILURES_PATH)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|e| StoreError::Corrupt(format!("publish_failures.jsonl:{}: {e}", i + 1)))?;
        let day = value
            .get("day")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!("publish_failures.jsonl:{}: missing day", i + 1))
            })?
            .to_owned();
        let error = value
            .get("error")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!("publish_failures.jsonl:{}: missing error", i + 1))
            })?
            .to_owned();
        let seq = value.get("seq").and_then(|v| v.as_u64());
        out.push(PublishFailure { day, seq, error });
    }
    Ok(out)
}

fn load_publish_successes<V: Volume>(vol: &mut V) -> Result<Vec<PublishSuccess>, StoreError> {
    let Some(bytes) = vol.read(PUBLISH_SUCCESSES_PATH)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|e| StoreError::Corrupt(format!("publish_successes.jsonl:{}: {e}", i + 1)))?;
        let day = value
            .get("day")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!("publish_successes.jsonl:{}: missing day", i + 1))
            })?
            .to_owned();
        let seq = value.get("seq").and_then(|v| v.as_u64());
        out.push(PublishSuccess { day, seq });
    }
    Ok(out)
}

fn parse_day_label(label: &str) -> Result<Date, StoreError> {
    let parts: Vec<_> = label.split('-').collect();
    if parts.len() != 3 {
        return Err(StoreError::Message(format!("bad day label: {label}")));
    }
    let year: i32 = parts[0]
        .parse()
        .map_err(|_| StoreError::Message(format!("bad day label: {label}")))?;
    let month_n: u8 = parts[1]
        .parse()
        .map_err(|_| StoreError::Message(format!("bad day label: {label}")))?;
    let day_n: u8 = parts[2]
        .parse()
        .map_err(|_| StoreError::Message(format!("bad day label: {label}")))?;
    let month = time::Month::try_from(month_n)
        .map_err(|_| StoreError::Message(format!("bad day label: {label}")))?;
    Date::from_calendar_date(year, month, day_n)
        .map_err(|_| StoreError::Message(format!("bad day label: {label}")))
}

/// Move legacy `log/*.jsonl` sidecars into `state/` so `log/` holds only the
/// chain. Preserves pending digest notes (for example the false-422 warning).
fn migrate_log_sidecars<V: Volume>(vol: &mut V) -> Result<(), StoreError> {
    migrate_one_sidecar(vol, LEGACY_IDENTITY_WARNINGS, IDENTITY_WARNINGS_PATH)?;
    migrate_one_sidecar(vol, LEGACY_PUBLISH_FAILURES, PUBLISH_FAILURES_PATH)?;
    migrate_one_sidecar(vol, LEGACY_ARCHIVE_FAILURES, ARCHIVE_FAILURES_PATH)?;
    Ok(())
}

fn migrate_one_sidecar<V: Volume>(vol: &mut V, legacy: &str, dest: &str) -> Result<(), StoreError> {
    let Some(legacy_bytes) = vol.read(legacy)? else {
        return Ok(());
    };
    if legacy_bytes.is_empty() {
        vol.remove(legacy)?;
        return Ok(());
    }
    let existing = vol.read(dest)?.unwrap_or_default();
    let mut merged = existing;
    if !merged.is_empty() && !merged.ends_with(b"\n") {
        merged.push(b'\n');
    }
    merged.extend_from_slice(&legacy_bytes);
    if !merged.ends_with(b"\n") {
        merged.push(b'\n');
    }
    vol.write_exact(dest, &merged)?;
    vol.remove(legacy)?;
    Ok(())
}

fn load_chain<V: Volume>(
    vol: &mut V,
    recovered_at: OffsetDateTime,
    log_id: &str,
) -> Result<(Chain, BTreeSet<Date>, BTreeSet<Date>), StoreError> {
    let mut files = vol.list("log")?;
    files.retain(|p| is_day_log(p));
    files.sort();
    if let Some(last) = files.last().cloned() {
        recover_tail(vol, &last, recovered_at)?;
    }
    for (i, rel) in files.iter().enumerate() {
        if i + 1 == files.len() {
            continue;
        }
        let bytes = vol.read(rel)?.unwrap_or_default();
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(StoreError::Corrupt(format!(
                "{rel} does not end in a newline and is not the chain tail"
            )));
        }
    }
    // Re-list: recovery may have removed an all-torn file.
    let mut files = vol.list("log")?;
    files.retain(|p| is_day_log(p));
    files.sort();
    let mut entries = Vec::new();
    for rel in &files {
        let bytes = vol.read(rel)?.unwrap_or_default();
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(StoreError::Corrupt(format!(
                "{rel} still has a torn tail after recovery"
            )));
        }
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: Entry = serde_json::from_slice(line)
                .map_err(|e| StoreError::Corrupt(format!("{rel}:{}: {e}", i + 1)))?;
            entries.push(entry);
        }
    }
    let chain = Chain::from_verified(log_id, entries)
        .map_err(|e| StoreError::Corrupt(format!("tail failed verification: {e}")))?;

    let mut activity = BTreeSet::new();
    let mut sealed = BTreeSet::new();
    for entry in chain.entries() {
        if entry.event == Event::ObservationDigest {
            if let Some(d) = &entry.observation_digest {
                if let Some(date) = parse_day(&d.date) {
                    sealed.insert(date);
                }
            }
        } else {
            activity.insert(entry.recorded_at.as_offset_datetime().date());
        }
    }
    let obs = vol.list("observations")?;
    for rel in obs {
        if let Some(date) = date_from_obs_rel(&rel) {
            activity.insert(date);
        }
    }
    Ok((chain, activity, sealed))
}

fn recover_tail<V: Volume>(
    vol: &mut V,
    rel: &str,
    recovered_at: OffsetDateTime,
) -> Result<(), StoreError> {
    let bytes = vol.read(rel)?.unwrap_or_default();
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let split = bytes.iter().rposition(|b| *b == b'\n');
    let (keep, torn) = match split {
        Some(i) => (&bytes[..=i], &bytes[i + 1..]),
        None => (&b""[..], &bytes[..]),
    };
    if torn.is_empty() {
        return Ok(());
    }
    let torn_rel = format!("{rel}.torn.{}", stamp(recovered_at));
    // Write the sidecar first so a crash cannot drop the only copy.
    vol.write_exact(&torn_rel, torn)?;
    if keep.is_empty() {
        vol.remove(rel)?;
    } else {
        vol.write_exact(rel, keep)?;
    }
    Ok(())
}

fn is_day_log(rel: &str) -> bool {
    // Day files are exactly log/YYYY/MM/DD.jsonl — never sidecars under log/.
    if rel.contains(".torn.") {
        return false;
    }
    let Some(rest) = rel.strip_prefix("log/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 {
        return false;
    }
    let (y, m, file) = (parts[0], parts[1], parts[2]);
    let Some(d) = file.strip_suffix(".jsonl") else {
        return false;
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return false;
    }
    if !y.chars().all(|c| c.is_ascii_digit())
        || !m.chars().all(|c| c.is_ascii_digit())
        || !d.chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    true
}

fn observation_rel(day: Date, repo_segment: String) -> String {
    format!(
        "observations/{:04}/{:02}/{:02}/{repo_segment}.jsonl",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

fn log_rel(day: Date) -> String {
    format!(
        "log/{:04}/{:02}/{:02}.jsonl",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

fn fmt_day(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

fn parse_day(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

fn date_from_obs_rel(rel: &str) -> Option<Date> {
    // observations/YYYY/MM/DD/<repo>.jsonl
    let mut parts = rel.split('/');
    if parts.next()? != "observations" {
        return None;
    }
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

fn start_of(day: Date) -> OffsetDateTime {
    PrimitiveDateTime::new(day, Time::MIDNIGHT).assume_utc()
}

fn stamp(t: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 1_000_000
    )
}

fn hashedrekord_body(
    canonical_head: &[u8],
    key: &SigningKey,
) -> Result<(Value, String), StoreError> {
    let hash = Sha512::digest(canonical_head);
    let hex_hash = hex::encode(hash);
    let artifact_hash = format!("sha512:{hex_hash}");
    let sig =
        sign_ed25519ph(key, canonical_head).map_err(|e| StoreError::Message(e.to_string()))?;
    let pem = public_key_pkix_pem(&key.verifying_key_bytes());
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "apiVersion": REKOR_VERSION,
        "kind": REKOR_KIND,
        "spec": {
            "data": {
                "hash": {
                    "algorithm": "sha512",
                    "value": hex_hash,
                }
            },
            "signature": {
                "content": b64.encode(sig),
                "publicKey": {
                    "content": b64.encode(pem.as_bytes()),
                }
            }
        }
    });
    Ok((body, artifact_hash))
}

fn head_record(
    signed: &refledger_log::SignedHead,
    key: &SigningKey,
    artifact_hash: &str,
    result: &Result<RekorAcceptance, String>,
    attempts: u64,
) -> Result<Vec<u8>, StoreError> {
    let mut rekor = serde_json::Map::new();
    rekor.insert("kind".into(), REKOR_KIND.into());
    rekor.insert("api_version".into(), REKOR_VERSION.into());
    rekor.insert("artifact_hash".into(), artifact_hash.into());
    rekor.insert("attempts".into(), serde_json::json!(attempts));
    match result {
        Ok(accepted) => {
            rekor.insert("log_index".into(), serde_json::json!(accepted.log_index));
            rekor.insert("uuid".into(), accepted.uuid.clone().into());
            if let Some(id) = &accepted.log_id {
                rekor.insert("log_id".into(), id.clone().into());
            }
            if let Some(t) = accepted.integrated_time {
                rekor.insert("integrated_time".into(), serde_json::json!(t));
            }
        }
        Err(err) => {
            let clipped: String = err.chars().take(500).collect();
            rekor.insert("error".into(), clipped.into());
        }
    }
    let value = serde_json::json!({
        "head": &signed.head,
        "signature": &signed.signature,
        "public_key": &signed.public_key,
        "key_id": key_id(&key.verifying_key_bytes()),
        "rekor": Value::Object(rekor),
    });
    canonical_json(&value).map_err(|e| StoreError::Message(e.to_string()))
}

fn head_record_from_existing(
    previous: &Value,
    artifact_hash: &str,
    result: &Result<RekorAcceptance, String>,
    attempts: u64,
) -> Result<Vec<u8>, StoreError> {
    let mut rekor = serde_json::Map::new();
    rekor.insert("kind".into(), REKOR_KIND.into());
    rekor.insert("api_version".into(), REKOR_VERSION.into());
    rekor.insert("artifact_hash".into(), artifact_hash.into());
    rekor.insert("attempts".into(), serde_json::json!(attempts));
    match result {
        Ok(accepted) => {
            rekor.insert("log_index".into(), serde_json::json!(accepted.log_index));
            rekor.insert("uuid".into(), accepted.uuid.clone().into());
            if let Some(id) = &accepted.log_id {
                rekor.insert("log_id".into(), id.clone().into());
            }
            if let Some(t) = accepted.integrated_time {
                rekor.insert("integrated_time".into(), serde_json::json!(t));
            }
        }
        Err(err) => {
            let clipped: String = err.chars().take(500).collect();
            rekor.insert("error".into(), clipped.into());
        }
    }
    let value = serde_json::json!({
        "head": previous.get("head").cloned().unwrap_or(Value::Null),
        "signature": previous.get("signature").cloned().unwrap_or(Value::Null),
        "public_key": previous.get("public_key").cloned().unwrap_or(Value::Null),
        "key_id": previous.get("key_id").cloned().unwrap_or(Value::Null),
        "rekor": Value::Object(rekor),
    });
    canonical_json(&value).map_err(|e| StoreError::Message(e.to_string()))
}
