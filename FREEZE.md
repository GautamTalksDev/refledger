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
| _(none yet)_ | | |

## Ends

Seven full days after the first clean seal on `main` (operator records the start wall-clock here when the run begins):

- **Run start (UTC):** _TBD (first clean seal)_
- **Freeze lifts (UTC):** _TBD (start + 7 days)_
