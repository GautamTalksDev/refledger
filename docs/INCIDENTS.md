# Incidents

Disclosures of poller or publish defects that touched the public record or
observation archive. Sealed chain entries are never rewritten; each incident
is noted on a later ObservationDigest and listed here.

| Date (UTC) | Marker / id | Commit | What happened | How disclosed |
| --- | --- | --- | --- | --- |
| 2026-09-29 | false-422 (`01M3Q896RH64XBABMKK8AXKNJ1`) | `e30f6c7` | Observation recorded `http_status` 422 that was fabricated by a poller bug (budget exhaustion misreported). | ObservationDigest note naming the observation id; observation left unchanged. |
| 2026-09-29 | tree-sha listing (`docs/tree-sha-affected-observations.txt`) | `07c049c` / note `ce469fd` | Phase-1 listing used commit SHA as `tree_sha` (lightweight) or invent placeholders (annotated). | Digest note + `docs/tree-sha-affected-observations.txt`; observations not edited. |
| 2026-09-29 | invented-placeholder-shas | `b5e1b05` (skip) / note queued `ea8aefb` | Same listing path invented `commit_sha=000…001` / `tree_sha=000…002`; not git objects. | Digest note `invented-placeholder-shas`; covered by the tree-sha affected list (placeholder column). |
| 2026-09-29 | gap-no-derive-2026-09-29 | `07c049c` | `once` stored observations but never ran classify→derive→append; no Move/Deletion/Recreation until the fix. | Digest note `gap-no-derive-2026-09-29`; sealed days untouched. |
| 2026-10-01 | enrich / outage | `b5e1b05` / `a367afc` / `ea8aefb` | Polls from ~00:02Z aborted on enrich compare failure; Sep 30 unsealed; canary Move buffered in memory and lost until durable buffer + re-derive. | CHANGELOG + FREEZE exceptions; canary Move recovered as seq 44. |
| 2026-10-01 | heads-line-rewrite-2026-10-01 | `fcaecb5` | On `main`, commit `ff75e915` replaced the seq 42 heads line from `0800c1f5` (with `log_index` 3027764712) with a 409 error line via wholesale file publish. Head and signature bytes were identical in both; Rekor entry never lost; `93211084` re-appended the index. Both versions remain in git history. | Digest note `heads-line-rewrite-2026-10-01`; this file; publish is now prefix-checked (refuse non-append). No force-push. |
| 2026-10-01 | batch-listing-only-prior | e3f3a7d | Canary batch `v9.0.0/1/2` at 02:18Z: tips moved listing-only→listing-only; priority-peel required a peeled prior binding, so no Moves/Correlation. | Forward fix + archive recovery peel; reality-based `run_once` regression; SchedulerLag scored as gap; canary bootstrap stops the rotation. |

Force-pushing history to erase a rewritten heads line would turn a bug into a cover-up. Disclosure keeps both versions visible.
