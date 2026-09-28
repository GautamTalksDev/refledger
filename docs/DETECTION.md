# Detection latency

Awaiting the seven-day M1 run.

This file is produced by `tools/canary-score` from
[GautamTalksDev/canary](https://github.com/GautamTalksDev/canary)'s
`canary/ledger.jsonl` (pass `--ledger` a local path or URL) joined against the chain.
Only actions after the canary's PopulationChange Added entry are scored; rows
during recorded PollerDown / SecondaryLimitBackoff gaps are reported separately.
Until that run completes, there is no published p50/p95/max.

OPERATIONS.md §7 links here once numbers exist.
