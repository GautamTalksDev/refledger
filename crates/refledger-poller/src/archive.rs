//! Optional extra mirror of sealed observation files.
//!
//! Heads and the chain live in git; observation JSONL is published on the
//! `data` branch with each seal. This module is only for an *additional*
//! local mirror (second disk / NFS). There is no object-store uploader.
//! A failed mirror upload is recorded and surfaces in the *next*
//! ObservationDigest note — never silently dropped.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use time::Date;

/// One sealed day's observation files, ready to leave the VM.
#[derive(Debug, Clone)]
pub struct DayArchive {
    pub day: Date,
    /// Relative paths under the data root, with file bytes.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Destination for sealed observation files.
pub trait ObservationArchive: Send + Sync {
    fn upload_day(&self, archive: &DayArchive) -> Result<(), String>;
}

/// Default: observations are published on the data branch; no extra mirror.
pub struct NoopArchive;

impl ObservationArchive for NoopArchive {
    fn upload_day(&self, _archive: &DayArchive) -> Result<(), String> {
        Ok(())
    }
}

/// Copy each file under `root/{YYYY}/{MM}/{DD}/…` — useful when a second disk
/// or NFS mount is an extra off-host destination beside the data branch.
pub struct MirrorArchive {
    root: PathBuf,
}

impl MirrorArchive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ObservationArchive for MirrorArchive {
    fn upload_day(&self, archive: &DayArchive) -> Result<(), String> {
        let day = archive.day;
        let prefix = self.root.join(format!(
            "{:04}/{:02}/{:02}",
            day.year(),
            u8::from(day.month()),
            day.day()
        ));
        for (rel, bytes) in &archive.files {
            let name = Path::new(rel)
                .file_name()
                .ok_or_else(|| format!("archive path has no file name: {rel}"))?;
            let dest = prefix.join(name);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&dest)
                .map_err(|e| e.to_string())?;
            f.write_all(bytes).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Persistent queue of archive failures awaiting a digest note.
#[derive(Debug, Clone)]
pub struct ArchiveFailure {
    pub day: String,
    pub error: String,
}

pub fn format_archive_failure_note(failure: &ArchiveFailure) -> String {
    format!(
        "observation archive upload failed for {}: {}",
        failure.day, failure.error
    )
}
