# Main branch freeze (seven-day run)

**Code on `main` is frozen** from the first clean seal for **seven days**.

During the freeze:

- **Docs only.** Markdown and comments that do not change build inputs, lockfiles, workflows, crates, or the clock Worker may land.
- **No code.** No Rust, TypeScript, workflow, Dependabot, lockfile, or `clock/` changes on `main` unless this file records an emergency exception.
- **Emergency fixes** require a dated entry below explaining why the instrument had to change mid-measurement.

The freeze exists so the running poller, log format, and signatures stay the instrument they were when the run started. Dependency majors for the signing/hashing stack wait until after the run and are gated on both conformance suites.

## Exceptions

| Date (UTC) | Commit | Why |
| --- | --- | --- |
| 2026-09-30 | 231b93e, 251fcdc, fe08b8f | Pre-freeze: first seal was signed/witnessed on `data` but never reached `main`. Causes fixed in order: `.gitignore` ignored `data/log/`; untracked parent `data/` refused; push auth double-credential (`origin` token URL + Authorization header). Pending publishes retry every poll. Freeze had not started (starts at first clean seal on `main`). |
| 2026-09-30 | 07c049c | Emergency: `once` never ran classify→derive→append (ledger could not record moves); listing invented `tree_sha`; confirm starved by backfill; canary patterns 3/4 invisible within one poll. Option A forward-only; sealed days untouched. |
| 2026-09-30 | ce469fd | Queue signed digest note for `docs/tree-sha-affected-observations.txt` (same class as the false-422 note). Freeze start recorded in this commit. |
| 2026-09-30 | (this commit) | Emergency before M1 window (starts 2026-10-01T00:00:00Z): changed/reappeared refs stored listing-only when the new tip was uncached, and classify skipped unpeeled refs — a tag moved to a brand-new malicious commit (tj-actions / Trivy shape) would produce no Move. Priority-peel those tips from the reserved budget before classify. Also: Correction for seq 40 Deletion `content_change`; canary-score retired-pattern + manual-intervention sections. |

## Run window

- **Freeze start (UTC):** 2026-09-30T05:24:40Z
- **Freeze lifts (UTC):** 2026-10-07T05:24:40Z (start + 7 days)

## M1 measurement window

The seven-day M1 exit is measured on sealed days in this closed interval:

- **M1 start (UTC):** 2026-10-01T00:00:00Z
- **M1 end (UTC):** seal at 2026-10-08T00:00:00Z (day 2026-10-07's ObservationDigest, stamped at the start of 2026-10-08)
