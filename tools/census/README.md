# Census tool (population seeding)

Builds and expands `population/watched.jsonl`. **Not** a Cargo workspace member
(same pattern as `tools/calibrate`).

## Hard rules

- **No HTML scraping** of GitHub “Used by” / dependents pages — forbidden by
  `OPERATIONS.md`.
- **Code search is not a ranking engine.** It can at best confirm candidates.

## Code-search limits (encoded 2026-09-28)

| Source | Observation |
|--------|-------------|
| Docs ([REST search](https://docs.github.com/en/rest/search/search)) | Authenticated code search: **10 req/min**; other search endpoints 30/min. Max **100 results/page**, **1,000 results** accessible per query. Auth required. |
| Live unauthenticated call | `GET /search/code?...` → **401** `Requires authentication` (User-Agent `refledger/0.1.0`, 2026-09-28). No token was available in this environment for an authenticated probe; the docs figure stands until a follow-up authenticated capture. |

These caps cannot enumerate or rank the Actions ecosystem. Census therefore
builds the seed from **citable lists** (official orgs, incident reports, ACM
REP dataset names, documented Marketplace APIs) and expands via
**action.yml transitive closure** (`refledger_poller::population`).

## Commands (planned)

```bash
# Expand closure from committed seeds + local action.yml blob cache
cargo run --manifest-path tools/census/Cargo.toml -- expand \
  --watched ../../population/watched.jsonl \
  --blobs ../../data/action-yml/
```

First expand must print **closure size** and **depth-cap cutoffs** into
`population/METHOD.md`.
