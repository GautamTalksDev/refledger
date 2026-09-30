//! Score canary ledger actions against the published chain.
//!
//! Only actions the poller was in a position to detect are scored: after the
//! canary's PopulationChange Added entry, and outside recorded PollerDown /
//! SecondaryLimitBackoff gaps for that poll group.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Deserialize)]
pub struct LedgerLine {
    pub pattern: String,
    pub tag: String,
    pub from: String,
    pub to: String,
    pub performed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    PollerDown,
    SecondaryLimitBackoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedGap {
    pub kind: GapKind,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    /// Before the canary PopulationChange Added entry (or no such entry yet).
    PreGenesis,
    /// performed_at falls inside a recorded skip gap for the canary group.
    DuringGap { kind: GapKind },
    /// Eligible to score against the chain.
    Scorable,
}

#[derive(Debug, Clone)]
pub struct ScoreReport {
    pub generated_at: String,
    pub canary_repo: String,
    pub canary_added_at: Option<String>,
    pub scored: Vec<ScoredRow>,
    pub creation_only: Vec<LedgerLine>,
    pub pre_genesis: Vec<LedgerLine>,
    pub during_gap: Vec<(LedgerLine, GapKind)>,
    pub detected: usize,
    pub misclassified: usize,
    pub latencies: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct ScoredRow {
    pub line: LedgerLine,
    pub detected: bool,
    pub classified_ok: bool,
    pub latency_secs: Option<i64>,
    pub note: String,
}

/// Patterns expected to produce a Move/Deletion/Recreation chain event.
pub fn pattern_expects_chain_event(pattern: &str) -> bool {
    matches!(
        pattern,
        "floating_major_forward"
            | "exact_content_change"
            | "commit_metadata_only"
            | "lightweight_to_annotated"
            | "annotated_to_lightweight"
            | "lightweight_annotated_roundtrip"
            | "delete"
            | "recreate"
            | "delete_recreate"
            | "batch_exact_to_one"
    )
}

/// Creation-only ledger rows (bootstrap / first-seen) are reported separately
/// and never counted as detection misses.
pub fn pattern_is_creation_only(pattern: &str) -> bool {
    matches!(pattern, "bootstrap" | "creation" | "create")
}

pub fn score(
    ledger: &[LedgerLine],
    entries: &[Value],
    gaps: &[RecordedGap],
    canary_repo: &str,
    now: OffsetDateTime,
) -> ScoreReport {
    let added_at = canary_added_at(entries, canary_repo);
    let mut scored = Vec::new();
    let mut creation_only = Vec::new();
    let mut pre_genesis = Vec::new();
    let mut during_gap = Vec::new();
    let mut detected = 0usize;
    let mut misclassified = 0usize;
    let mut latencies = Vec::new();

    for line in ledger {
        if pattern_is_creation_only(&line.pattern) || !pattern_expects_chain_event(&line.pattern) {
            creation_only.push(line.clone());
            continue;
        }

        match classify_eligibility(line, added_at, gaps) {
            Eligibility::PreGenesis => {
                pre_genesis.push(line.clone());
                continue;
            }
            Eligibility::DuringGap { kind } => {
                during_gap.push((line.clone(), kind));
                continue;
            }
            Eligibility::Scorable => {}
        }

        let row = score_line(line, entries, canary_repo);
        if row.detected {
            detected += 1;
        }
        if row.detected && !row.classified_ok {
            misclassified += 1;
        }
        if let Some(s) = row.latency_secs {
            latencies.push(s);
        }
        scored.push(row);
    }

    latencies.sort_unstable();
    ScoreReport {
        generated_at: now.format(&Rfc3339).unwrap_or_else(|_| "unknown".into()),
        canary_repo: canary_repo.to_owned(),
        canary_added_at: added_at.map(|t| t.format(&Rfc3339).unwrap_or_default()),
        scored,
        creation_only,
        pre_genesis,
        during_gap,
        detected,
        misclassified,
        latencies,
    }
}

pub fn classify_eligibility(
    line: &LedgerLine,
    canary_added_at: Option<OffsetDateTime>,
    gaps: &[RecordedGap],
) -> Eligibility {
    let Some(performed) = parse_time(&line.performed_at) else {
        // Unparseable timestamps cannot be scored honestly.
        return Eligibility::PreGenesis;
    };
    match canary_added_at {
        None => return Eligibility::PreGenesis,
        Some(added) if performed <= added => return Eligibility::PreGenesis,
        Some(_) => {}
    }
    for gap in gaps {
        if performed >= gap.from && performed <= gap.to {
            return Eligibility::DuringGap { kind: gap.kind };
        }
    }
    Eligibility::Scorable
}

pub fn canary_added_at(entries: &[Value], repo: &str) -> Option<OffsetDateTime> {
    let mut best: Option<OffsetDateTime> = None;
    for e in entries {
        if e.get("event").and_then(|v| v.as_str()) != Some("population_change") {
            continue;
        }
        if e.get("repo").and_then(|v| v.as_str()) != Some(repo) {
            continue;
        }
        let change = e
            .get("population_change")
            .and_then(|p| p.get("change"))
            .and_then(|v| v.as_str());
        if change != Some("added") {
            continue;
        }
        let Some(t) = e
            .get("recorded_at")
            .and_then(|v| v.as_str())
            .and_then(parse_time)
        else {
            continue;
        };
        best = Some(match best {
            Some(prev) if prev <= t => prev,
            _ => t,
        });
    }
    best
}

/// Collect PollerDown and SecondaryLimitBackoff gaps for `repo` from observation JSONL.
pub fn load_gaps(observations_root: &Path, repo: &str) -> Result<Vec<RecordedGap>> {
    if !observations_root.is_dir() {
        return Ok(Vec::new());
    }
    let segment = repo.replace('/', "--");
    let suffix = format!("/{segment}.jsonl");
    let mut files = Vec::new();
    walk_jsonl(observations_root, &mut files)?;
    files.retain(|p| {
        p.to_string_lossy().ends_with(&suffix) && !p.to_string_lossy().contains(".torn.")
    });
    files.sort();

    let mut skips: Vec<(OffsetDateTime, GapKind, Option<(OffsetDateTime, OffsetDateTime)>)> =
        Vec::new();
    let mut all_times: Vec<OffsetDateTime> = Vec::new();

    for path in &files {
        for (i, line) in fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?
            .lines()
            .enumerate()
        {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .with_context(|| format!("{}:{}", path.display(), i + 1))?;
            let Some(at) = value
                .get("observed_at")
                .and_then(|v| v.as_str())
                .and_then(parse_time)
            else {
                continue;
            };
            all_times.push(at);
            let Some(outcome) = value.get("outcome") else {
                continue;
            };
            if outcome.get("type").and_then(|v| v.as_str()) != Some("skipped") {
                continue;
            }
            match parse_skip_reason(outcome.get("reason")) {
                Some(SkipParsed::PollerDown { from, to }) => {
                    skips.push((at, GapKind::PollerDown, Some((from, to))));
                }
                Some(SkipParsed::SecondaryLimitBackoff) => {
                    skips.push((at, GapKind::SecondaryLimitBackoff, None));
                }
                None => {}
            }
        }
    }
    all_times.sort_unstable();

    let mut gaps = Vec::new();
    for (at, kind, range) in skips {
        match kind {
            GapKind::PollerDown => {
                if let Some((from, to)) = range {
                    gaps.push(RecordedGap { kind, from, to });
                }
            }
            GapKind::SecondaryLimitBackoff => {
                // Cover from this skip until the next observation for the repo
                // (exclusive end becomes inclusive by using next, or `at` alone).
                let to = all_times
                    .iter()
                    .copied()
                    .find(|t| *t > at)
                    .unwrap_or(at);
                gaps.push(RecordedGap {
                    kind,
                    from: at,
                    to,
                });
            }
        }
    }
    gaps.sort_by_key(|g| g.from);
    Ok(gaps)
}

enum SkipParsed {
    PollerDown {
        from: OffsetDateTime,
        to: OffsetDateTime,
    },
    SecondaryLimitBackoff,
}

fn parse_skip_reason(reason: Option<&Value>) -> Option<SkipParsed> {
    let reason = reason?;
    if let Some(s) = reason.as_str() {
        return match s {
            "secondary_limit_backoff" => Some(SkipParsed::SecondaryLimitBackoff),
            "poller_down" => None, // malformed without range
            _ => None,
        };
    }
    if let Some(obj) = reason.as_object() {
        if let Some(pd) = obj.get("poller_down") {
            let from = parse_time(pd.get("from")?.as_str()?)?;
            let to = parse_time(pd.get("to")?.as_str()?)?;
            return Some(SkipParsed::PollerDown { from, to });
        }
        if obj.contains_key("secondary_limit_backoff") {
            return Some(SkipParsed::SecondaryLimitBackoff);
        }
    }
    None
}

fn score_line(line: &LedgerLine, entries: &[Value], canary_repo: &str) -> ScoredRow {
    if line.to.is_empty() {
        let hit = find_event(entries, canary_repo, &line.tag, "deletion", &line.performed_at);
        let ok = hit.is_some();
        return ScoredRow {
            line: line.clone(),
            detected: ok,
            classified_ok: ok,
            latency_secs: None,
            note: "delete half".into(),
        };
    }
    if line.from.is_empty() {
        let hit = find_event(
            entries,
            canary_repo,
            &line.tag,
            "recreation",
            &line.performed_at,
        );
        let ok = hit.is_some();
        return ScoredRow {
            line: line.clone(),
            detected: ok,
            classified_ok: ok,
            latency_secs: None,
            note: "recreate half".into(),
        };
    }

    let expected_event = match line.pattern.as_str() {
        "commit_metadata_only" => "move",
        "exact_content_change" | "floating_major_forward" | "batch_exact_to_one" => "move",
        "lightweight_annotated_roundtrip"
        | "lightweight_to_annotated"
        | "annotated_to_lightweight" => "move",
        "delete_recreate" | "recreate" => "recreation",
        "delete" => "deletion",
        _ => "move",
    };
    let hit = find_move_to(
        entries,
        canary_repo,
        &line.tag,
        &line.to,
        &line.performed_at,
    );
    match hit {
        Some(e) => {
            let event = e.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let class_ok = event == expected_event
                || (line.pattern == "commit_metadata_only"
                    && e.get("classification").and_then(|v| v.as_str())
                        == Some("commit_metadata_only"));
            let first = e
                .get("to")
                .and_then(|t| t.get("first_observed"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lat = latency_secs(&line.performed_at, first);
            ScoredRow {
                line: line.clone(),
                detected: true,
                classified_ok: class_ok,
                latency_secs: lat,
                note: String::new(),
            }
        }
        None => ScoredRow {
            line: line.clone(),
            detected: false,
            classified_ok: false,
            latency_secs: None,
            note: String::new(),
        },
    }
}

pub fn render_markdown(report: &ScoreReport) -> String {
    let mut latencies = report.latencies.clone();
    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let max = latencies.last().copied();

    let mut rows = Vec::new();
    for row in &report.scored {
        rows.push(format!(
            "| {} | {} | {} | {} | {} |",
            row.line.pattern,
            row.line.tag,
            if row.detected { "yes" } else { "no" },
            if row.note.is_empty() {
                if row.classified_ok { "yes" } else { "no" }
            } else {
                "—"
            },
            if !row.note.is_empty() {
                row.note.clone()
            } else {
                row.latency_secs
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "—".into())
            }
        ));
    }

    let added = report
        .canary_added_at
        .as_deref()
        .unwrap_or("(not yet in chain)");

    format!(
        r#"# Detection latency

Measured by joining `{repo}` ledger actions against the Refledger chain.
Only actions the poller could have seen are scored (after the canary's
PopulationChange Added at {added}; outside recorded PollerDown /
SecondaryLimitBackoff gaps). Canary events are excluded from public
ecosystem stats.

**Generated:** {generated}

## Summary

| Metric | Value |
|--------|-------|
| Ledger actions | {total} |
| Scored (event-producing) | {scored} |
| Creation-only, not scored | {creation} |
| Pre genesis, not scored | {pre} |
| Performed during a recorded gap | {gap} |
| Detected | {detected} |
| Misclassified | {mis} |
| p50 latency | {p50} |
| p95 latency | {p95} |
| max latency | {max} |

This is the figure OPERATIONS.md §7 promises to publish.

## Per-action (scored)

| pattern | tag | detected | classified | latency |
|---------|-----|----------|------------|---------|
{rows}

## Creation-only (not scored)

{creation_n} ledger row(s) whose pattern is not expected to produce a
Move/Deletion/Recreation (bootstrap / first-seen create). Reported separately
so they are never counted as detection misses.

| pattern | tag | performed_at |
|---------|-----|--------------|
{creation_rows}

## Pre genesis, not scored

{pre_n} ledger row(s) with `performed_at` at or before the canary PopulationChange Added entry (or before that entry exists). They are listed so a reader can see where they went; they are not detection misses.

| pattern | tag | performed_at |
|---------|-----|--------------|
{pre_rows}

## Performed during a recorded gap

{gap_n} ledger row(s) whose `performed_at` falls inside a recorded PollerDown or SecondaryLimitBackoff gap for the canary poll group. Excluded from scoring is not the same as hidden.

| pattern | tag | performed_at | gap |
|---------|-----|--------------|-----|
{gap_rows}
"#,
        repo = report.canary_repo,
        added = added,
        generated = report.generated_at,
        total = report.scored.len()
            + report.creation_only.len()
            + report.pre_genesis.len()
            + report.during_gap.len(),
        scored = report.scored.len(),
        creation = report.creation_only.len(),
        pre = report.pre_genesis.len(),
        gap = report.during_gap.len(),
        detected = report.detected,
        mis = report.misclassified,
        p50 = fmt_opt(p50),
        p95 = fmt_opt(p95),
        max = fmt_opt(max),
        rows = if rows.is_empty() {
            "| — | — | — | — | — |".into()
        } else {
            rows.join("\n")
        },
        creation_n = report.creation_only.len(),
        creation_rows = if report.creation_only.is_empty() {
            "| — | — | — |".into()
        } else {
            report
                .creation_only
                .iter()
                .map(|l| format!("| {} | {} | {} |", l.pattern, l.tag, l.performed_at))
                .collect::<Vec<_>>()
                .join("\n")
        },
        pre_n = report.pre_genesis.len(),
        pre_rows = if report.pre_genesis.is_empty() {
            "| — | — | — |".into()
        } else {
            report
                .pre_genesis
                .iter()
                .map(|l| format!("| {} | {} | {} |", l.pattern, l.tag, l.performed_at))
                .collect::<Vec<_>>()
                .join("\n")
        },
        gap_n = report.during_gap.len(),
        gap_rows = if report.during_gap.is_empty() {
            "| — | — | — | — |".into()
        } else {
            report
                .during_gap
                .iter()
                .map(|(l, k)| {
                    let kind = match k {
                        GapKind::PollerDown => "PollerDown",
                        GapKind::SecondaryLimitBackoff => "SecondaryLimitBackoff",
                    };
                    format!("| {} | {} | {} | {} |", l.pattern, l.tag, l.performed_at, kind)
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}

pub fn load_ledger(source: &str) -> Result<Vec<LedgerLine>> {
    let text = read_ledger_text(source)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).with_context(|| format!("{source}:{}", i + 1))?);
    }
    Ok(out)
}

fn read_ledger_text(source: &str) -> Result<String> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let response = ureq::get(source)
            .call()
            .with_context(|| format!("GET {source}"))?;
        let status = response.status();
        if !(200..300).contains(&status) {
            bail!("GET {source}: HTTP {status}");
        }
        return response
            .into_string()
            .with_context(|| format!("read body {source}"));
    }
    fs::read_to_string(source).with_context(|| format!("read {source}"))
}

pub fn load_entries(dir: &Path) -> Result<Vec<Value>> {
    let mut files = Vec::new();
    walk_jsonl(dir, &mut files)?;
    files.retain(|p| {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        name != "heads.jsonl" && !name.contains(".torn.") && name.ends_with(".jsonl")
    });
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

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            walk_jsonl(&p, out)?;
        } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
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
    let a = parse_time(performed)?;
    let b = parse_time(first_observed)?;
    Some((b - a).whole_seconds().max(0))
}

fn parse_time(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

fn find_move_to<'a>(
    entries: &'a [Value],
    repo: &str,
    tag: &str,
    to_commit: &str,
    after: &str,
) -> Option<&'a Value> {
    let after_t = parse_time(after)?;
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
        let Some(recorded) = e
            .get("recorded_at")
            .and_then(|v| v.as_str())
            .and_then(parse_time)
        else {
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
    let after_t = parse_time(after)?;
    let ref_name = format!("refs/tags/{tag}");
    entries.iter().find(|e| {
        e.get("repo").and_then(|v| v.as_str()) == Some(repo)
            && e.get("event").and_then(|v| v.as_str()) == Some(event)
            && e.get("ref").and_then(|v| v.as_str()) == Some(ref_name.as_str())
            && e.get("recorded_at")
                .and_then(|v| v.as_str())
                .and_then(parse_time)
                .is_some_and(|t| t >= after_t)
    })
}
