# Main branch freeze (seven-day run)

**Code on `main` is frozen** from **2026-10-03T00:00:00Z** through the seal at **2026-10-10T00:00:00Z** — the same closed interval as the M1 measurement window. The 2026-10-02 start was voided by the exception below.

During the freeze:

- **Docs only.** Markdown and comments that do not change build inputs, lockfiles, workflows, crates, or the clock Worker may land.
- **No code.** No Rust, TypeScript, workflow, Dependabot, lockfile, or `clock/` changes on `main` unless this file records an emergency exception.
- **Emergency fixes** require a dated entry under [Exceptions](#exceptions) explaining why the instrument had to change mid-measurement.

The freeze exists so the running poller, log format, and signatures stay the instrument they were when the measurement window started. Dependency majors for the signing/hashing stack wait until after the run and are gated on both conformance suites.

Earlier canary-driven fixes (through 2026-10-01) are **pre-freeze hardening**, not freeze exceptions. Treating them as exceptions blurred what the freeze guarantees.

## Pre-freeze hardening

Fixes found by the canary and live polls before the M1 window. History only — they do not authorize mid-window changes.

| Date (UTC) | Commit | What |
| --- | --- | --- |
| 2026-09-30 | 231b93e, 251fcdc, fe08b8f | First seal signed/witnessed on `data` but never reached `main` (`.gitignore` ignored `data/log/`; untracked parent `data/` refused; push auth double-credential). Pending publishes retry every poll. |
| 2026-09-30 | 07c049c | `once` never ran classify→derive→append; listing invented `tree_sha`; confirm starved by backfill; canary patterns 3/4 invisible within one poll. |
| 2026-09-30 | ce469fd | Queue signed digest note for `docs/tree-sha-affected-observations.txt`. |
| 2026-09-30 | 6ec4dde | Changed/reappeared refs stored listing-only when the new tip was uncached; classify skipped unpeeled refs (tj-actions / Trivy shape would produce no Move). Priority-peel from reserve; seq 40 Deletion correction; canary-score retired-pattern + manual-intervention. |
| 2026-10-01 | b5e1b05 | Polls aborted on enrich compare failure; invented listing placeholders looked like Moves. Enrich isolation + skip invented SHAs. (Voided the first attempted M1 start.) |
| 2026-10-01 | ea8aefb | Canary Move buffered in memory and lost across process exit; Rekor 409 hard-failed; seal could publish to main before data commit. Durable buffer + re-derive; 409→lookup; publish after data. |
| 2026-10-01 | fcaecb5 | Disclose `heads.jsonl` line rewrite on main; refuse non-prefix publish; `--strict` requires identical head+signature across witness lines for one seq. |
| 2026-10-01 | e3f3a7d | Batch proof stayed listing-only when the prior tip was also listing-only; priority-peel any `target_sha` change (and never-peeled FROM); recovery re-derive; `SchedulerLag` as gap; canary bootstrap stops before pattern. |

## Exceptions

| Date (UTC) | Commit | Why |
| --- | --- | --- |
| 2026-10-02 | fee9ffa | A classifier bug can sign a false High "content change" about a real maintainer's tag. |

## Run window

- **Freeze start (UTC):** 2026-10-03T00:00:00Z
- **Freeze lifts (UTC):** 2026-10-10T00:00:00Z (after the 2026-10-09 day seal)

## M1 measurement window

The seven-day M1 exit is measured on sealed days in this closed interval. Freeze and M1 start together so the instrument under measurement is the one that enters the window.

**Prior attempts voided:** a 2026-10-01T00:00:00Z start was abandoned after `b5e1b05` changed the poller inside that day. The 2026-10-02T00:00:00Z start was abandoned after the 2026-10-02 tree-invariant exception. Measurement restarts at the first UTC midnight after that exception lands.

- **M1 start (UTC):** 2026-10-03T00:00:00Z
- **M1 end (UTC):** seal at 2026-10-10T00:00:00Z (day 2026-10-09's ObservationDigest, stamped at the start of 2026-10-10)
