//! Chain replay and head verification from docs/LOG-FORMAT.md §§2–4.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::canonical::{canonical_json, CanonError};

const GENESIS_PREV: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("canonical: {0}")]
    Canonical(#[from] CanonError),
    #[error("{failure}")]
    Failed { failure: Failure },
    #[error("head: {0}")]
    Head(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Failure {
    #[error("entry_hash mismatch at seq {seq}")]
    EntryHashMismatch { seq: u64 },
    #[error("prev_hash mismatch at seq {seq}")]
    PrevHashMismatch { seq: u64 },
    #[error("sequence gap at seq {seq}")]
    SeqGap { seq: u64 },
    #[error("sequence not ascending at seq {seq}")]
    SeqNotAscending { seq: u64 },
    #[error("genesis must be first; saw seq {seq}")]
    GenesisNotFirst { seq: u64 },
    #[error("missing entry_hash at seq {seq}")]
    MissingEntryHash { seq: u64 },
    #[error("missing seq field")]
    MissingSeq,
    #[error("format_version must be 1 at seq {seq}")]
    InvalidFormatVersion { seq: u64 },
    #[error("correlation member_seq must be < own seq at seq {seq}")]
    CorrelationMemberSeq { seq: u64 },
    #[error("observation_digest file mismatch at seq {seq}: {detail}")]
    ObservationDigestMismatch { seq: u64, detail: String },
    #[error("head signature invalid")]
    HeadSignatureInvalid,
    #[error("head entry_hash does not match tip")]
    HeadHashMismatch,
    #[error("head seq does not match tip")]
    HeadSeqMismatch,
    #[error("head public key does not match --pubkey")]
    HeadPubkeyMismatch,
    #[error("head key_id does not match the public key")]
    HeadKeyIdMismatch,
    #[error("head seq {seq} is not in the log")]
    HeadSeqMissing { seq: u64 },
    #[error("witness backlog: head seq {seq} has no Rekor log_index after 48h")]
    WitnessBacklog { seq: u64 },
    #[error("entry seq {seq} is more than 48h older than {relative_to}")]
    EntryTooOld { seq: u64, relative_to: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainVerdict {
    pub ok: bool,
    pub entries: u64,
    pub seq_first: Option<u64>,
    pub seq_last: Option<u64>,
    pub span_start: Option<String>,
    pub span_end: Option<String>,
    pub head: Option<HeadStatus>,
    pub coverage_gaps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadStatus {
    pub present: bool,
    pub valid: bool,
    pub key_prefix: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureReport {
    pub seq: Option<u64>,
    pub check: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignedHeadFile {
    pub head: HeadBody,
    pub signature: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeadBody {
    pub seq: u64,
    pub entry_hash: String,
    pub recorded_at: String,
    pub log_id: String,
}

/// SHA-256 of canonical JSON of `entry` with `entry_hash` omitted.
pub fn entry_hash(entry: &Value) -> Result<String, VerifyError> {
    let mut for_hash = entry.clone();
    if let Some(obj) = for_hash.as_object_mut() {
        obj.remove("entry_hash");
    } else {
        return Err(VerifyError::Parse("entry must be a JSON object".into()));
    }
    let bytes = canonical_json(&for_hash)?;
    let digest = Sha256::digest(&bytes);
    Ok(format!("sha256:{}", hex::encode(digest)))
}

/// Result of scanning a log directory for chain day files.
#[derive(Debug, Default)]
pub struct LogDirLoad {
    pub entries: Vec<Value>,
    /// Paths relative to the log dir that were not loaded as chain entries.
    /// `heads.jsonl` is expected and omitted from this list.
    pub ignored: Vec<String>,
}

/// Load chain day files (`YYYY/MM/DD.jsonl`) under `log_dir`, sorted by path.
///
/// Documented layout: only those day files and `heads.jsonl`. Any other file
/// is listed in [`LogDirLoad::ignored`] and is not parsed.
pub fn load_jsonl_dir(log_dir: &Path) -> Result<LogDirLoad, VerifyError> {
    let scanned = scan_log_dir(log_dir)?;
    let mut entries = Vec::new();
    for path in &scanned.day_files {
        let file = File::open(path).map_err(|e| VerifyError::Io(format!("{path:?}: {e}")))?;
        let reader = BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| VerifyError::Io(format!("{path:?}:{lineno}: {e}")))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| VerifyError::Parse(format!("{path:?}:{}: {e}", lineno + 1)))?;
            entries.push(value);
        }
    }
    Ok(LogDirLoad {
        entries,
        ignored: scanned.ignored,
    })
}

struct ScannedLogDir {
    day_files: Vec<PathBuf>,
    ignored: Vec<String>,
}

fn scan_log_dir(log_dir: &Path) -> Result<ScannedLogDir, VerifyError> {
    if !log_dir.is_dir() {
        return Err(VerifyError::Io(format!(
            "log dir is not a directory: {log_dir:?}"
        )));
    }
    let mut day_files = Vec::new();
    let mut ignored = Vec::new();
    fn walk(
        root: &Path,
        dir: &Path,
        day_files: &mut Vec<PathBuf>,
        ignored: &mut Vec<String>,
    ) -> Result<(), VerifyError> {
        for ent in fs::read_dir(dir).map_err(|e| VerifyError::Io(e.to_string()))? {
            let ent = ent.map_err(|e| VerifyError::Io(e.to_string()))?;
            let path = ent.path();
            if path.is_dir() {
                walk(root, &path, day_files, ignored)?;
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.display().to_string());
            if is_chain_day_rel(&rel) {
                day_files.push(path);
            } else if rel == "heads.jsonl" {
                // Documented companion file; not a chain entry.
            } else {
                ignored.push(rel);
            }
        }
        Ok(())
    }
    walk(log_dir, log_dir, &mut day_files, &mut ignored)?;
    day_files.sort();
    ignored.sort();
    Ok(ScannedLogDir { day_files, ignored })
}

/// Relative path under the log dir matching `YYYY/MM/DD.jsonl`.
fn is_chain_day_rel(rel: &str) -> bool {
    if rel.contains(".torn.") {
        return false;
    }
    let parts: Vec<&str> = rel.split('/').collect();
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
    y.chars().all(|c| c.is_ascii_digit())
        && m.chars().all(|c| c.is_ascii_digit())
        && d.chars().all(|c| c.is_ascii_digit())
}

/// Verify chain linkage and hashes for `entries` (already filtered by seq range).
pub fn verify_chain(entries: &[Value]) -> Result<ChainVerdict, VerifyError> {
    if entries.is_empty() {
        return Ok(ChainVerdict {
            ok: true,
            entries: 0,
            seq_first: None,
            seq_last: None,
            span_start: None,
            span_end: None,
            head: None,
            coverage_gaps: 0,
            failure: None,
        });
    }

    let mut coverage_gaps = 0u64;
    let mut span_start: Option<String> = None;
    let mut span_end: Option<String> = None;
    let mut prev_hash: Option<String> = None;
    let mut expected_seq: Option<u64> = None;

    for entry in entries {
        let obj = entry
            .as_object()
            .ok_or_else(|| VerifyError::Parse("entry must be object".into()))?;

        let seq = obj
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or(VerifyError::Failed {
                failure: Failure::MissingSeq,
            })?;

        match obj.get("format_version").and_then(|v| v.as_u64()) {
            Some(1) => {}
            _ => {
                return Err(VerifyError::Failed {
                    failure: Failure::InvalidFormatVersion { seq },
                });
            }
        }

        if obj.get("event").and_then(|v| v.as_str()) == Some("correlation") {
            validate_correlation_member_seqs(obj, seq)?;
        }

        match expected_seq {
            None => expected_seq = Some(seq),
            Some(exp) => {
                if seq != exp {
                    let failure = if seq > exp {
                        Failure::SeqGap { seq: exp }
                    } else {
                        Failure::SeqNotAscending { seq: exp }
                    };
                    return Err(VerifyError::Failed { failure });
                }
            }
        }

        if let Some(s) = expected_seq {
            expected_seq = Some(s + 1);
        }

        let recorded = obj
            .get("recorded_at")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(ref r) = recorded {
            span_start = Some(match span_start.take() {
                Some(prev) if prev <= *r => prev,
                _ => r.clone(),
            });
            span_end = Some(match span_end.take() {
                Some(prev) if prev >= *r => prev,
                _ => r.clone(),
            });
        }

        if obj.get("event").and_then(|v| v.as_str()) == Some("coverage_gap") {
            coverage_gaps += 1;
        }

        let stored_hash =
            obj.get("entry_hash")
                .and_then(|v| v.as_str())
                .ok_or(VerifyError::Failed {
                    failure: Failure::MissingEntryHash { seq },
                })?;

        let computed = entry_hash(entry)?;
        if computed != stored_hash {
            return Err(VerifyError::Failed {
                failure: Failure::EntryHashMismatch { seq },
            });
        }

        let this_prev = obj
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerifyError::Parse(format!("missing prev_hash at seq {seq}")))?;

        match &prev_hash {
            None => {
                if seq == 0 && this_prev != GENESIS_PREV {
                    return Err(VerifyError::Failed {
                        failure: Failure::PrevHashMismatch { seq: 0 },
                    });
                }
            }
            Some(expected_prev) => {
                if this_prev != expected_prev {
                    return Err(VerifyError::Failed {
                        failure: Failure::PrevHashMismatch { seq },
                    });
                }
            }
        }

        prev_hash = Some(stored_hash.to_owned());
    }

    let first_seq = entries[0]
        .get("seq")
        .and_then(|v| v.as_u64())
        .ok_or(VerifyError::Failed {
            failure: Failure::MissingSeq,
        })?;
    if first_seq == 0 {
        let prev = entries[0]
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if prev != GENESIS_PREV {
            return Err(VerifyError::Failed {
                failure: Failure::GenesisNotFirst { seq: 0 },
            });
        }
    }

    let last_seq = entries
        .last()
        .and_then(|e| e.get("seq"))
        .and_then(|v| v.as_u64());

    Ok(ChainVerdict {
        ok: true,
        entries: entries.len() as u64,
        seq_first: Some(first_seq),
        seq_last: last_seq,
        span_start,
        span_end,
        head: None,
        coverage_gaps,
        failure: None,
    })
}

pub fn verify_signed_head(
    signed: &SignedHeadFile,
    tip: &Value,
    pubkey_override: Option<&str>,
) -> Result<HeadStatus, VerifyError> {
    if let Some(expected) = pubkey_override {
        if expected != signed.public_key {
            return Err(VerifyError::Failed {
                failure: Failure::HeadPubkeyMismatch,
            });
        }
    }
    let pubkey_hex = signed.public_key.as_str();

    let tip_seq = tip
        .get("seq")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| VerifyError::Head("tip missing seq".into()))?;
    let tip_hash = tip
        .get("entry_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VerifyError::Head("tip missing entry_hash".into()))?;

    if signed.head.seq != tip_seq {
        return Err(VerifyError::Failed {
            failure: Failure::HeadSeqMismatch,
        });
    }
    if signed.head.entry_hash != tip_hash {
        return Err(VerifyError::Failed {
            failure: Failure::HeadHashMismatch,
        });
    }

    let head_value = serde_json::json!({
        "entry_hash": signed.head.entry_hash,
        "log_id": signed.head.log_id,
        "recorded_at": signed.head.recorded_at,
        "seq": signed.head.seq,
    });
    let message = canonical_json(&head_value)?;

    let pk = decode_hex32(pubkey_hex).map_err(VerifyError::Head)?;
    let sig = decode_hex64(&signed.signature).map_err(VerifyError::Head)?;
    let verifying_key =
        VerifyingKey::from_bytes(&pk).map_err(|e| VerifyError::Head(e.to_string()))?;
    let signature = Signature::from_slice(&sig).map_err(|e| VerifyError::Head(e.to_string()))?;
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| VerifyError::Failed {
            failure: Failure::HeadSignatureInvalid,
        })?;

    Ok(HeadStatus {
        present: true,
        valid: true,
        key_prefix: pubkey_hex.chars().take(4).collect(),
    })
}

/// Verify every line of `heads.jsonl`.
///
/// Each line's head is checked against the log entry with that `seq`, not
/// against the chain tip. `key_id` must be `sha256:` + SHA-256 of the raw
/// 32-byte public key. `rekor` is witness metadata and is not signed.
pub fn verify_heads_jsonl(
    entries: &[Value],
    raw: &str,
    pubkey_override: Option<&str>,
) -> Result<HeadStatus, VerifyError> {
    let mut checked = 0u64;
    let mut last: Option<HeadStatus> = None;
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| VerifyError::Parse(format!("heads.jsonl:{}: {e}", lineno + 1)))?;
        let signed: SignedHeadFile = serde_json::from_value(value.clone())
            .map_err(|e| VerifyError::Parse(format!("heads.jsonl:{}: {e}", lineno + 1)))?;
        let seq = signed.head.seq;
        let entry = entries
            .iter()
            .find(|e| e.get("seq").and_then(|v| v.as_u64()) == Some(seq));
        let Some(entry) = entry else {
            return Err(VerifyError::Failed {
                failure: Failure::HeadSeqMissing { seq },
            });
        };
        // Historical heads address their own seq, so compare against that
        // entry rather than the tip. Reuse the tip checker by passing the
        // addressed entry.
        let status = verify_signed_head(&signed, entry, pubkey_override)?;
        let key_id = value
            .get("key_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                VerifyError::Parse(format!("heads.jsonl:{}: key_id required", lineno + 1))
            })?;
        let expected = key_id_for_hex(&signed.public_key).map_err(VerifyError::Head)?;
        if key_id != expected {
            return Err(VerifyError::Failed {
                failure: Failure::HeadKeyIdMismatch,
            });
        }
        checked += 1;
        last = Some(status);
    }
    if checked == 0 {
        return Err(VerifyError::Head("heads.jsonl contained no heads".into()));
    }
    last.ok_or_else(|| VerifyError::Head("heads.jsonl contained no heads".into()))
}

fn key_id_for_hex(public_key_hex: &str) -> Result<String, String> {
    let raw = decode_hex32(public_key_hex)?;
    let digest = Sha256::digest(raw);
    Ok(format!("sha256:{}", hex::encode(digest)))
}

const WITNESS_BACKLOG_HOURS: i64 = 48;

/// Fail when any entry is more than 48h older than wall-clock now (no heads yet).
pub fn verify_entries_not_stale_vs_now(entries: &[Value]) -> Result<(), VerifyError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    verify_entries_not_stale(entries, now, "now")
}

/// Fail when any entry is more than 48h older than the latest signed head.
pub fn verify_entries_not_stale_vs_heads(
    entries: &[Value],
    heads_raw: &str,
) -> Result<(), VerifyError> {
    let mut latest_head_at: Option<i64> = None;
    for (lineno, line) in heads_raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| VerifyError::Parse(format!("heads.jsonl:{}: {e}", lineno + 1)))?;
        let recorded = value
            .get("head")
            .and_then(|h| h.get("recorded_at"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                VerifyError::Parse(format!("heads.jsonl:{}: missing recorded_at", lineno + 1))
            })?;
        let at = parse_rfc3339(recorded)?;
        latest_head_at = Some(match latest_head_at {
            Some(prev) if prev >= at => prev,
            _ => at,
        });
    }
    let Some(head_at) = latest_head_at else {
        return verify_entries_not_stale_vs_now(entries);
    };
    verify_entries_not_stale(entries, head_at, "latest head")
}

fn verify_entries_not_stale(
    entries: &[Value],
    reference: i64,
    relative_to: &str,
) -> Result<(), VerifyError> {
    let cutoff = reference - (WITNESS_BACKLOG_HOURS * 3600);
    for entry in entries {
        let seq = entry.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let Some(recorded) = entry.get("recorded_at").and_then(|v| v.as_str()) else {
            continue;
        };
        let at = parse_rfc3339(recorded)?;
        if at < cutoff {
            return Err(VerifyError::Failed {
                failure: Failure::EntryTooOld {
                    seq,
                    relative_to: relative_to.to_owned(),
                },
            });
        }
    }
    Ok(())
}

/// Strict checks for the seven-day exit condition.
///
/// Requires `heads.jsonl` contents. Fails if any head whose `recorded_at` is
/// more than 48 hours before the newest chain entry still lacks
/// `rekor.log_index`, or if any ObservationDigest note records a witness
/// backlog. Does not call Rekor — resolution of indexes is an ops check.
pub fn verify_strict(entries: &[Value], heads_raw: &str) -> Result<(), VerifyError> {
    let tip_at = entries
        .iter()
        .rev()
        .find_map(|e| e.get("recorded_at").and_then(|v| v.as_str()))
        .ok_or_else(|| VerifyError::Io("--strict requires at least one log entry".into()))?;
    let tip = parse_rfc3339(tip_at)?;
    let cutoff = tip - (WITNESS_BACKLOG_HOURS * 3600);

    let mut latest: std::collections::BTreeMap<u64, &str> = std::collections::BTreeMap::new();
    let mut lines: Vec<Value> = Vec::new();
    for (lineno, line) in heads_raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| VerifyError::Parse(format!("heads.jsonl:{}: {e}", lineno + 1)))?;
        let seq = value
            .get("head")
            .and_then(|h| h.get("seq"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                VerifyError::Parse(format!("heads.jsonl:{}: missing seq", lineno + 1))
            })?;
        lines.push(value);
        latest.insert(seq, ""); // placeholder; rewrite below
    }
    // Re-key to owned values
    let mut latest_line: std::collections::BTreeMap<u64, &Value> =
        std::collections::BTreeMap::new();
    for value in &lines {
        let seq = value["head"]["seq"].as_u64().unwrap();
        latest_line.insert(seq, value);
    }
    let _ = latest;

    for (seq, value) in &latest_line {
        let recorded = value
            .get("head")
            .and_then(|h| h.get("recorded_at"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerifyError::Parse(format!("head seq {seq}: missing recorded_at")))?;
        let at = parse_rfc3339(recorded)?;
        if at > cutoff {
            continue;
        }
        let has_index = value
            .get("rekor")
            .and_then(|r| r.get("log_index"))
            .and_then(|v| v.as_u64())
            .is_some();
        if !has_index {
            return Err(VerifyError::Failed {
                failure: Failure::WitnessBacklog { seq: *seq },
            });
        }
    }

    for entry in entries {
        if entry.get("event").and_then(|v| v.as_str()) != Some("observation_digest") {
            continue;
        }
        let note = entry
            .get("observation_digest")
            .and_then(|d| d.get("note"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if note.contains("witness backlog") {
            let seq = entry.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
            return Err(VerifyError::Failed {
                failure: Failure::WitnessBacklog { seq },
            });
        }
    }
    Ok(())
}

/// Parse `YYYY-MM-DDTHH:MM:SS.sssZ` to unix seconds (UTC). Sufficient for the
/// 48h backlog comparison without pulling in a time crate.
fn parse_rfc3339(s: &str) -> Result<i64, VerifyError> {
    if !s.ends_with('Z') || s.len() < 20 {
        return Err(VerifyError::Parse(format!("bad timestamp {s}")));
    }
    let date = &s[..10];
    let time = &s[11..s.len() - 1]; // strip Z; may include .sss
    let mut dp = date.split('-');
    let year: i64 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad date {s}")))?;
    let month: i64 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad date {s}")))?;
    let day: i64 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad date {s}")))?;
    let (hms, _frac) = match time.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time, "0"),
    };
    let mut tp = hms.split(':');
    let hour: i64 = tp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad time {s}")))?;
    let min: i64 = tp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad time {s}")))?;
    let sec: i64 = tp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| VerifyError::Parse(format!("bad time {s}")))?;
    // Days from civil date (Howard Hinnant algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468; // days since 1970-01-01
    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn decode_hex32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 || !is_lower_hex(s) {
        return Err("public key must be 64 lowercase hex chars".into());
    }
    let v = hex::decode(s).map_err(|e| e.to_string())?;
    v.try_into().map_err(|_| "public key length".into())
}

fn decode_hex64(s: &str) -> Result<[u8; 64], String> {
    if s.len() != 128 || !is_lower_hex(s) {
        return Err("signature must be 128 lowercase hex chars".into());
    }
    let v = hex::decode(s).map_err(|e| e.to_string())?;
    v.try_into().map_err(|_| "signature length".into())
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_correlation_member_seqs(
    obj: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<(), VerifyError> {
    let members = obj
        .get("correlation")
        .and_then(|c| c.get("member_seqs"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            VerifyError::Parse(format!("correlation missing member_seqs at seq {seq}"))
        })?;
    for m in members {
        let member = m
            .as_u64()
            .ok_or_else(|| VerifyError::Parse(format!("member_seq not u64 at seq {seq}")))?;
        if member >= seq {
            return Err(VerifyError::Failed {
                failure: Failure::CorrelationMemberSeq { seq },
            });
        }
    }
    Ok(())
}

/// When observation JSONL files are present on disk, check that every published
/// `observation_digest` commits to the actual file bytes.
pub fn verify_observation_digests(
    entries: &[Value],
    observations_root: &Path,
) -> Result<(), VerifyError> {
    for entry in entries {
        let obj = entry
            .as_object()
            .ok_or_else(|| VerifyError::Parse("entry must be object".into()))?;
        if obj.get("event").and_then(|v| v.as_str()) != Some("observation_digest") {
            continue;
        }
        let seq = obj
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or(VerifyError::Failed {
                failure: Failure::MissingSeq,
            })?;
        let digest = obj.get("observation_digest").ok_or_else(|| {
            VerifyError::Parse(format!("observation_digest missing payload at seq {seq}"))
        })?;
        let files = digest
            .get("files")
            .and_then(|v| v.as_array())
            .ok_or_else(|| VerifyError::Parse(format!("observation_digest.files at seq {seq}")))?;

        let mut prev_path: Option<&str> = None;
        for f in files {
            let path = f
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| VerifyError::Parse(format!("digest file path at seq {seq}")))?;
            let expected = f
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or_else(|| VerifyError::Parse(format!("digest file sha256 at seq {seq}")))?;
            if let Some(prev) = prev_path {
                if path < prev {
                    return Err(VerifyError::Failed {
                        failure: Failure::ObservationDigestMismatch {
                            seq,
                            detail: format!("files not sorted by path ({path} after {prev})"),
                        },
                    });
                }
            }
            prev_path = Some(path);

            let abs = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                observations_root.join(path)
            };
            // Also try relative to root stripping a leading "data/observations/" prefix
            // when the digest stores repo-relative paths.
            let abs = if abs.is_file() {
                abs
            } else {
                let stripped = path.strip_prefix("data/observations/").unwrap_or(path);
                observations_root.join(stripped)
            };
            if !abs.is_file() {
                // Files absent: skip content check (operator may verify chain alone).
                continue;
            }
            let bytes = fs::read(&abs).map_err(|e| VerifyError::Io(format!("{abs:?}: {e}")))?;
            let got = hex::encode(Sha256::digest(&bytes));
            if got != expected {
                return Err(VerifyError::Failed {
                    failure: Failure::ObservationDigestMismatch {
                        seq,
                        detail: format!("{path}: expected {expected}, got {got}"),
                    },
                });
            }
        }
    }
    Ok(())
}

pub fn failure_report(failure: &Failure) -> FailureReport {
    let seq = match failure {
        Failure::EntryHashMismatch { seq }
        | Failure::PrevHashMismatch { seq }
        | Failure::SeqGap { seq }
        | Failure::SeqNotAscending { seq }
        | Failure::GenesisNotFirst { seq }
        | Failure::MissingEntryHash { seq }
        | Failure::InvalidFormatVersion { seq }
        | Failure::CorrelationMemberSeq { seq }
        | Failure::ObservationDigestMismatch { seq, .. } => Some(*seq),
        Failure::MissingSeq
        | Failure::HeadSignatureInvalid
        | Failure::HeadHashMismatch
        | Failure::HeadSeqMismatch
        | Failure::HeadPubkeyMismatch
        | Failure::HeadKeyIdMismatch => None,
        Failure::HeadSeqMissing { seq }
        | Failure::WitnessBacklog { seq }
        | Failure::EntryTooOld { seq, .. } => Some(*seq),
    };
    FailureReport {
        seq,
        check: failure.to_string(),
    }
}
