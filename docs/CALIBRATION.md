# Calibration

Measurement of GitHub's undocumented REST secondary rate-limit bucketing.

**Question:** does the ~900 points/minute REST secondary limit bucket per route
**template** (`/repos/{owner}/{repo}/git/refs/tags`) or per **full URL**?

## Status: deferred past M1 — gate is population growth, not launch

### 2026-09-28 — gate moved

At the amended M1 population (tens of repositories, one poll every 5 minutes with
REST+ETag on GitHub Actions), secondary spend is a small fraction of the
documented 900-point ceiling **whichever way GitHub buckets it**. The
per-template-versus-per-URL question only bites past a few hundred repos.

Calibration is therefore **not** an M1 exit gate. The scheduler ships with a
conservative fixed rate (see `scheduler.rs`: 300s interval on GitHub Actions,
concurrency 4, global 300 points/minute, 150 requests per run). Archive days
cannot be recovered; waiting on this measurement costs days that the log can
never backfill.

### 2026-09-29 — Actions token rotation and ETags

Each Actions run authenticates with a fresh `GITHUB_TOKEN`. GitHub may treat
validators as bound to the token that stored them, so a 304 rate measured on a
long-lived PAT may not hold across runs. Every `once` run logs how many
responses were **304** versus **200**.

If ETags do not survive token rotation, each sweep re-lists tags (~one request
per watched repository, plus peels only on change or warm-up). At 35
repositories and a 5 minute cadence that is about **420 listing requests per
hour**, still under the 1,000/hour `GITHUB_TOKEN` budget even before 304s.
Record live 304/200 ratios here after genesis; do not invent them.

**Required before the watched population passes ~200 repositories** (and again
before any high-frequency tier). Until then a live Mode A/B run is useful but
not blocking.

```bash
export REFLEDGER_CALIBRATE_TOKEN=…   # never commit the token
cargo run --manifest-path tools/calibrate/Cargo.toml --release -- \
  --repos tools/calibrate/repos.txt \
  --mode both \
  --token-type classic-pat \
  --out-dir tools/calibrate/data \
  --report docs/CALIBRATION.md
```

The instrument:

1. Mode A — conditional `GET /repos/{owner}/{repo}/git/refs/tags` across N
   different repos, ramping 100→1200 rpm.
2. Mode B — the same volume against one repo repeatedly.
3. Records timestamp, status, `x-ratelimit-*`, `retry-after`, latency for every
   request.
4. Honours `Retry-After` without exception and **stops the ramp on the first
   429/403** (guest policy; OPERATIONS.md is already published).

### How to read the result

| Observation | Implication for a larger population |
|---|---|
| Mode A and Mode B trip at ~the same rpm | Bucket is **per template**. Growth past ~200 at one sweep/minute must stay well under 900 rpm. |
| Mode A tolerates far more than Mode B | Bucket is **per URL**. High-frequency tier can be much larger. |

## Results

_Awaiting live run (deferred; not an M1 blocker)._

## Raw data

_Awaiting `tools/calibrate/data/mode_a_samples.csv` and `mode_b_samples.csv`._
