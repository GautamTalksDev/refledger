//! Off-VM observation archive.
//!
//! Heads and the chain live in git; observation JSONL lives on the poller disk.
//! Losing that disk makes byte-for-byte replay permanently unverifiable for
//! every sealed day before the loss. Each seal therefore ships that day's
//! observation files off-VM. A failed upload is recorded and surfaces in the
//! *next* ObservationDigest note — never silently dropped.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::Date;

type HmacSha256 = Hmac<Sha256>;

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

/// Test double / local-only deployments. Always succeeds and retains nothing.
pub struct NoopArchive;

impl ObservationArchive for NoopArchive {
    fn upload_day(&self, _archive: &DayArchive) -> Result<(), String> {
        Ok(())
    }
}

/// Copy each file under `root/{YYYY}/{MM}/{DD}/…` — useful when a second disk
/// or NFS mount is the off-VM destination.
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

/// Cloudflare R2 (S3-compatible) PutObject uploader.
///
/// Env (read by [`R2Archive::from_env`]):
/// - `REFLEDGER_R2_ACCOUNT_ID`
/// - `REFLEDGER_R2_ACCESS_KEY_ID`
/// - `REFLEDGER_R2_SECRET_ACCESS_KEY`
/// - `REFLEDGER_R2_BUCKET`
/// - `REFLEDGER_R2_PREFIX` (optional; default `observations`)
pub struct R2Archive {
    account_id: String,
    access_key_id: String,
    secret_access_key: String,
    bucket: String,
    prefix: String,
}

impl R2Archive {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            account_id: env_required("REFLEDGER_R2_ACCOUNT_ID")?,
            access_key_id: env_required("REFLEDGER_R2_ACCESS_KEY_ID")?,
            secret_access_key: env_required("REFLEDGER_R2_SECRET_ACCESS_KEY")?,
            bucket: env_required("REFLEDGER_R2_BUCKET")?,
            prefix: std::env::var("REFLEDGER_R2_PREFIX")
                .unwrap_or_else(|_| "observations".to_owned()),
        })
    }

    fn endpoint(&self) -> String {
        format!("https://{}.r2.cloudflarestorage.com", self.account_id)
    }

    fn object_key(&self, day: Date, rel: &str) -> String {
        let name = Path::new(rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rel);
        format!(
            "{}/{:04}/{:02}/{:02}/{}",
            self.prefix.trim_matches('/'),
            day.year(),
            u8::from(day.month()),
            day.day(),
            name
        )
    }
}

impl ObservationArchive for R2Archive {
    fn upload_day(&self, archive: &DayArchive) -> Result<(), String> {
        for (rel, bytes) in &archive.files {
            let key = self.object_key(archive.day, rel);
            put_object(self, &key, bytes)?;
        }
        Ok(())
    }
}

fn env_required(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn put_object(cfg: &R2Archive, key: &str, body: &[u8]) -> Result<(), String> {
    let host = format!("{}.r2.cloudflarestorage.com", cfg.account_id);
    let url = format!("{}/{}/{}", cfg.endpoint(), cfg.bucket, key);
    let now = refledger_log::normalize_to_utc_millis(time::OffsetDateTime::now_utc());
    let date = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let amz_date = format!(
        "{}T{:02}{:02}{:02}Z",
        date,
        now.hour(),
        now.minute(),
        now.second()
    );
    let payload_hash = hex::encode(Sha256::digest(body));
    let region = "auto";
    let service = "s3";
    let credential_scope = format!("{date}/{region}/{service}/aws4_request");
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "PUT\n/{}/{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        cfg.bucket, key
    );
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_hash}");
    let signing_key = aws4_signing_key(&cfg.secret_access_key, &date, region, service)?;
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        cfg.access_key_id
    );

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build();
    let response = agent
        .put(&url)
        .set("Host", &host)
        .set("x-amz-content-sha256", &payload_hash)
        .set("x-amz-date", &amz_date)
        .set("Authorization", &authorization)
        .set("Content-Type", "application/x-ndjson")
        .set("Content-Length", &body.len().to_string())
        .send_bytes(body)
        .map_err(|e| format!("R2 PUT {key}: {e}"))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        let text = response.into_string().unwrap_or_default();
        return Err(format!("R2 PUT {key}: HTTP {status}: {text}"));
    }
    Ok(())
}

fn aws4_signing_key(
    secret: &str,
    date: &str,
    region: &str,
    service: &str,
) -> Result<Vec<u8>, String> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| format!("hmac key error: {e}"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
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
