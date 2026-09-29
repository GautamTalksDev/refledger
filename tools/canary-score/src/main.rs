//! Join a canary ledger (local path or URL) against the published chain.
//!
//! Only scores actions the poller could have seen. See `refledger_canary_score`.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use time::{OffsetDateTime, UtcOffset};
use refledger_canary_score::{load_entries, load_gaps, load_ledger, render_markdown, score};

#[derive(Debug, Parser)]
struct Args {
    /// Canary ledger JSONL: local path or https URL
    /// (GautamTalksDev/canary `canary/ledger.jsonl`).
    #[arg(long)]
    ledger: String,
    /// Directory of chain JSONL day files.
    #[arg(long)]
    log_dir: PathBuf,
    /// Observation JSONL root (for PollerDown / SecondaryLimitBackoff gaps).
    /// Defaults to `<log_dir>/../observations` when that directory exists.
    #[arg(long)]
    observations: Option<PathBuf>,
    /// Output markdown (default docs/DETECTION.md).
    #[arg(long, default_value = "docs/DETECTION.md")]
    out: PathBuf,
    /// Canary repo slug as it appears in log entries.
    #[arg(long, default_value = "GautamTalksDev/canary")]
    canary_repo: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let ledger = load_ledger(&args.ledger)?;
    let entries = load_entries(&args.log_dir)?;
    let obs_root = args.observations.unwrap_or_else(|| {
        args.log_dir
            .parent()
            .map(|p| p.join("observations"))
            .unwrap_or_else(|| PathBuf::from("observations"))
    });
    let gaps = load_gaps(&obs_root, &args.canary_repo)?;
    let report = score(
        &ledger,
        &entries,
        &gaps,
        &args.canary_repo,
        normalize_wall_clock(OffsetDateTime::now_utc()),
    );
    let md = render_markdown(&report);
    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.out, md)?;
    println!("wrote {}", args.out.display());
    println!(
        "scored={} pre_genesis={} during_gap={} detected={}",
        report.scored.len(),
        report.pre_genesis.len(),
        report.during_gap.len(),
        report.detected
    );
    Ok(())
}

/// Same contract as `refledger_log::normalize_to_utc_millis` (standalone tool).
fn normalize_wall_clock(t: OffsetDateTime) -> OffsetDateTime {
    let utc = t.to_offset(UtcOffset::UTC);
    let floored = (utc.nanosecond() / 1_000_000) * 1_000_000;
    utc.replace_nanosecond(floored)
        .expect("millisecond-aligned nanosecond is always valid")
}
