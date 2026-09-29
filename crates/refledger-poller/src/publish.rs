//! Publish sealed ledger files to the public git repository.
//!
//! After each seal, sealed `log/` bytes are copied into a dedicated publishing
//! clone (or the Actions checkout), committed under `data/log/`, and
//! fast-forward pushed. A git failure is recorded for the next
//! ObservationDigest note and never stops polling.
//!
//! Authentication is either a deploy key (VM) or `GITHUB_TOKEN` (Actions).
//! Never a personal access token.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use time::Date;

/// Payload for one post-seal publish attempt.
#[derive(Debug, Clone)]
pub struct LedgerPublishPayload {
    /// Sealed calendar day D (`YYYY-MM-DD` in the commit message).
    pub day: Date,
    /// Sequence of the ObservationDigest that closed day D.
    pub seq: u64,
    /// Store-relative paths under `log/` with file bytes
    /// (for example `log/2026/01/02.jsonl`, `log/heads.jsonl`).
    pub files: Vec<(String, Vec<u8>)>,
}

/// Destination for sealed ledger files in the public repository.
pub trait LedgerPublisher: Send + Sync {
    fn publish_seal(&self, payload: &LedgerPublishPayload) -> Result<(), String>;
}

/// Default: omit publishing (tests / until the publish clone is configured).
pub struct NoopPublisher;

impl LedgerPublisher for NoopPublisher {
    fn publish_seal(&self, _payload: &LedgerPublishPayload) -> Result<(), String> {
        Ok(())
    }
}

/// Copy sealed log files into a dedicated clone and fast-forward push.
///
/// The clone must already exist and track `origin` for
/// `GautamTalksDev/refledger` (or a test bare remote). The live poller data
/// directory must not be this path.
pub struct GitLedgerPublisher {
    /// Absolute path to the publishing clone (or Actions main checkout).
    pub clone_dir: PathBuf,
    /// Deploy key (write access to this repo only). Sets `GIT_SSH_COMMAND`.
    /// Omit when using [`Self::github_token`] or local file:// remotes.
    pub deploy_key: Option<PathBuf>,
    /// Actions `GITHUB_TOKEN` for HTTPS push. Never logged.
    pub github_token: Option<String>,
}

impl GitLedgerPublisher {
    pub fn new(clone_dir: impl Into<PathBuf>, deploy_key: Option<PathBuf>) -> Self {
        Self {
            clone_dir: clone_dir.into(),
            deploy_key,
            github_token: None,
        }
    }

    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self
    }

    /// `REFLEDGER_PUBLISH_CLONE` (required) plus either
    /// `REFLEDGER_PUBLISH_DEPLOY_KEY` or `GITHUB_TOKEN`.
    pub fn from_env() -> Result<Self, String> {
        let clone_dir = std::env::var("REFLEDGER_PUBLISH_CLONE")
            .map_err(|_| "REFLEDGER_PUBLISH_CLONE is not set".to_owned())?;
        let deploy_key = match std::env::var("REFLEDGER_PUBLISH_DEPLOY_KEY") {
            Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
            _ => None,
        };
        let github_token = match std::env::var("GITHUB_TOKEN") {
            Ok(t) if !t.is_empty() => Some(t),
            _ => None,
        };
        Ok(Self {
            clone_dir: PathBuf::from(clone_dir),
            deploy_key,
            github_token,
        })
    }

    fn git(&self, args: &[&str]) -> Result<std::process::Output, String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.clone_dir).args(args);
        if let Some(key) = &self.deploy_key {
            let key = key.display();
            cmd.env(
                "GIT_SSH_COMMAND",
                format!("ssh -i {key} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"),
            );
        }
        // Prefer header auth so the token never appears in the remote URL.
        if let Some(token) = &self.github_token {
            cmd.env(
                "GIT_CONFIG_COUNT",
                "1",
            );
            cmd.env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader");
            cmd.env(
                "GIT_CONFIG_VALUE_0",
                format!("AUTHORIZATION: bearer {token}"),
            );
        }
        cmd.output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))
    }

    fn git_ok(&self, args: &[&str]) -> Result<String, String> {
        let out = self.git(args)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(format!(
                "git {} failed: {stderr}{stdout}",
                args.join(" ")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Refuse if any path outside `data/log/` is dirty.
    fn refuse_if_dirty_outside_log(&self) -> Result<(), String> {
        let porcelain = self.git_ok(&["status", "--porcelain"])?;
        for line in porcelain.lines() {
            let Some(path) = porcelain_path(line) else {
                continue;
            };
            if !path.starts_with("data/log/") && path != "data/log" {
                return Err(format!(
                    "publish clone has changes outside data/log/: {path}"
                ));
            }
        }
        Ok(())
    }

    fn write_files(&self, files: &[(String, Vec<u8>)]) -> Result<(), String> {
        for (rel, bytes) in files {
            let under_log = rel
                .strip_prefix("log/")
                .ok_or_else(|| format!("publish path must be under log/: {rel}"))?;
            let dest = self.clone_dir.join("data/log").join(under_log);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&dest)
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            f.write_all(bytes)
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            f.sync_all()
                .map_err(|e| format!("fsync {}: {e}", dest.display()))?;
        }
        Ok(())
    }
}

impl LedgerPublisher for GitLedgerPublisher {
    fn publish_seal(&self, payload: &LedgerPublishPayload) -> Result<(), String> {
        if !self.clone_dir.join(".git").exists() {
            return Err(format!(
                "publish clone missing at {}",
                self.clone_dir.display()
            ));
        }
        self.refuse_if_dirty_outside_log()?;
        self.write_files(&payload.files)?;
        self.refuse_if_dirty_outside_log()?;

        self.git_ok(&["add", "--", "data/log"])?;
        let staged = self.git_ok(&["diff", "--cached", "--name-only"])?;
        for path in staged.lines() {
            let path = path.trim();
            if path.is_empty() {
                continue;
            }
            if !path.starts_with("data/log/") {
                return Err(format!(
                    "refusing to commit path outside data/log/: {path}"
                ));
            }
        }
        if staged.trim().is_empty() {
            // Idempotent retry: already published this tip.
            return Ok(());
        }

        let day = format!(
            "{:04}-{:02}-{:02}",
            payload.day.year(),
            u8::from(payload.day.month()),
            payload.day.day()
        );
        let msg = format!("ledger: seal {day} seq {}", payload.seq);
        self.git_ok(&[
            "-c",
            "user.email=refledger-publish@users.noreply.github.com",
            "-c",
            "user.name=refledger-publish",
            "commit",
            "-m",
            &msg,
        ])?;

        // Fast-forward only. Never --force.
        let push = self.git(&["push", "origin", "HEAD:main"])?;
        if !push.status.success() {
            let stderr = String::from_utf8_lossy(&push.stderr);
            let stdout = String::from_utf8_lossy(&push.stdout);
            return Err(format!("fast-forward push rejected: {stderr}{stdout}"));
        }
        Ok(())
    }
}

fn porcelain_path(line: &str) -> Option<&str> {
    let line = line.trim_end();
    if line.len() < 3 {
        return None;
    }
    // XY (2 chars) then whitespace then path; renames use `old -> new`.
    let rest = line[2..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let path = rest
        .rsplit_once(" -> ")
        .map(|(_, new)| new)
        .unwrap_or(rest)
        .trim()
        .trim_matches('"');
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Persistent queue of publish failures awaiting a digest note.
#[derive(Debug, Clone)]
pub struct PublishFailure {
    pub day: String,
    pub error: String,
}

pub fn format_publish_failure_note(failure: &PublishFailure) -> String {
    format!(
        "ledger publish failed for {}: {}",
        failure.day, failure.error
    )
}

/// Map a store-relative `log/…` path into the public tree `data/log/…`.
pub fn public_log_path(store_rel: &str) -> Option<PathBuf> {
    store_rel
        .strip_prefix("log/")
        .map(|rest| Path::new("data/log").join(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_handles_rename() {
        assert_eq!(
            porcelain_path("R  data/log/a.jsonl -> data/log/b.jsonl"),
            Some("data/log/b.jsonl")
        );
    }
}

