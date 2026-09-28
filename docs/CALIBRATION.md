# Calibration

Measurement of GitHub's undocumented REST secondary rate-limit bucketing.

**Question:** does the ~900 points/minute REST secondary limit bucket per route
**template** (`/repos/{owner}/{repo}/git/refs/tags`) or per **full URL**?

## Status: deferred past M1 — gate is population growth, not launch

### 2026-09-28 — gate moved

At the amended M1 population (tens of repositories, one poll per minute with
REST+ETag), secondary spend is under 4% of the documented 900-point ceiling
**whichever way GitHub buckets it**. The per-template-versus-per-URL question
only bites past a few hundred repos.

Calibration is therefore **not** an M1 exit gate. The scheduler ships with a
conservative fixed rate (see `scheduler.rs`: 60s interval, concurrency 4,
global 300 points/minute). Archive days cannot be recovered; waiting on this
measurement costs days that the log can never backfill.

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
