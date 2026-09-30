# Detection latency

Measured by joining `GautamTalksDev/canary` ledger actions against the Refledger chain.
Only actions the poller could have seen are scored (after the canary's
PopulationChange Added at 2026-09-29T18:53:44.59Z; outside recorded PollerDown /
SecondaryLimitBackoff gaps). Canary events are excluded from public
ecosystem stats. Manual-intervention and retired-pattern rows are listed
separately and never enter latency figures.

**Generated:** 2026-09-30T05:46:48.333Z

## Summary

| Metric | Value |
|--------|-------|
| Ledger actions | 9 |
| Scored (event-producing) | 1 |
| Creation-only, not scored | 0 |
| Retired pattern, not scored | 4 |
| Manual intervention, not scored | 1 |
| Pre genesis, not scored | 3 |
| Performed during a recorded gap | 0 |
| Detected | 0 |
| Misclassified | 0 |
| p50 latency | — |
| p95 latency | — |
| max latency | — |

This is the figure OPERATIONS.md §7 promises to publish.

## Per-action (scored)

| pattern | tag | detected | classified | latency |
|---------|-----|----------|------------|---------|
| recreate | v3.0.0 | no | — | recreate half |

## Creation-only (not scored)

0 ledger row(s) whose pattern is not expected to produce a
Move/Deletion/Recreation (bootstrap / first-seen create). Reported separately
so they are never counted as detection misses.

| pattern | tag | performed_at |
|---------|-----|--------------|
| — | — | — |

## Retired pattern (not scored)

4 ledger row(s) from retired combined patterns
(`lightweight_annotated_roundtrip`, `delete_recreate`) that could not be
detected by design under the split-pattern canary. Not counted as misses.

| pattern | tag | performed_at |
|---------|-----|--------------|
| lightweight_annotated_roundtrip | v2 | 2026-09-30T04:17:41.000Z |
| lightweight_annotated_roundtrip | v2 | 2026-09-30T04:17:41.000Z |
| delete_recreate | v3.0.0 | 2026-09-30T04:18:35.000Z |
| delete_recreate | v3.0.0 | 2026-09-30T04:18:35.000Z |

## Manual intervention (not scored)

1 ledger row(s) marked by an appended correction as a manual
intervention (for example a remote delete performed by hand after a push
bug). Excluded from latency figures; the original ledger row is unchanged.

| pattern | tag | performed_at | reason |
|---------|-----|--------------|--------|
| delete | v3.0.0 | 2026-09-30T04:51:14.000Z | remote delete happened later by hand after the push bug; performed_at is not the true remote deletion time and must not enter latency figures |

## Pre genesis, not scored

3 ledger row(s) with `performed_at` at or before the canary PopulationChange Added entry (or before that entry exists). They are listed so a reader can see where they went; they are not detection misses.

| pattern | tag | performed_at |
|---------|-----|--------------|
| floating_major_forward | v1 | 2026-09-28T20:55:24.000Z |
| exact_content_change | v1.0.0 | 2026-09-29T05:51:30.000Z |
| commit_metadata_only | v1.0.1 | 2026-09-29T14:53:17.000Z |

## Performed during a recorded gap

0 ledger row(s) whose `performed_at` falls inside a recorded PollerDown or SecondaryLimitBackoff gap for the canary poll group. Excluded from scoring is not the same as hidden.

| pattern | tag | performed_at | gap |
|---------|-----|--------------|-----|
| — | — | — | — |
