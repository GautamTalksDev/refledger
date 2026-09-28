//! GitHub REST secondary rate-limit calibration.
//!
//! Answers whether the undocumented 900-points-per-minute REST secondary limit
//! buckets per route TEMPLATE or per full URL — the question that decides M1's
//! scheduler architecture.
//!
//! Mode A: conditional GETs across N different repos at a ramping rate.
//! Mode B: the same volume against one repo repeatedly.
//!
//! Honour Retry-After. Stop the ramp on the first 429. We are measuring
//! GitHub's limits as a guest; OPERATIONS.md is already published.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, IF_NONE_MATCH, USER_AGENT};
use reqwest::StatusCode;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// Conditional GETs across N different repos (template vs URL question).
    A,
    /// Same volume against one repo repeatedly.
    B,
    /// Run A then B and write the comparison report.
    Both,
}

#[derive(Debug, Parser)]
#[command(
    name = "refledger-calibrate",
    about = "Measure GitHub REST secondary rate-limit bucketing (template vs URL)"
)]
struct Args {
    /// GitHub token (classic or fine-grained). Prefer env REFLEDGER_CALIBRATE_TOKEN.
    #[arg(long, env = "REFLEDGER_CALIBRATE_TOKEN")]
    token: String,

    /// File of owner/repo slugs, one per line. Required for Mode A; Mode B uses the first.
    #[arg(long)]
    repos: PathBuf,

    /// Which mode to run.
    #[arg(long, value_enum, default_value = "both")]
    mode: Mode,

    /// Starting request rate (requests per minute).
    #[arg(long, default_value_t = 100)]
    ramp_start: u32,

    /// Ending request rate (requests per minute). Stop earlier on first 429.
    #[arg(long, default_value_t = 1200)]
    ramp_end: u32,

    /// Rate step between ramp levels.
    #[arg(long, default_value_t = 100)]
    ramp_step: u32,

    /// How many seconds to hold each ramp level before stepping up.
    #[arg(long, default_value_t = 60)]
    hold_secs: u64,

    /// Directory for raw CSV and the written CALIBRATION.md (repo-relative recommended).
    #[arg(long, default_value = "tools/calibrate/data")]
    out_dir: PathBuf,

    /// Path to write docs/CALIBRATION.md (relative to cwd, typically repo root).
    #[arg(long, default_value = "docs/CALIBRATION.md")]
    report: PathBuf,

    /// Token type label recorded in the report (e.g. classic-pat, fine-grained, github-app).
    #[arg(long, default_value = "classic-pat")]
    token_type: String,

    /// User-Agent identifying this instrument (OPERATIONS.md style).
    #[arg(long, default_value = "refledger-calibrate/0.1 (+https://raw.githubusercontent.com/GautamTalksDev/refledger/main/OPERATIONS.md)")]
    user_agent: String,
}

#[derive(Debug, Clone, Serialize)]
struct Sample {
    mode: String,
    ramp_rpm: u32,
    timestamp: String,
    repo: String,
    status: u16,
    x_ratelimit_remaining: Option<String>,
    x_ratelimit_used: Option<String>,
    x_ratelimit_limit: Option<String>,
    x_ratelimit_resource: Option<String>,
    retry_after: Option<String>,
    etag: Option<String>,
    latency_ms: u128,
}

#[derive(Debug, Clone)]
struct ModeResult {
    mode: String,
    first_throttle_rpm: Option<u32>,
    first_throttle_status: Option<u16>,
    samples_path: PathBuf,
    total_requests: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let repos = load_repos(&args.repos)?;
    if repos.is_empty() {
        bail!("repo list is empty: {}", args.repos.display());
    }

    fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;

    let client = Client::builder()
        .user_agent(&args.user_agent)
        .timeout(Duration::from_secs(30))
        .build()?;

    let mut results = Vec::new();
    match args.mode {
        Mode::A => results.push(run_mode_a(&client, &args, &repos)?),
        Mode::B => results.push(run_mode_b(&client, &args, &repos[0])?),
        Mode::Both => {
            results.push(run_mode_a(&client, &args, &repos)?);
            // Brief cool-down so Mode B is not immediately punished by Mode A.
            eprintln!("cool-down 90s between Mode A and Mode B…");
            thread::sleep(Duration::from_secs(90));
            results.push(run_mode_b(&client, &args, &repos[0])?);
        }
    }

    write_report(&args, &repos, &results)?;
    eprintln!("wrote report {}", args.report.display());
    Ok(())
}

fn load_repos(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.contains('/') {
            bail!("expected owner/repo, got {line:?}");
        }
        out.push(line.to_owned());
    }
    Ok(out)
}

fn run_mode_a(client: &Client, args: &Args, repos: &[String]) -> Result<ModeResult> {
    eprintln!(
        "Mode A: {} repos, ramp {}..{} rpm step {}",
        repos.len(),
        args.ramp_start,
        args.ramp_end,
        args.ramp_step
    );
    let path = args.out_dir.join("mode_a_samples.csv");
    let mut etags: Vec<Option<String>> = vec![None; repos.len()];
    let result = ramp(client, args, "A", &path, |i, rpm| {
        let idx = i % repos.len();
        let repo = &repos[idx];
        let (sample, new_etag) = one_request(client, args, "A", rpm, repo, etags[idx].as_deref())?;
        if new_etag.is_some() {
            etags[idx] = new_etag;
        }
        Ok(sample)
    })?;
    Ok(result)
}

fn run_mode_b(client: &Client, args: &Args, repo: &str) -> Result<ModeResult> {
    eprintln!(
        "Mode B: single repo {repo}, ramp {}..{} rpm step {}",
        args.ramp_start, args.ramp_end, args.ramp_step
    );
    let path = args.out_dir.join("mode_b_samples.csv");
    let mut etag: Option<String> = None;
    let result = ramp(client, args, "B", &path, |_i, rpm| {
        let (sample, new_etag) = one_request(client, args, "B", rpm, repo, etag.as_deref())?;
        if new_etag.is_some() {
            etag = new_etag;
        }
        Ok(sample)
    })?;
    Ok(result)
}

fn ramp<F>(
    _client: &Client,
    args: &Args,
    mode: &str,
    out_path: &Path,
    mut next: F,
) -> Result<ModeResult>
where
    F: FnMut(usize, u32) -> Result<Sample>,
{
    let file = File::create(out_path).with_context(|| format!("create {}", out_path.display()))?;
    let mut wtr = csv::Writer::from_writer(file);

    let mut first_throttle_rpm = None;
    let mut first_throttle_status = None;
    let mut total = 0usize;
    let mut req_index = 0usize;

    let mut rpm = args.ramp_start;
    while rpm <= args.ramp_end {
        eprintln!("  {mode}: holding {rpm} rpm for {}s", args.hold_secs);
        let interval = Duration::from_secs_f64(60.0 / f64::from(rpm));
        let hold_deadline = Instant::now() + Duration::from_secs(args.hold_secs);
        let mut stop_ramp = false;

        while Instant::now() < hold_deadline {
            let tick = Instant::now();
            let sample = next(req_index, rpm)?;
            req_index += 1;
            total += 1;

            let status = sample.status;
            wtr.serialize(&sample)?;
            wtr.flush()?;

            // Honour Retry-After without exception (OPERATIONS.md).
            if let Some(ref ra) = sample.retry_after {
                if let Ok(secs) = ra.parse::<u64>() {
                    eprintln!("  Retry-After={secs}s — sleeping");
                    thread::sleep(Duration::from_secs(secs));
                }
            }

            if status == 429 {
                first_throttle_rpm = Some(rpm);
                first_throttle_status = Some(status);
                eprintln!("  first 429 at {rpm} rpm — stopping ramp (guest policy)");
                stop_ramp = true;
                break;
            }
            if status == 403 {
                // Secondary rate limits often surface as 403. Treat as trip,
                // but keep honouring Retry-After above.
                first_throttle_rpm = Some(rpm);
                first_throttle_status = Some(status);
                eprintln!("  first 403 at {rpm} rpm — stopping ramp (guest policy)");
                stop_ramp = true;
                break;
            }

            let elapsed = tick.elapsed();
            if elapsed < interval {
                thread::sleep(interval - elapsed);
            }
        }

        if stop_ramp {
            break;
        }
        rpm = rpm.saturating_add(args.ramp_step);
        if args.ramp_step == 0 {
            break;
        }
    }

    Ok(ModeResult {
        mode: mode.to_owned(),
        first_throttle_rpm,
        first_throttle_status,
        samples_path: out_path.to_path_buf(),
        total_requests: total,
    })
}

fn one_request(
    client: &Client,
    args: &Args,
    mode: &str,
    ramp_rpm: u32,
    repo: &str,
    etag: Option<&str>,
) -> Result<(Sample, Option<String>)> {
    let url = format!("https://api.github.com/repos/{repo}/git/refs/tags");
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", args.token.trim()))?,
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&args.user_agent)?,
    );
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );
    if let Some(tag) = etag {
        headers.insert(IF_NONE_MATCH, HeaderValue::from_str(tag)?);
    }

    let started = Instant::now();
    let response = client.get(&url).headers(headers).send()?;
    let latency_ms = started.elapsed().as_millis();

    let status = response.status();
    let headers = response.headers().clone();
    // Drain body so the connection can be reused; we do not need contents.
    let _ = response.bytes()?;

    let hdr = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    let new_etag = hdr("etag");
    let sample = Sample {
        mode: mode.to_owned(),
        ramp_rpm,
        timestamp: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".into()),
        repo: repo.to_owned(),
        status: status.as_u16(),
        x_ratelimit_remaining: hdr("x-ratelimit-remaining"),
        x_ratelimit_used: hdr("x-ratelimit-used"),
        x_ratelimit_limit: hdr("x-ratelimit-limit"),
        x_ratelimit_resource: hdr("x-ratelimit-resource"),
        retry_after: hdr("retry-after"),
        etag: new_etag.clone(),
        latency_ms,
    };

    if status == StatusCode::UNAUTHORIZED {
        eprintln!("  warning: unauthorized — check token scopes");
    }

    Ok((sample, new_etag))
}

fn write_report(args: &Args, repos: &[String], results: &[ModeResult]) -> Result<()> {
    if let Some(parent) = args.report.parent() {
        fs::create_dir_all(parent)?;
    }

    let date = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into());

    let mode_a = results.iter().find(|r| r.mode == "A");
    let mode_b = results.iter().find(|r| r.mode == "B");

    let interpretation = match (
        mode_a.and_then(|r| r.first_throttle_rpm),
        mode_b.and_then(|r| r.first_throttle_rpm),
    ) {
        (Some(a), Some(b)) if a.abs_diff(b) <= args.ramp_step => {
            format!(
                "Mode A and Mode B tripped at approximately the same rate \
                 (A={a} rpm, B={b} rpm). This is consistent with a **per-template** \
                 secondary bucket: 500 actions at one sweep per minute would consume \
                 roughly 55% of a 900 rpm ceiling."
            )
        }
        (Some(a), Some(b)) if a > b.saturating_add(args.ramp_step) => {
            format!(
                "Mode A tolerated substantially more traffic than Mode B \
                 (A={a} rpm, B={b} rpm). This is consistent with a **per-URL** \
                 secondary bucket: the high-frequency tier can be much larger than \
                 a naive 900/N split."
            )
        }
        (Some(a), Some(b)) => {
            format!(
                "Mode A tripped earlier than Mode B (A={a} rpm, B={b} rpm). \
                 Re-run before M3; results are ambiguous relative to the ramp step."
            )
        }
        (None, None) => {
            "Neither mode observed a 403/429 within the ramp. The secondary ceiling \
             was not reached under these conditions — re-run with a higher ramp_end \
             before M3, still stopping on the first 429."
                .to_owned()
        }
        (a, b) => format!(
            "Incomplete pair (A={a:?}, B={b:?}). Re-run `--mode both` before M3."
        ),
    };

    let mut md = String::new();
    md.push_str("# Calibration\n\n");
    md.push_str("Measurement of GitHub's undocumented REST secondary rate-limit bucketing.\n\n");
    md.push_str("**Question:** does the ~900 points/minute REST secondary limit bucket ");
    md.push_str("per route **template** (`/repos/{owner}/{repo}/git/refs/tags`) or per ");
    md.push_str("**full URL**?\n\n");
    md.push_str(&format!("- **Date (UTC):** {date}\n"));
    md.push_str(&format!("- **Token type:** {}\n", args.token_type));
    md.push_str(&format!("- **Repos in list:** {}\n", repos.len()));
    md.push_str(&format!(
        "- **Ramp:** {}..{} rpm, step {}, hold {}s\n",
        args.ramp_start, args.ramp_end, args.ramp_step, args.hold_secs
    ));
    md.push_str("- **Endpoint:** `GET /repos/{owner}/{repo}/git/refs/tags` (conditional, `If-None-Match`)\n");
    md.push_str("- **Policy:** honour `Retry-After`; stop the ramp on the first 429/403 secondary trip.\n\n");

    md.push_str("## Results\n\n");
    for r in results {
        md.push_str(&format!("### Mode {}\n\n", r.mode));
        md.push_str(&format!("- Requests issued: {}\n", r.total_requests));
        md.push_str(&format!(
            "- First throttle: {}\n",
            match (r.first_throttle_rpm, r.first_throttle_status) {
                (Some(rpm), Some(st)) => format!("{st} at {rpm} rpm"),
                _ => "not observed within ramp".into(),
            }
        ));
        md.push_str(&format!(
            "- Raw samples: `{}`\n\n",
            r.samples_path.display()
        ));
    }

    md.push_str("## Interpretation\n\n");
    md.push_str(&interpretation);
    md.push_str("\n\n");
    md.push_str("## Re-run before M3\n\n");
    md.push_str("```bash\n");
    md.push_str("export REFLEDGER_CALIBRATE_TOKEN=…   # never commit\n");
    md.push_str("cargo run --manifest-path tools/calibrate/Cargo.toml -- \\\n");
    md.push_str("  --repos tools/calibrate/repos.txt \\\n");
    md.push_str("  --mode both \\\n");
    md.push_str(&format!("  --token-type {} \\\n", args.token_type));
    md.push_str("  --out-dir tools/calibrate/data \\\n");
    md.push_str("  --report docs/CALIBRATION.md\n");
    md.push_str("```\n");

    let mut f = File::create(&args.report)?;
    f.write_all(md.as_bytes())?;
    Ok(())
}
