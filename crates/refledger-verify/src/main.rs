//! Independent Refledger log verifier CLI.
//!
//! Must not depend on refledger-log. Canonicalisation and replay are local.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use refledger_verify::{
    failure_report, load_jsonl_dir, verify_chain, verify_entries_not_stale_vs_heads,
    verify_entries_not_stale_vs_now, verify_heads_jsonl, verify_observation_digests,
    verify_signed_head, verify_strict, ChainVerdict, Failure, SignedHeadFile, VerifyError,
};

#[derive(Debug, Parser)]
#[command(
    name = "refledger-verify",
    about = "Independently verify a Refledger log"
)]
struct Args {
    /// Directory containing JSONL log files (e.g. data/log).
    log_dir: PathBuf,

    /// First sequence number to verify (inclusive).
    #[arg(long)]
    from: Option<u64>,

    /// Last sequence number to verify (inclusive).
    #[arg(long)]
    to: Option<u64>,

    /// Path to a signed-head.json document.
    #[arg(long)]
    head: Option<PathBuf>,

    /// Expected Ed25519 public key (64 lowercase hex chars). Compared to the
    /// key in --head when both are present; used for signature verification.
    #[arg(long)]
    pubkey: Option<String>,

    /// Root directory of observation JSONL archives. When set, published
    /// ObservationDigest entries are checked against file contents if present.
    #[arg(long)]
    observations: Option<PathBuf>,

    /// Emit a machine-readable JSON verdict.
    #[arg(long)]
    json: bool,

    /// Fail on a witness backlog older than 48 hours (missing Rekor log_index)
    /// or a digest note that records one. Implies checking `heads.jsonl` beside
    /// the log directory when `--head` is not set.
    #[arg(long)]
    strict: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(code) => code,
        Err(VerifyError::Io(msg)) | Err(VerifyError::Parse(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
        Err(VerifyError::Canonical(e)) => {
            eprintln!("error: canonicalisation: {e}");
            ExitCode::from(2)
        }
        Err(VerifyError::Head(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
        Err(VerifyError::Failed { failure }) => {
            // Should be handled inside run via verdict printing.
            eprintln!("verification failed: {failure}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<ExitCode, VerifyError> {
    let loaded = load_jsonl_dir(&args.log_dir)?;
    for path in &loaded.ignored {
        eprintln!("ignored non-chain file: {path}");
    }
    let mut entries = loaded.entries;
    entries.sort_by_key(|e| e.get("seq").and_then(|v| v.as_u64()).unwrap_or(u64::MAX));

    if let Some(from) = args.from {
        entries.retain(|e| {
            e.get("seq")
                .and_then(|v| v.as_u64())
                .is_some_and(|s| s >= from)
        });
    }
    if let Some(to) = args.to {
        entries.retain(|e| {
            e.get("seq")
                .and_then(|v| v.as_u64())
                .is_some_and(|s| s <= to)
        });
    }

    let mut verdict = match verify_chain(&entries) {
        Ok(v) => v,
        Err(VerifyError::Failed { failure }) => {
            let verdict = fail_verdict(&failure);
            print_verdict(&verdict, args.json);
            return Ok(ExitCode::from(1));
        }
        Err(e) => return Err(e),
    };

    if let Some(head_path) = &args.head {
        let raw = std::fs::read_to_string(head_path)
            .map_err(|e| VerifyError::Io(format!("read head: {e}")))?;
        let is_jsonl = head_path.extension().and_then(|s| s.to_str()) == Some("jsonl");
        let head_result = if is_jsonl {
            verify_heads_jsonl(&entries, &raw, args.pubkey.as_deref())
        } else {
            let signed: SignedHeadFile = serde_json::from_str(&raw)
                .map_err(|e| VerifyError::Parse(format!("head json: {e}")))?;
            let tip = entries.last().ok_or_else(|| {
                VerifyError::Io("--head requires at least one entry in range".into())
            })?;
            verify_signed_head(&signed, tip, args.pubkey.as_deref())
        };
        match head_result {
            Ok(status) => verdict.head = Some(status),
            Err(VerifyError::Failed { failure }) => {
                let mut v = fail_verdict(&failure);
                v.entries = verdict.entries;
                v.seq_first = verdict.seq_first;
                v.seq_last = verdict.seq_last;
                v.span_start = verdict.span_start;
                v.span_end = verdict.span_end;
                v.coverage_skipped = verdict.coverage_skipped;
                v.coverage_failed = verdict.coverage_failed;
                print_verdict(&v, args.json);
                return Ok(ExitCode::from(1));
            }
            Err(e) => return Err(e),
        }
    } else if args.pubkey.is_some() && !args.strict {
        return Err(VerifyError::Io(
            "--pubkey requires --head, or --strict (which reads heads.jsonl beside the log)".into(),
        ));
    }

    if let Some(obs) = &args.observations {
        match verify_observation_digests(&entries, obs) {
            Ok(()) => {}
            Err(VerifyError::Failed { failure }) => {
                let mut v = fail_verdict(&failure);
                v.entries = verdict.entries;
                v.seq_first = verdict.seq_first;
                v.seq_last = verdict.seq_last;
                v.span_start = verdict.span_start;
                v.span_end = verdict.span_end;
                v.coverage_skipped = verdict.coverage_skipped;
                v.coverage_failed = verdict.coverage_failed;
                v.head = verdict.head;
                print_verdict(&v, args.json);
                return Ok(ExitCode::from(1));
            }
            Err(e) => return Err(e),
        }
    }

    if args.strict {
        let heads_path = args
            .head
            .clone()
            .unwrap_or_else(|| args.log_dir.join("heads.jsonl"));
        let heads_raw = match std::fs::read_to_string(&heads_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(VerifyError::Io(format!(
                    "--strict read {}: {e}",
                    heads_path.display()
                )));
            }
        };
        let heads_empty = heads_raw
            .as_deref()
            .map(|s| s.lines().all(|l| l.trim().is_empty()))
            .unwrap_or(true);

        // Empty chain / no heads yet (pre-first-seal) is a valid genesis state.
        if entries.is_empty() {
            if args.json {
                print_verdict(&verdict, true);
            } else {
                println!("chain: empty");
                println!("heads: none yet");
            }
            return Ok(ExitCode::from(0));
        }

        if heads_empty {
            // Entries exist but no signed head yet: fail only when an entry is
            // more than 48h older than now (stale unsigned history).
            if let Err(VerifyError::Failed { failure }) = verify_entries_not_stale_vs_now(&entries)
            {
                let mut v = fail_verdict(&failure);
                v.entries = verdict.entries;
                v.seq_first = verdict.seq_first;
                v.seq_last = verdict.seq_last;
                v.span_start = verdict.span_start;
                v.span_end = verdict.span_end;
                print_verdict(&v, args.json);
                return Ok(ExitCode::from(1));
            }
            if !args.json {
                println!("heads: none yet");
            }
        } else {
            let raw = heads_raw.as_deref().unwrap_or("");
            if args.head.is_none() {
                match verify_heads_jsonl(&entries, raw, args.pubkey.as_deref()) {
                    Ok(status) => verdict.head = Some(status),
                    Err(VerifyError::Failed { failure }) => {
                        let mut v = fail_verdict(&failure);
                        v.entries = verdict.entries;
                        v.seq_first = verdict.seq_first;
                        v.seq_last = verdict.seq_last;
                        v.span_start = verdict.span_start;
                        v.span_end = verdict.span_end;
                        v.coverage_skipped = verdict.coverage_skipped;
                        v.coverage_failed = verdict.coverage_failed;
                        print_verdict(&v, args.json);
                        return Ok(ExitCode::from(1));
                    }
                    Err(e) => return Err(e),
                }
            }
            // Entry older than latest head by >48h is a hard failure.
            if let Err(VerifyError::Failed { failure }) =
                verify_entries_not_stale_vs_heads(&entries, raw)
            {
                let mut v = fail_verdict(&failure);
                v.entries = verdict.entries;
                v.seq_first = verdict.seq_first;
                v.seq_last = verdict.seq_last;
                v.span_start = verdict.span_start;
                v.span_end = verdict.span_end;
                v.coverage_skipped = verdict.coverage_skipped;
                v.coverage_failed = verdict.coverage_failed;
                v.head = verdict.head.clone();
                print_verdict(&v, args.json);
                return Ok(ExitCode::from(1));
            }
            match verify_strict(&entries, raw) {
                Ok(()) => {}
                Err(VerifyError::Failed { failure }) => {
                    let mut v = fail_verdict(&failure);
                    v.entries = verdict.entries;
                    v.seq_first = verdict.seq_first;
                    v.seq_last = verdict.seq_last;
                    v.span_start = verdict.span_start;
                    v.span_end = verdict.span_end;
                    v.coverage_skipped = verdict.coverage_skipped;
                    v.coverage_failed = verdict.coverage_failed;
                    v.head = verdict.head;
                    print_verdict(&v, args.json);
                    return Ok(ExitCode::from(1));
                }
                Err(e) => return Err(e),
            }
        }
    }

    if entries.is_empty() && !args.json {
        println!("chain: empty");
        println!("heads: none yet");
        return Ok(ExitCode::from(0));
    }

    print_verdict(&verdict, args.json);
    Ok(ExitCode::from(0))
}

fn fail_verdict(failure: &Failure) -> ChainVerdict {
    let report = failure_report(failure);
    ChainVerdict {
        ok: false,
        entries: 0,
        seq_first: None,
        seq_last: None,
        span_start: None,
        span_end: None,
        head: None,
        coverage_skipped: 0,
        coverage_failed: 0,
        failure: Some(report),
    }
}

fn print_verdict(v: &ChainVerdict, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(v).expect("json"));
        return;
    }

    if !v.ok {
        if let Some(f) = &v.failure {
            match f.seq {
                Some(seq) => println!("FAIL seq {seq}: {}", f.check),
                None => println!("FAIL: {}", f.check),
            }
        } else {
            println!("FAIL");
        }
        return;
    }

    // At most five lines.
    if v.entries == 0 {
        println!("chain: empty");
        println!("heads: none yet");
        return;
    }
    println!("chain: OK");
    match (v.seq_first, v.seq_last) {
        (Some(a), Some(b)) => println!("entries: {} (seq {a} .. {b})", v.entries),
        _ => println!("entries: {}", v.entries),
    }
    match (&v.span_start, &v.span_end) {
        (Some(a), Some(b)) => {
            let a = a.get(..10).unwrap_or(a);
            let b = b.get(..10).unwrap_or(b);
            println!("span: {a} .. {b}");
        }
        _ => println!("span: (none)"),
    }
    match &v.head {
        Some(h) if h.valid => {
            println!("head: signed, valid, key {}...", h.key_prefix);
        }
        Some(_) => println!("head: present, invalid"),
        None => println!("head: (not checked)"),
    }
    println!(
        "coverage gaps recorded: {} skipped, {} failed polls (from signed digests)",
        v.coverage_skipped, v.coverage_failed
    );
}
