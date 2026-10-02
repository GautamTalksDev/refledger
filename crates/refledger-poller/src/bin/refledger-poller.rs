//! `refledger-poller` — once sweeps and signing-key generation.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use refledger_log::{
    generate_signing_key_file, load_signing_key, normalize_to_utc_millis, KeySource,
};
use refledger_poller::once::{run_once, scheduled_time_from_env, OnceArgs, ENABLED_VAR};
use refledger_poller::publish::GitLedgerPublisher;
use refledger_poller::store::{Store, StoreOptions};
use time::OffsetDateTime;

fn main() -> ExitCode {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let Some(cmd) = args.first().map(String::as_str) else {
        usage();
        return ExitCode::from(2);
    };
    match cmd {
        "once" => {
            args.remove(0);
            cmd_once(&args)
        }
        "publish-pending" => {
            args.remove(0);
            cmd_publish_pending(&args)
        }
        "keygen" => {
            args.remove(0);
            cmd_keygen(&args)
        }
        "-h" | "--help" | "help" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            usage();
            ExitCode::from(2)
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n  refledger-poller once --data <dir> [--watched <path>]\n  refledger-poller publish-pending --data <dir>\n  refledger-poller keygen --out <path>"
    );
}

fn cmd_keygen(args: &[String]) -> ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let Some(out) = out else {
        eprintln!("--out <path> is required");
        return ExitCode::from(2);
    };

    match generate_signing_key_file(&out) {
        Ok(pubk) => {
            // Public material only. Never print the seed.
            println!("public_key={}", pubk.public_key_hex);
            println!("key_id={}", pubk.key_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("keygen failed: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_data_dir(args: &[String]) -> Result<PathBuf, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                i += 1;
                data_dir = args.get(i).map(PathBuf::from);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    data_dir.ok_or_else(|| "--data <dir> is required".to_owned())
}

fn store_options_from_env(actual_start: OffsetDateTime) -> Result<StoreOptions, String> {
    let key_hex = match env::var("REFLEDGER_SIGNING_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Err("REFLEDGER_SIGNING_KEY is not set".into()),
    };
    let signing_key = load_signing_key(KeySource::HexSeed(key_hex)).map_err(|e| e.to_string())?;

    let push_token = match env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => String::new(),
    };

    let mut opts = StoreOptions::new(signing_key, actual_start);
    opts.observations_on_data_branch = true;
    if std::env::var("REFLEDGER_SKIP_STORE_LOCK").ok().as_deref() == Some("1") {
        opts.skip_lock = true;
    }
    if let Ok(publisher) = GitLedgerPublisher::from_env() {
        opts.publisher = Box::new(publisher);
    } else if let Ok(clone) = env::var("REFLEDGER_PUBLISH_CLONE") {
        let mut pub_ = GitLedgerPublisher::new(clone, None);
        if !push_token.is_empty() {
            pub_ = pub_.with_github_token(push_token);
        }
        opts.publisher = Box::new(pub_);
    }
    Ok(opts)
}

/// Push any deferred/failed seal publishes to main, and backfill Rekor indexes.
/// Must run *after* the data-branch commit (no REFLEDGER_DEFER_LEDGER_PUBLISH).
fn cmd_publish_pending(args: &[String]) -> ExitCode {
    let data_dir = match parse_data_dir(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let actual_start = normalize_to_utc_millis(OffsetDateTime::now_utc());
    let opts = match store_options_from_env(actual_start) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let mut store = match Store::open(&data_dir, opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open store: {e}");
            return ExitCode::from(1);
        }
    };
    match store.retry_pending_publishes() {
        Ok(published) => eprintln!("publish-pending: published={published}"),
        Err(e) => {
            eprintln!("publish-pending failed: {e}");
            return ExitCode::from(1);
        }
    }
    match store.retry_witnesses() {
        Ok(n) => eprintln!("publish-pending: witnesses_retried={n}"),
        Err(e) => {
            eprintln!("retry_witnesses failed: {e}");
            return ExitCode::from(1);
        }
    }
    // Push sealed log/heads even when there was no new seal this poll — a
    // witness backfill (Rekor 409→lookup) must still reach main.
    match store.republish_sealed_tip() {
        Ok(ok) => eprintln!("publish-pending: tip_republished={ok}"),
        Err(e) => {
            eprintln!("republish_sealed_tip failed: {e}");
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

fn cmd_once(args: &[String]) -> ExitCode {
    let mut data_dir: Option<PathBuf> = None;
    let mut watched: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                i += 1;
                data_dir = args.get(i).map(PathBuf::from);
            }
            "--watched" => {
                i += 1;
                watched = args.get(i).map(PathBuf::from);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let Some(data_dir) = data_dir else {
        eprintln!("--data <dir> is required");
        return ExitCode::from(2);
    };
    let watched = watched.unwrap_or_else(|| PathBuf::from("population/watched.jsonl"));

    // Defence in depth: the workflow also gates on this variable.
    if !refledger_poller::poller_enabled(env::var(ENABLED_VAR).ok().as_deref()) {
        // When invoked outside Actions without the variable, treat as disabled
        // only if REFLEDGER_REQUIRE_ENABLED=1 (workflow sets this).
        if env::var("REFLEDGER_REQUIRE_ENABLED").ok().as_deref() == Some("1") {
            eprintln!("{ENABLED_VAR} is not true; refusing to poll");
            return ExitCode::from(0);
        }
    }

    let actual_start = normalize_to_utc_millis(OffsetDateTime::now_utc());
    let scheduled_at = scheduled_time_from_env(actual_start);

    let mut opts = match store_options_from_env(actual_start) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // Read-only PAT for GitHub API. Never fall back to GITHUB_TOKEN for reads
    // when Actions requires a dedicated token; never log the value.
    let read_token = match env::var("REFLEDGER_GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ if env::var("REFLEDGER_REQUIRE_ENABLED").ok().as_deref() == Some("1") => {
            eprintln!("REFLEDGER_GITHUB_TOKEN is not set (required for GitHub reads)");
            return ExitCode::from(1);
        }
        _ => match env::var("GITHUB_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("REFLEDGER_GITHUB_TOKEN is not set");
                return ExitCode::from(1);
            }
        },
    };

    // Ensure publisher is wired (store_options_from_env already did); keep opts.
    let _ = &mut opts;

    let mut once = OnceArgs::production(data_dir, watched);
    once.token = read_token;
    once.scheduled_at = scheduled_at;
    once.actual_start = actual_start;

    match run_once(opts, once) {
        Ok(report) => {
            eprintln!(
                "once ok: obs={} gaps={} sealed={} confirm={} req={} 200={} 304={} cond={} tip_seq={} tree_unverified_before={} tree_unverified_after={}",
                report.observations,
                report.gaps,
                report.days_sealed.len(),
                report.confirmations,
                report.requests,
                report.status_200,
                report.status_304,
                report.conditional_requests,
                report.tip_seq,
                report.tree_unverified_before,
                report.tree_unverified_after
            );
            // Machine-readable line for the workflow commit message.
            println!(
                "poll {} seq {}",
                actual_start
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| actual_start.to_string()),
                report.tip_seq
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("once failed: {e}");
            ExitCode::from(1)
        }
    }
}
