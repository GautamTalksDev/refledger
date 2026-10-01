//! Append-only hash chain (LOG-FORMAT.md §§2–3, 5).
//!
//! [`verify`] is a free function so a party who never built the chain can
//! still check entries loaded from disk.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;

use crate::canonical::canonical_json;
use crate::entry::{
    Ancestry, Binding, Classification, Correlation, Diff, Entry, EntryError, Event, HashRef,
    ObservationDigest, PopulationChange, RefForm, RefType, Severity, Timestamp, FORMAT_VERSION,
};

/// Published head of a log (LOG-FORMAT.md §4), unsigned.
pub use crate::sign::Head;

/// Errors from append or [`verify`]. Every verification variant names the
/// first failing `seq` and which check failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChainError {
    #[error("prev_hash mismatch at seq {seq}")]
    PrevHashMismatch { seq: u64 },
    #[error("entry_hash mismatch at seq {seq}")]
    EntryHashMismatch { seq: u64 },
    #[error("sequence gap at seq {seq}")]
    SeqGap { seq: u64 },
    #[error("sequence not ascending at seq {seq}")]
    SeqNotAscending { seq: u64 },
    #[error("genesis must be first (seq 0 with zero prev_hash); saw seq {seq}")]
    GenesisNotFirst { seq: u64 },
    #[error("chain already has a genesis entry")]
    GenesisAlreadyPresent,
    #[error("entry error: {0}")]
    Entry(#[from] EntryError),
    #[error("canonicalisation error: {0}")]
    Canonical(String),
    #[error("serde error: {0}")]
    Serde(String),
    #[error("io error: {0}")]
    Io(String),
}

impl ChainError {
    pub fn first_bad_seq(&self) -> Option<u64> {
        match self {
            Self::PrevHashMismatch { seq }
            | Self::EntryHashMismatch { seq }
            | Self::SeqGap { seq }
            | Self::SeqNotAscending { seq }
            | Self::GenesisNotFirst { seq } => Some(*seq),
            Self::GenesisAlreadyPresent
            | Self::Entry(_)
            | Self::Canonical(_)
            | Self::Serde(_)
            | Self::Io(_) => None,
        }
    }
}

/// Entry content without chain linkage fields. [`Chain::append`] assigns
/// `seq`, `prev_hash`, and `entry_hash`.
///
/// Serde is for the poller's durable entry buffer only — not the sealed log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnhashedEntry {
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_form: Option<RefForm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestry: Option<Ancestry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_type_before: Option<RefType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_type_after: Option<RefType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Diff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_observations: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<Correlation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_digest: Option<ObservationDigest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub population_change: Option<PopulationChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_window_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_latency_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl UnhashedEntry {
    pub fn empty(recorded_at: OffsetDateTime, event: Event) -> Self {
        Self {
            recorded_at,
            event,
            classification: None,
            severity: None,
            ref_form: None,
            ancestry: None,
            repo: None,
            r#ref: None,
            ref_type_before: None,
            ref_type_after: None,
            from: None,
            to: None,
            diff: None,
            gap_seconds: None,
            source_observations: None,
            correlation: None,
            observation_digest: None,
            population_change: None,
            http_status: None,
            redirect_location: None,
            observation_window_seconds: None,
            detection_latency_note: None,
            corrects_seq: None,
            reason: None,
        }
    }

    pub fn correction(
        recorded_at: OffsetDateTime,
        corrects_seq: u64,
        reason: impl Into<String>,
    ) -> Self {
        let mut s = Self::empty(recorded_at, Event::Correction);
        s.corrects_seq = Some(corrects_seq);
        s.reason = Some(reason.into());
        s
    }

    /// Build a Move draft from a grouped field set (keeps the required Move
    /// fields together without a 12-argument constructor).
    pub fn move_event(draft: MoveDraft) -> Self {
        let mut s = Self::empty(draft.recorded_at, Event::Move);
        s.classification = Some(draft.classification);
        s.severity = Some(draft.severity);
        s.repo = Some(draft.repo);
        s.r#ref = Some(draft.ref_name);
        s.ref_type_before = Some(draft.ref_type_before);
        s.ref_type_after = Some(draft.ref_type_after);
        s.from = Some(draft.from);
        s.to = Some(draft.to);
        s.diff = draft.diff;
        s.source_observations = Some(draft.source_observations);
        s.observation_window_seconds = Some(draft.observation_window_seconds);
        s
    }
}

/// Required fields for [`UnhashedEntry::move_event`].
#[derive(Debug, Clone)]
pub struct MoveDraft {
    pub recorded_at: OffsetDateTime,
    pub classification: Classification,
    pub severity: Severity,
    pub repo: String,
    pub ref_name: String,
    pub ref_type_before: RefType,
    pub ref_type_after: RefType,
    pub from: Binding,
    pub to: Binding,
    pub observation_window_seconds: u64,
    pub source_observations: Vec<String>,
    pub diff: Option<Diff>,
}

/// In-memory chain with JSONL durability under `data/log/YYYY/MM/DD.jsonl`.
pub struct Chain {
    log_id: String,
    root: PathBuf,
    entries: Vec<Entry>,
    /// When false, append stays in memory only (tests). Production uses true.
    durable: bool,
}

impl Chain {
    /// Create an empty durable chain rooted at `data/log`.
    pub fn new(log_id: impl Into<String>) -> Self {
        Self::open(log_id, "data/log")
    }

    /// Create an empty durable chain with a custom storage root.
    pub fn open(log_id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            log_id: log_id.into(),
            root: root.into(),
            entries: Vec::new(),
            durable: true,
        }
    }

    /// In-memory chain for tests — same hashing/linkage, no JSONL I/O.
    pub fn ephemeral(log_id: impl Into<String>) -> Self {
        Self {
            log_id: log_id.into(),
            root: PathBuf::new(),
            entries: Vec::new(),
            durable: false,
        }
    }

    pub fn log_id(&self) -> &str {
        &self.log_id
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Load an already-verified chain. Disk I/O stays with the caller —
    /// [`crate::chain::Chain`] here is the linkage calculator, not the
    /// file owner.
    pub fn from_verified(
        log_id: impl Into<String>,
        entries: Vec<Entry>,
    ) -> Result<Self, ChainError> {
        verify(&entries)?;
        Ok(Self {
            log_id: log_id.into(),
            root: PathBuf::new(),
            entries,
            durable: false,
        })
    }

    /// Drop the last entry after a failed durable write. The caller must
    /// not have published that entry.
    pub fn rollback_last(&mut self) -> Option<Entry> {
        self.entries.pop()
    }

    pub fn head(&self) -> Option<Head> {
        let last = self.entries.last()?;
        Some(Head {
            seq: last.seq,
            entry_hash: last.entry_hash.clone()?,
            recorded_at: last.recorded_at,
            log_id: self.log_id.clone(),
        })
    }

    /// Append one entry. The first append is genesis (`seq == 0`, zero `prev_hash`).
    pub fn append(&mut self, unhashed: UnhashedEntry) -> Result<&Entry, ChainError> {
        let seq = self.entries.len() as u64;
        let prev_hash = if seq == 0 {
            HashRef::genesis()
        } else {
            self.entries[seq as usize - 1]
                .entry_hash
                .clone()
                .ok_or(ChainError::EntryHashMismatch { seq: seq - 1 })?
        };

        let mut entry = Entry {
            format_version: FORMAT_VERSION,
            seq,
            prev_hash,
            entry_hash: None,
            recorded_at: Timestamp::from_offset_datetime(unhashed.recorded_at)?,
            event: unhashed.event,
            classification: unhashed.classification,
            severity: unhashed.severity,
            ref_form: unhashed.ref_form,
            ancestry: unhashed.ancestry,
            repo: unhashed.repo,
            r#ref: unhashed.r#ref,
            ref_type_before: unhashed.ref_type_before,
            ref_type_after: unhashed.ref_type_after,
            from: unhashed.from,
            to: unhashed.to,
            diff: unhashed.diff,
            gap_seconds: unhashed.gap_seconds,
            source_observations: unhashed.source_observations,
            correlation: unhashed.correlation,
            observation_digest: unhashed.observation_digest,
            population_change: unhashed.population_change,
            http_status: unhashed.http_status,
            redirect_location: unhashed.redirect_location,
            observation_window_seconds: unhashed.observation_window_seconds,
            detection_latency_note: unhashed.detection_latency_note,
            corrects_seq: unhashed.corrects_seq,
            reason: unhashed.reason,
        };
        entry.validate()?;

        let hash = compute_entry_hash(&entry)?;
        entry.entry_hash = Some(hash);

        if self.durable {
            self.persist_line(&entry)?;
        }
        self.entries.push(entry);
        Ok(self.entries.last().expect("just pushed"))
    }

    /// Explicit genesis append — fails if the chain is already non-empty.
    pub fn append_genesis(&mut self, unhashed: UnhashedEntry) -> Result<&Entry, ChainError> {
        if !self.entries.is_empty() {
            return Err(ChainError::GenesisAlreadyPresent);
        }
        self.append(unhashed)
    }

    fn persist_line(&self, entry: &Entry) -> Result<(), ChainError> {
        let recorded = entry.recorded_at.as_offset_datetime();
        let path = day_path(&self.root, recorded);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ChainError::Io(e.to_string()))?;
        }

        let value = serde_json::to_value(entry).map_err(|e| ChainError::Serde(e.to_string()))?;
        let line = canonical_json(&value).map_err(|e| ChainError::Canonical(e.to_string()))?;

        // O_APPEND: a log that loses its last entry on power failure is a log
        // that will one day be missing exactly the entry that mattered.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| ChainError::Io(e.to_string()))?;
        file.write_all(&line)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| ChainError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| ChainError::Io(e.to_string()))?;
        Ok(())
    }
}

fn day_path(root: &Path, when: OffsetDateTime) -> PathBuf {
    let (year, month, day) = (when.year(), u8::from(when.month()), when.day());
    root.join(format!("{year:04}/{month:02}/{day:02}.jsonl"))
}

fn compute_entry_hash(entry: &Entry) -> Result<HashRef, ChainError> {
    debug_assert!(
        entry.entry_hash.is_none(),
        "hash over an entry that already carries entry_hash"
    );
    let value = serde_json::to_value(entry).map_err(|e| ChainError::Serde(e.to_string()))?;
    let bytes = canonical_json(&value).map_err(|e| ChainError::Canonical(e.to_string()))?;
    let digest = Sha256::digest(&bytes);
    HashRef::parse(format!("sha256:{}", hex::encode(digest))).map_err(ChainError::from)
}

fn compute_entry_hash_ignoring_stored(entry: &Entry) -> Result<HashRef, ChainError> {
    let mut clone = entry.clone();
    clone.entry_hash = None;
    compute_entry_hash(&clone)
}

/// Verify a slice of entries loaded from anywhere. Not a method on [`Chain`]
/// so disk-only verifiers never need a builder.
pub fn verify(entries: &[Entry]) -> Result<(), ChainError> {
    if entries.is_empty() {
        return Ok(());
    }

    let first = &entries[0];
    if first.seq != 0 || first.prev_hash != HashRef::genesis() {
        return Err(ChainError::GenesisNotFirst { seq: first.seq });
    }

    for (i, entry) in entries.iter().enumerate() {
        let expected_seq = i as u64;
        if entry.seq != expected_seq {
            if entry.seq > expected_seq {
                return Err(ChainError::SeqGap { seq: expected_seq });
            }
            return Err(ChainError::SeqNotAscending { seq: expected_seq });
        }

        if i > 0 {
            let prev = &entries[i - 1];
            if prev.seq + 1 != entry.seq {
                return Err(ChainError::SeqNotAscending { seq: entry.seq });
            }
            let Some(prev_hash) = prev.entry_hash.as_ref() else {
                return Err(ChainError::EntryHashMismatch { seq: prev.seq });
            };
            if &entry.prev_hash != prev_hash {
                return Err(ChainError::PrevHashMismatch { seq: entry.seq });
            }
        } else if entry.prev_hash != HashRef::genesis() {
            return Err(ChainError::PrevHashMismatch { seq: 0 });
        }

        let expected_hash = compute_entry_hash_ignoring_stored(entry)?;
        match &entry.entry_hash {
            Some(h) if h == &expected_hash => {}
            _ => return Err(ChainError::EntryHashMismatch { seq: entry.seq }),
        }
    }
    Ok(())
}
