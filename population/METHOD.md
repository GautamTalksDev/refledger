# Population methodology

**Date:** 2026-09-28 
**Definition:** Refledger M1 watches a **seed set + its transitive composite-action closure**, not a popularity ranking. Competing with StepSecurity on count (3,000+) loses; competing on the population the Trivy incident proved matters - the transitive closure - is the point.

## Seed sources (citable)

Each line in `watched.jsonl` records its `reason`. Seeds come from:

1. **Official org actions** - `actions/*`, `github/*`, `docker/*`, `aws-actions/*`, `azure/*`, `google-github-actions/*` (representative, not exhaustive of every repo under those orgs).
2. **Incident reports** - actions named in the tj-actions, reviewdog, and Trivy (March 2026) supply-chain incidents, including `aquasecurity/setup-trivy` (the transitive path into `trivy-action`).
3. **ACM REP paper dataset** - actions named in that dataset may be added with `source.type = acm_rep_paper` as the census tool incorporates them.
4. **Marketplace** - only where a documented API listing exists (`source.type = marketplace`). Not scraped from HTML.

## What we deliberately do not do

- **Do not scrape GitHub “Used by” / dependents HTML.** `OPERATIONS.md` forbids HTML scraping.
- **Do not rank the ecosystem via code search.** Live check 2026-09-28: unauthenticated `GET /search/code` returns **401 Requires authentication**. Documented authenticated limits: **10 requests/minute**, **≤100 results/page**, **≤1,000 results accessible per query**. That cannot rank the ecosystem; at best it can confirm candidates. Encoded in `tools/census/README.md`.

## Closure rule

For every watched action, parse `action.yml` / `action.yaml` at each observed commit (blob content addressed by the SHA already recorded as `action_yml_sha` in rest resolution). Extract every external `uses:` in composite steps.

- Tag refs (`@v4`) and full commit SHAs both add the referenced `(repo, path)`.
- `./local` and `docker://` references are ignored.
- Cycles terminate (visited set).
- **Depth cap = 4.** Anything cut off is recorded (see cutoffs), never silently truncated.

## Closure size

### 2026-09-28 - default-branch expand (superseded)

Measured from each seed’s **default-branch** `action.yml` at `HEAD`. Result: **35** watched keys / **33** repositories. That undercounts: consumers run tags (`v3`, `v4`, `v4.2.1`), not `main`, and older tags can still reference actions that `main` has since dropped.

### 2026-09-28 - tag-commit expand (current)

Re-run over **observed tag commits**: `git ls-remote --tags --refs` for every seed, then `action.yml` / `action.yaml` at each picked tag SHA via `raw.githubusercontent.com`, BFS to depth 4. Floating major/minor tags always included; for repos with many exact tags, up to 30 additional patch tags sharing a floating major. Blobs cached under `/tmp/refledger-blobs` by `(repo, rev, path)`.

| Metric | Value |
|--------|-------|
| Seed count | 32 |
| Tag commits examined (picked) | 1299 |
| YAML files fetched | 1770 (1457 hits, 313 absent - old tags without an action file) |
| External `uses:` edges | 823 |
| Closure size after expand | **38** watched keys |
| Distinct repositories | **35** |
| Cutoffs at depth > 4 | 352 (recorded; not silently dropped) |

Keys added by closure beyond the 32 seeds:

- `actions/attest` and `actions/attest-build-provenance/predicate` (via `actions/attest-build-provenance` tag commits)
- `actions/cache/restore` and `actions/cache/save` (via `aquasecurity/setup-trivy`)
- `tj-actions/glob` and `tj-actions/json2file` (via older `tj-actions/changed-files` tags - absent from that action’s current default branch)

`poll_groups` still issues one ref listing per repository. Ecosystem poll count is **35** repositories (38 keys). With the Manual canary that is **36 poll groups** total.

## M1 population milestone

### 2026-09-28 - amended before the seven-day run (default-branch count)

The exit condition brought into this file was **500 actions for seven days**. Default-branch closure landed at 35. That number is superseded by the tag-commit expand below.

### 2026-09-28 - amended after tag-commit expand

**M1 population is 32 seeds plus tag-commit closure, 38 total (35 repositories polled), plus one Manual canary ([`GautamTalksDev/canary`](https://github.com/GautamTalksDev/canary), note `"canary"`) excluded from ecosystem stats.**

That is the number the seven-day run is for. It is not 500, and it is not whatever count a later poller process happens to have loaded. Changing it is a new dated section.

## M1 exit condition (restated 2026-10-01)

Seven consecutive days on GitHub Actions (`docs/DEPLOYMENT.md`) at a **5 minute** cadence with:

- chain verifies from genesis with `refledger-verify --strict`
- 7 ObservationDigests, 7 signed heads, 7 Rekor log indexes that resolve
- every canary action detected and correctly classified, p95 latency published in `docs/DETECTION.md`
- every coverage gap present as an observation, none inferred

**Window restart (2026-10-02):** the 2026-10-01 and 2026-10-02 M1 starts were abandoned after emergency poller fixes changed the instrument inside the window. Measurement restarts at the first UTC midnight after the latest exception , see `FREEZE.md`.

Scheduled runs can be delayed or dropped under GitHub load. Every miss is recorded as a gap (`SchedulerLag` with scheduled vs actual start). A restart or missed cron does not reset the seven-day count if the chain and digests show no unrecorded gap. A day with an unrecorded gap does.

## Ranking honesty

This population is **approximate and reproducible**. It is not “the top N Actions by usage.” An honestly approximate, cited seed + closure beats a precise-looking popularity list we cannot defend.
