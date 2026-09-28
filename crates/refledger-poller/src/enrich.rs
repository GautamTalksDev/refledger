//! Enrichment between observe and classify.
//!
//! Moves are rare relative to the poll budget: the steady state is 304s on tag
//! listings. This module is the **only** network-touching step between a raw
//! [`Observation`] and pure [`crate::classify::classify`]. Do not fold these
//! fetches into classify later as an "optimisation" — that would destroy
//! replayability.
//!
//! For each detected tip move, we fetch `GET /repos/{owner}/{repo}/compare/{old}...{new}`
//! once. Results are cached by `(old, new)` forever — the same content-addressed
//! immutability argument as [`crate::github::rest::ObjectCache`].
//!
//! # Compare file cap
//!
//! GitHub's compare endpoint returns at most 300 changed files on page 1 (documented)
//! and silently truncates beyond that — there is no top-level `truncated` field.
//! Live check 2026-09-28 (`torvalds/linux`, `v5.0...v6.0`) returned exactly 300
//! files. [`COMPARE_FILE_CAP`] records that observed cap; when
//! `files.len() >= COMPARE_FILE_CAP` we set `diff_possibly_truncated: true`
//! because we cannot distinguish "exactly 300 files changed" from "truncated
//! at 300". That precision is the whole point of the flag's name.

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
///
/// Encoded from docs ("up to 300 changed files") and a 2026-09-28 capture of
/// `torvalds/linux` `v5.0...v6.0` which returned `files_len == 300`.
pub const COMPARE_FILE_CAP: usize = 300;

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

/// Persist forever: `(old_sha, new_sha) -> CompareResult`.
#[derive(Debug)]
pub struct CompareCache {
    path: PathBuf,
    entries: BTreeMap<(String, String), CompareResult>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompareRecord {
    old_sha: String,
    new_sha: String,
    result: CompareResult,
}

impl CompareCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EnrichError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| EnrichError::Io(e.to_string()))?;
            }
        }
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

/// Detect tip moves against `state` and fetch any missing compares.
///
/// Returns an [`Enrichment`] suitable for [`crate::classify::classify`]. Does
/// nothing (and costs nothing) when the observation is not `Ok` or no tips moved.
pub fn enrich<T: Transport>(
    state: &RepoState,
    observation: &Observation,
    cache: &mut CompareCache,
    transport: &T,
    token: &str,
) -> Result<Enrichment, EnrichError> {
    let Outcome::Ok { refs, .. } = observation.outcome() else {
        return Ok(Enrichment::empty());
    };

    let (owner, name) = split_slug(observation.repo())?;
    let mut enrichment = Enrichment::empty();

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
        if let Some(cached) = cache.get(old, new_commit) {
            enrichment = enrichment.with_ancestry(old, new_commit, cached.ancestry);
            continue;
        }
        let result = fetch_compare(transport, token, &owner, &name, old, new_commit)?;
        enrichment = enrichment.with_ancestry(old, new_commit, result.ancestry);
        cache.put(old, new_commit, result)?;
    }
    Ok(enrichment)
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
        _ => Err(EnrichError::Protocol("invalid repo slug".into())),
    }
}

/// Build a synthetic compare JSON body for tests.
pub fn test_compare_body(status: &str, files: Vec<Value>) -> Value {
    serde_json::json!({
        "status": status,
        "ahead_by": 0,
        "behind_by": 0,
        "total_commits": 1,
        "files": files,
    })
}
