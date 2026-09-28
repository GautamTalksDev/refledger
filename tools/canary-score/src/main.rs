//! Join canary/ledger.jsonl against the published chain.
//!
//! For each performed action report: detected, classified correctly,
//! detection latency = to.first_observed − performed_at.
//! Writes p50/p95/max and misclassification count to docs/DETECTION.md.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Parser)]
struct Args {
    /// Canary ledger JSONL (from refledger/canary).
    #[arg(long)]
    ledger: PathBuf,
    /// Directory of chain JSONL day files.
    #[arg(long)]
    log_dir: PathBuf,
    /// Output markdown (default docs/DETECTION.md).
    #[arg(long, default_value = "docs/DETECTION.md")]
    out: PathBuf,
    /// Canary repo slug as it appears in log entries.
    #[arg(long, default_value = "refledger/canary")]
    canary_repo: String,
}

#[derive(Debug, Deserialize)]
struct LedgerLine {
    pattern: String,
    tag: String,
    from: String,
    to: String,
    performed_at: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let ledger = load_ledger(&args.ledger)?;
    let entries = load_entries(&args.log_dir)?;
    let mut latencies = Vec::new();
    let mut detected = 0usize;
    let mut misclassified = 0usize;
    let mut rows = Vec::new();

    for line in &ledger {
        if line.to.is_empty() {
            // delete half of delete_recreate — matched by a deletion event
            let hit = find_event(&entries, &args.canary_repo, &line.tag, "deletion", &line.performed_at);
            let ok = hit.is_some();
            if ok {
                detected += 1;
            }
            rows.push(format!(
                "| {} | {} | {} | — | delete half |",
                line.pattern,
                line.tag,
                if ok { "yes" } else { "no" }
            ));
            continue;
        }
        if line.from.is_empty() {
            // recreate half
            let hit = find_event(&entries, &args.canary_repo, &line.tag, "recreation", &line.performed_at);
            let ok = hit.is_some();
            if ok {
                detected += 1;
            }
            rows.push(format!(
                "| {} | {} | {} | — | recreate half |",
                line.pattern,
                line.tag,
                if ok { "yes" } else { "no" }
            ));
            continue;
        }

        let expected_event = match line.pattern.as_str() {
            "commit_metadata_only" => "move",
            "exact_content_change" | "floating_major_forward" | "batch_exact_to_one" => "move",
            "lightweight_annotated_roundtrip" => "move",
            "delete_recreate" => "recreation",
            _ => "move",
        };
        let hit = find_move_to(&entries, &args.canary_repo, &line.tag, &line.to, &line.performed_at);
        let (det, class_ok, latency) = match hit {
            Some(e) => {
                detected += 1;
                let event = e.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let class_ok = event == expected_event
                    || (line.pattern == "commit_metadata_only"
                        && e.get("classification").and_then(|v| v.as_str())
                            == Some("commit_metadata_only"));
                if !class_ok {
                    misclassified += 1;
                }
                let first = e
                    .get("to")
                    .and_then(|t| t.get("first_observed"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lat = latency_secs(&line.performed_at, first);
                if let Some(s) = lat {
                    latencies.push(s);
                }
                (true, class_ok, lat)
            }
            None => (false, false, None),
        };
        rows.push(format!(
            "| {} | {} | {} | {} | {} |",
            line.pattern,
            line.tag,
            if det { "yes" } else { "no" },
            if class_ok { "yes" } else { "no" },
            latency.map(|s| format!("{s}s")).unwrap_or_else(|| "—".into())
        ));
    }

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let max = latencies.last().copied();

    let md = format!(
        r#"# Detection latency

Measured by joining `refledger/canary` ledger actions against the
Refledger chain. Canary events are excluded from public ecosystem stats.

**Generated:** {}

## Summary

| Metric | Value |
|--------|-------|
| Ledger actions | {} |
| Detected | {} |
| Misclassified | {} |
| p50 latency | {} |
| p95 latency | {} |
| max latency | {} |

This is the figure OPERATIONS.md §7 promises to publish.

## Per-action

| pattern | tag | detected | classified | latency |
|---------|-----|----------|------------|---------|
{}
"#,
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".into()),
        ledger.len(),
        detected,
        misclassified,
        fmt_opt(p50),
        fmt_opt(p95),
        fmt_opt(max),
        rows.join("\n")
    );
    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.out, md)?;
    println!("wrote {}", args.out.display());
    Ok(())
}

fn fmt_opt(v: Option<i64>) -> String {
    match v {
        Some(s) => format!("{s}s"),
        None => "—".into(),
    }
}

fn percentile(sorted: &[i64], p: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (p * (sorted.len() - 1)) / 100;
    Some(sorted[idx])
}

fn latency_secs(performed: &str, first_observed: &str) -> Option<i64> {
    let a = OffsetDateTime::parse(performed, &Rfc3339).ok()?;
    let b = OffsetDateTime::parse(first_observed, &Rfc3339).ok()?;
    Some((b - a).whole_seconds().max(0))
}

fn load_ledger(path: &Path) -> Result<Vec<LedgerLine>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line)
                .with_context(|| format!("{}:{}", path.display(), i + 1))?,
        );
    }
    Ok(out)
}

fn load_entries(dir: &Path) -> Result<Vec<Value>> {
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            let p = ent.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name != "heads.jsonl" && !name.contains(".torn.") {
                    out.push(p);
                }
            }
        }
        Ok(())
    }
    walk(dir, &mut files)?;
    files.sort();
    let mut entries = Vec::new();
    for path in files {
        for (i, line) in fs::read_to_string(&path)?.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            entries.push(
                serde_json::from_str(line)
                    .with_context(|| format!("{}:{}", path.display(), i + 1))?,
            );
        }
    }
    Ok(entries)
}

fn find_move_to<'a>(
    entries: &'a [Value],
    repo: &str,
    tag: &str,
    to_commit: &str,
    after: &str,
) -> Option<&'a Value> {
    let after_t = OffsetDateTime::parse(after, &Rfc3339).ok()?;
    let ref_name = if tag.starts_with("refs/") {
        tag.to_owned()
    } else {
        format!("refs/tags/{tag}")
    };
    entries.iter().find(|e| {
        if e.get("repo").and_then(|v| v.as_str()) != Some(repo) {
            return false;
        }
        if e.get("ref").and_then(|v| v.as_str()) != Some(ref_name.as_str()) {
            return false;
        }
        let recorded = e
            .get("recorded_at")
            .and_then(|v| v.as_str())
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
        let Some(recorded) = recorded else {
            return false;
        };
        if recorded < after_t {
            return false;
        }
        e.get("to")
            .and_then(|t| t.get("commit_sha"))
            .and_then(|v| v.as_str())
            == Some(to_commit)
    })
}

fn find_event<'a>(
    entries: &'a [Value],
    repo: &str,
    tag: &str,
    event: &str,
    after: &str,
) -> Option<&'a Value> {
    let after_t = OffsetDateTime::parse(after, &Rfc3339).ok()?;
    let ref_name = format!("refs/tags/{tag}");
    entries.iter().find(|e| {
        e.get("repo").and_then(|v| v.as_str()) == Some(repo)
            && e.get("event").and_then(|v| v.as_str()) == Some(event)
            && e.get("ref").and_then(|v| v.as_str()) == Some(ref_name.as_str())
            && e.get("recorded_at")
                .and_then(|v| v.as_str())
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .is_some_and(|t| t >= after_t)
    })
}
