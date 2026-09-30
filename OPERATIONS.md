# Operations

This document is the public crawler policy for Refledger. It is published before the first request is made. It is a commitment to third parties (repository owners, GitHub, and anyone relying on our data), not an internal runbook.

## 1. What this service does

Refledger polls public GitHub repositories for git tag refs and records what each ref pointed at and when, using documented public APIs. The purpose is a durable, independently verifiable log of tag identity over time: not private repository access, not account harassment, and not scraping of non-API surfaces.

## 2. Identification

Every request identifies itself with a User-Agent of the form:

```
refledger/<version> (+https://raw.githubusercontent.com/GautamTalksDev/refledger/main/OPERATIONS.md)
```

A contact URL is always present in the User-Agent and always resolves to this policy (or its successor at the same path). Until `https://refledger.dev/operations` is live, the raw GitHub URL above is the published contact. The poller refuses to start if the contact URL still contains a `<…>` placeholder, fails to parse, or does not return HTTP 2xx.

## 3. What we request

We use exactly these GitHub REST endpoints:

- `GET /repos/{owner}/{repo}/git/refs/tags`
- `GET /repos/{owner}/{repo}/git/tags/{sha}`
- `GET /repos/{owner}/{repo}/commits/{sha}`
- `GET /repos/{owner}/{repo}/compare/{base}...{head}`
- `GET /repos/{owner}/{repo}/releases`
- `GET /repos/{owner}/{repo}/contents/{path}` (action.yml only)

Only public data. Only documented APIs. No HTML scraping. No undocumented endpoints. No authentication circumvention.

## 4. Frequency and conditional requests

Every request carries `If-None-Match` with a stored ETag, so the overwhelming majority of polls return `304 Not Modified` and cost GitHub almost nothing.

Polling tiers and intervals (to be filled from the M0 calibration):

| Tier | Interval | Notes |
|------|----------|-------|
| TBD  | TBD      | Filled after M0 calibration |
| TBD  | TBD      | Filled after M0 calibration |
| TBD  | TBD      | Filled after M0 calibration |

## 5. Rate limits and backoff

We honour `Retry-After` without exception.

On `403` or `429` we back off exponentially and record the backoff as an observation.

We cap concurrency well below GitHub's documented 100-concurrent secondary limit.

We treat the 900-points-per-minute REST secondary limit as a hard ceiling even though no header reports it.

## 6. Coverage gaps

Every failed or skipped poll is recorded as an observation and is visible in the published data. We never silently fill a gap.

## 7. Detection latency

We can only know a tag moved between two observations, never the exact moment. Every published claim states its observation window. The published latency measurement will be linked here once available.

## 8. Contact and correction

- **Security contact:** see [`SECURITY.md`](SECURITY.md) (GitHub private vulnerability advisory preferred).
- **Data-correction contact:** open an issue on `GautamTalksDev/refledger` tagged for data correction, or use the address published on the contact page when `refledger.dev` is live.

A maintainer who believes our data is wrong will get an investigation and, if we were wrong, a correction entry appended to the log.

## 9. Changelog

This policy changes only via a dated entry appended here. Prior text is not rewritten in place.

### 2026-09-21 - Initial publication

Initial publication of this operations policy, before the first crawler request.

### 2026-09-28 - Tag listing and commit endpoints

Two REST endpoints named in §3 are superseded for the reasons below. The §3 list is left as published; this entry is the amendment.

- **Tag listing:** `GET /repos/{owner}/{repo}/git/refs/tags` is replaced by `GET /repos/{owner}/{repo}/git/matching-refs/tags`. Live check (2026-09-28) against a tagless public repository (`octocat/Spoon-Knife`) and a nonexistent repository showed that `/git/refs/tags` returns `404` for both, so a tagless repo would be indistinguishable from a deleted one. `/git/matching-refs/tags` returns `200` with `[]` for the tagless case and `404` for the missing case.
- **Commit → tree:** `GET /repos/{owner}/{repo}/commits/{sha}` is replaced by `GET /repos/{owner}/{repo}/git/commits/{sha}`. The git-data endpoint returns the tree oid without the full file list and stats the poller does not need.

### 2026-09-28 - M1 polling rate and detection latency path

§4's tier table is left as published; this entry is the amendment for M1.

- **Interval:** one tier, every poll group once per **60 seconds**, dues spread across the interval (never a burst at `:00`). Concurrency **4**. Global secondary-points governor capped at **300/minute** (one third of the documented 900 ceiling), applied to every request class.
- **Calibration:** Mode A/B measurement is **not** an M1 gate. At tens of repositories the spend is under 4% of the secondary ceiling either way GitHub buckets. Calibration is required before the watched population passes ~200 (`docs/CALIBRATION.md`).
- **Detection latency:** the published figure for §7 will live at `docs/DETECTION.md`, produced by joining `GautamTalksDev/canary`'s ledger against the chain.

### 2026-09-29 - GitHub Actions host and 5 minute cadence

§4's tier table is left as published; this entry supersedes the 60 second interval above for M1.

- **Host:** scheduled GitHub Actions workflow (`.github/workflows/poll.yml`), not a long-lived VM. One sweep per run, then exit.
- **Interval:** every **5 minutes** at `:02`, `:07`, `:12`, … `:57` (never `:00`). Cron can be delayed or dropped under load; every miss is recorded as a `SchedulerLag` gap with scheduled vs actual start times.
- **Confirmation:** when a run sees a tag move, it re-polls that repository about 60 seconds later in the same job and records both observations.
- **Budget:** at most 150 GitHub API requests per run; first-run tag peels capped so large sets warm across runs.
- **Enable:** repository variable `REFLEDGER_ENABLED` must equal `true`. Default is off.
- **Observations:** public on the `data` branch. Daily sealed digests still land on `main` under `data/log/` via `GITHUB_TOKEN`.

### 2026-09-29 - Clock, read PAT, ledger Environment, two-phase budget

Amends the Actions-host entry above to match the system as it runs after genesis.

- **Primary clock:** Cloudflare Worker `refledger-clock` (`clock/`) POSTs `workflow_dispatch` for `poll.yml` on the same `:02`, `:07`, … cadence. The workflow `schedule` remains backup only.
- **Reads vs pushes:** API reads use repository secret `REFLEDGER_GITHUB_TOKEN` (fine-grained PAT, public-repo read). Pushes use the job `GITHUB_TOKEN`. The signing key is Environment secret `REFLEDGER_SIGNING_KEY` on Environment `ledger` (main only), never a repository-level secret.
- **Budget:** per-run cap raised to **300** requests and **120** new peels; secondary governor still **300** points/minute. Each run is **two-phase**: phase 1 does conditional repo metadata + tag listing for every poll group; phase 2 spends the remainder on peels and `action.yml` / compare. Detection is never starved by warm-up.
- **Population:** 36 poll groups (35 ecosystem repositories plus the canary).
- **Freeze:** code on `main` is frozen for seven days from the first clean seal (`FREEZE.md`). Docs may still change.

### 2026-09-30 - Seal publish retry

- **`.gitignore`:** top-level `/data/` no longer ignores `data/log/` (negated so sealed ledger paths can be `git add`ed on `main`).
- **Publish failures:** recorded under `state/publish_failures.jsonl` on the `data` branch; every poll retries a pending publish (fast-forward only) until it succeeds, then clears the pending entry and notes the recovery on the next ObservationDigest.