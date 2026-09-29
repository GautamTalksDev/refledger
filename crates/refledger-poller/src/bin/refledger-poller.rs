//! `refledger-poller once` — one GitHub Actions sweep, then exit.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use refledger_log::{load_signing_key, KeySource};
use refledger_poller::once::{
    run_once, scheduled_time_from_env, OnceArgs, ENABLED_VAR,
};
use refledger_poller::publish::GitLedgerPublisher;
use refledger_poller::store::StoreOptions;
use time::OffsetDateTime;

fn main() -> ExitCode {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some("once") {
        eprintln!("usage: refledger-poller once --data <dir> [--watched <path>]");
        return ExitCode::from(2);
    }
    args.remove(0);

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

    let actual_start = OffsetDateTime::now_utc();
    let scheduled_at = scheduled_time_from_env(actual_start);

    let key_hex = match env::var("REFLEDGER_SIGNING_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("REFLEDGER_SIGNING_KEY is not set");
            return ExitCode::from(1);
        }
    };
    // Never echo the key.
    let signing_key = match load_signing_key(KeySource::HexSeed(key_hex)) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("signing key: {e}");
            return ExitCode::from(1);
        }
    };

    let token = match env::var("GITHUB_TOKEN").or_else(|_| env::var("REFLEDGER_GITHUB_TOKEN")) {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("GITHUB_TOKEN is not set");
            return ExitCode::from(1);
        }
    };

    let mut opts = StoreOptions::new(signing_key, actual_start);
    opts.observations_on_data_branch = true;
    if let Ok(publisher) = GitLedgerPublisher::from_env() {
        opts.publisher = Box::new(publisher);
    } else if let Ok(clone) = env::var("REFLEDGER_PUBLISH_CLONE") {
        let pub_ = GitLedgerPublisher::new(clone, None).with_github_token(token.clone());
        opts.publisher = Box::new(pub_);
    }

    let mut once = OnceArgs::production(data_dir, watched);
    once.token = token;
    once.scheduled_at = scheduled_at;
    once.actual_start = actual_start;

    match run_once(opts, once) {
        Ok(report) => {
            eprintln!(
                "once ok: obs={} gaps={} sealed={} confirm={} req={} 200={} 304={} tip_seq={}",
                report.observations,
                report.gaps,
                report.days_sealed.len(),
                report.confirmations,
                report.requests,
                report.status_200,
                report.status_304,
                report.tip_seq
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
