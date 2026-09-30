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

## Ends

Seven full days after the first clean seal on `main` (operator records the start wall-clock here when the run begins):

- **Run start (UTC):** _TBD (first clean seal)_
- **Freeze lifts (UTC):** _TBD (start + 7 days)_
