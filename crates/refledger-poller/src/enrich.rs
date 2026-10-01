//! Ancestry enrichment via GitHub's compare API.
//!
//! For each detected tip move, we fetch `GET /repos/{owner}/{repo}/compare/{old}...{new}`
//! and map the `status` field onto [`Ancestry`]. File counts feed optional Diff
//! blocks on Move entries.
//!
//! Enrichment is **optional**. Classification needs only peeled trees. A failed
//! compare (404 for an orphan/attack tip, 5xx, timeout, malformed body) must
//! never abort the observatory: the Move still lands with `ancestry`/`diff`
//! omitted.
//!
//! GitHub's compare endpoint returns at most 300 changed files on page 1 (documented)
//! — when the array length is exactly 300 we set `diff_possibly_truncated`.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::classify::{Ancestry, Enrichment, RepoState};
use crate::github::rest::{RestRequest, RestResponse, Transport};
use crate::observation::{Observation, Outcome, RepoSlug};

/// Documented and live-observed maximum length of `files` on a compare response.
pub const COMPARE_FILE_CAP: usize = 300;

/// Invented SHAs written by the pre-fix listing path for unpeeled annotated
/// tags. They are not git objects; comparing against them always 404s.
pub const INVENTED_ANNOTATED_COMMIT: &str = "0000000000000000000000000000000000000001";
pub const INVENTED_ANNOTATED_TREE: &str = "0000000000000000000000000000000000000002";

pub fn is_invented_placeholder(sha: &str) -> bool {
    sha == INVENTED_ANNOTATED_COMMIT || sha == INVENTED_ANNOTATED_TREE
}

/// Cached compare result. Append-only; never expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareResult {
    pub ancestry: Ancestry,
    pub files_added: u64,
    pub files_removed: u64,
    pub files_modified: u64,
    pub files_renamed: u64,
    pub paths: Vec<String>,
    pub diff_possibly_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompareRecord {
    old_sha: String,
    new_sha: String,
    result: CompareResult,
}

#[derive(Debug)]
pub struct CompareCache {
    path: PathBuf,
    entries: BTreeMap<(String, String), CompareResult>,
}

impl CompareCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EnrichError> {
        let path = path.as_ref().to_path_buf();
        let mut cache = Self {
            path,
            entries: BTreeMap::new(),
        };
        cache.load()?;
        Ok(cache)
    }

    pub fn get(&self, old: &str, new: &str) -> Option<&CompareResult> {
        self.entries.get(&(old.to_owned(), new.to_owned()))
    }

    pub fn is_possibly_truncated(files_len: usize) -> bool {
        files_len >= COMPARE_FILE_CAP
    }

    /// Insert a compare result. Append-only; used by [`enrich`] and replay seeding.
    pub fn put(&mut self, old: &str, new: &str, result: CompareResult) -> Result<(), EnrichError> {
        let record = CompareRecord {
            old_sha: old.to_owned(),
            new_sha: new.to_owned(),
            result: result.clone(),
        };
        self.append(&record)?;
        self.entries
            .insert((old.to_owned(), new.to_owned()), result);
        Ok(())
    }

    /// All cached compares — for derive tip construction on replay.
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &CompareResult)> {
        self.entries.iter()
    }

    fn load(&mut self) -> Result<(), EnrichError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(EnrichError::Io(err.to_string())),
        };
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: CompareRecord = serde_json::from_str(line)
                .map_err(|e| EnrichError::Corrupt(format!("line {}: {e}", idx + 1)))?;
            self.entries
                .insert((record.old_sha, record.new_sha), record.result);
        }
        Ok(())
    }

    fn append(&self, record: &CompareRecord) -> Result<(), EnrichError> {
        let mut line = serde_json::to_vec(record).map_err(|e| EnrichError::Serde(e.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| EnrichError::Io(e.to_string()))?;
        file.write_all(&line)
            .map_err(|e| EnrichError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| EnrichError::Io(e.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnrichError {
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
}

/// Outcome of an enrich pass: ancestry map plus an optional note when any
/// compare was skipped or failed (attached to derived Moves as
/// `detection_latency_note` — the only free-text note field format v1 allows
/// on binding events).
#[derive(Debug, Clone, Default)]
pub struct EnrichOutcome {
    pub enrichment: Enrichment,
    pub note: Option<String>,
}

/// Detect tip moves against `state` and fetch any missing compares.
///
/// Never fails the caller on a compare problem: classification only needs
/// trees. Invented placeholder SHAs from the listing bug are skipped (they
/// are not git objects). Network/protocol failures are logged and noted.
pub fn enrich<T: Transport>(
    state: &RepoState,
    observation: &Observation,
    cache: &mut CompareCache,
    transport: &T,
    token: &str,
) -> EnrichOutcome {
    let Outcome::Ok { refs, .. } = observation.outcome() else {
        return EnrichOutcome::default();
    };

    let (owner, name) = match split_slug(observation.repo()) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("enrich: skip repo={}: {e}", observation.repo().as_str());
            return EnrichOutcome {
                enrichment: Enrichment::empty(),
                note: Some(format!("enrich_failed: {e}")),
            };
        }
    };
    let mut enrichment = Enrichment::empty();
    let mut notes: Vec<String> = Vec::new();

    for r in refs {
        let (Some(new_commit), Some(_tree)) = (r.commit_sha(), r.tree_sha()) else {
            continue;
        };
        let Some(prev) = state.binding(r.name()) else {
            continue;
        };
        if prev.commit_sha() == new_commit {
            continue;
        }
        let old = prev.commit_sha();
        if is_invented_placeholder(old) || is_invented_placeholder(new_commit) {
            let msg = format!(
                "enrich_skipped: invented placeholder compare {old}...{new_commit} for {}",
                r.name()
            );
            eprintln!("{msg}");
            notes.push(msg);
            continue;
        }
        if let Some(cached) = cache.get(old, new_commit) {
            enrichment = enrichment.with_ancestry(old, new_commit, cached.ancestry);
            continue;
        }
        match fetch_compare(transport, token, &owner, &name, old, new_commit) {
            Ok(result) => {
                enrichment = enrichment.with_ancestry(old, new_commit, result.ancestry);
                if let Err(e) = cache.put(old, new_commit, result) {
                    eprintln!("enrich: cache put failed: {e}");
                    notes.push(format!("enrich_cache_failed: {e}"));
                }
            }
            Err(e) => {
                let msg =
                    format!("enrich_failed: compare {owner}/{name} {old}...{new_commit}: {e}");
                eprintln!("{msg}");
                notes.push(msg);
            }
        }
    }
    EnrichOutcome {
        enrichment,
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        },
    }
}

fn fetch_compare<T: Transport>(
    transport: &T,
    token: &str,
    owner: &str,
    name: &str,
    old: &str,
    new: &str,
) -> Result<CompareResult, EnrichError> {
    let target = format!("/repos/{owner}/{name}/compare/{old}...{new}");
    let mut headers = BTreeMap::new();
    headers.insert("authorization".into(), format!("Bearer {token}"));
    headers.insert("accept".into(), "application/vnd.github+json".into());
    headers.insert("x-github-api-version".into(), "2022-11-28".into());
    let resp = transport
        .send(&RestRequest {
            method: "GET",
            target,
            headers,
        })
        .map_err(EnrichError::Transport)?;
    parse_compare_response(&resp)
}

pub fn parse_compare_response(resp: &RestResponse) -> Result<CompareResult, EnrichError> {
    if resp.status != 200 {
        return Err(EnrichError::Protocol(format!(
            "compare status {}",
            resp.status
        )));
    }
    let body = resp
        .body
        .as_ref()
        .ok_or_else(|| EnrichError::Protocol("compare empty body".into()))?;
    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| EnrichError::Protocol("compare missing status".into()))?;
    let ancestry = match status {
        "ahead" => Ancestry::Ahead,
        "behind" => Ancestry::Behind,
        "diverged" => Ancestry::Diverged,
        "identical" => Ancestry::Identical,
        other => {
            return Err(EnrichError::Protocol(format!(
                "unknown compare status {other}"
            )))
        }
    };
    let files = body
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let diff_possibly_truncated = CompareCache::is_possibly_truncated(files.len());
    let mut files_added = 0u64;
    let mut files_removed = 0u64;
    let mut files_modified = 0u64;
    let mut files_renamed = 0u64;
    let mut paths = Vec::new();
    for f in &files {
        let status = f.get("status").and_then(|v| v.as_str()).unwrap_or("");
        match status {
            "added" => files_added += 1,
            "removed" => files_removed += 1,
            "modified" => files_modified += 1,
            "renamed" => files_renamed += 1,
            _ => files_modified += 1,
        }
        if !diff_possibly_truncated {
            if let Some(p) = f.get("filename").and_then(|v| v.as_str()) {
                paths.push(p.to_owned());
            }
        }
    }
    Ok(CompareResult {
        ancestry,
        files_added,
        files_removed,
        files_modified,
        files_renamed,
        paths,
        diff_possibly_truncated,
    })
}

fn split_slug(repo: &RepoSlug) -> Result<(String, String), EnrichError> {
    let mut parts = repo.as_str().split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) => Ok((o.to_owned(), n.to_owned())),
        _ => Err(EnrichError::Protocol(format!(
            "invalid repo slug {}",
            repo.as_str()
        ))),
    }
}

/// Build a synthetic compare JSON body for tests.
pub fn test_compare_body(status: &str, files: Vec<Value>) -> Value {
    serde_json::json!({
        "status": status,
        "files": files,
    })
}
