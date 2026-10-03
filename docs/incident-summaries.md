# Incident summaries

Plain-language accounts of each row in [INCIDENTS.md](INCIDENTS.md).
The technical note repeats that row. These summaries do not add facts.

## false-422

### When

29 Sep 2026

### Title

A poll recorded a status code the server never sent

### What happened

On 29 September 2026 the poller wrote HTTP status 422 on an observation when its request budget had run out, and that status was not returned by GitHub.

### Effect

The observation still shows 422, because sealed observations are not edited.

### What changed

A later signed observation digest names that observation and says the status was misreported.

### Technical details

Observation `01M3Q896RH64XBABMKK8AXKNJ1` recorded `http_status` 422 that was fabricated by a poller bug (budget exhaustion misreported). Disclosed in an ObservationDigest note naming the observation id. Commit `e30f6c7`. The observation was left unchanged.

## tree-sha-listing

### When

29 Sep 2026

### Title

Early listings stored the wrong tree id

### What happened

The first listing pass stored a commit id in the tree field for lightweight tags, and made up placeholder ids for annotated tags.

### Effect

Those observations still carry the bad tree ids, because observations are not edited after they are written.

### What changed

A digest note and the affected-observations list name every row, and the observations themselves were left as they were.

### Technical details

Phase-1 listing used the commit SHA as `tree_sha` for lightweight tags, or invented placeholders for annotated tags. Disclosed in a digest note and in `docs/tree-sha-affected-observations.txt`. Commits `07c049c` and note `ce469fd`. Observations were not edited.

## invented-placeholder-shas

### When

29 Sep 2026

### Title

Placeholder ids were stored as if they were git objects

### What happened

The same listing path wrote commit id `000…001` and tree id `000…002`, and those values are not git objects.

### Effect

Anything that trusted those fields treated invented ids as commits and trees.

### What changed

Digest note `invented-placeholder-shas` records it, and the rows are also listed in the tree id file, in the placeholder column.

### Technical details

The listing path invented `commit_sha=000…001` and `tree_sha=000…002`. They are not git objects. The skip landed in commit `b5e1b05`. The digest note was queued in `ea8aefb`. The rows are covered by `docs/tree-sha-affected-observations.txt` (placeholder column).

## gap-no-derive-2026-09-29

### When

29 Sep 2026

### Title

One run stored observations and then stopped

### What happened

The `once` command saved observations but never classified them, derived entries, or appended them to the log.

### Effect

No move, deletion, or recreation from that run was recorded until the command was fixed.

### What changed

Digest note `gap-no-derive-2026-09-29` records the gap, and days that were already sealed were left untouched.

### Technical details

`once` stored observations but never ran classify, derive, and append, so there was no Move, Deletion, or Recreation until the fix. Commit `07c049c`. Digest note `gap-no-derive-2026-09-29`. Sealed days were not touched.

## enrich-outage

### When

1 Oct 2026

### Title

A failed compare aborted the night's polls

### What happened

From about 00:02 UTC on 1 October 2026, polls aborted when the compare step that enriches a move failed, the 30 September day stayed unsealed, and a canary move held only in memory was lost when the process exited.

### Effect

That canary move was missing from the log until it was derived again.

### What changed

The buffer is now durable, and the canary move was recovered as entry 44.

### Technical details

Polls from about 00:02 UTC aborted on enrich compare failure. 30 September stayed unsealed. The canary Move was buffered in memory and lost until the durable buffer and re-derive. Commits `b5e1b05`, `a367afc`, and `ea8aefb`. Disclosed in the changelog and the freeze record. The canary Move was recovered as seq 44.

## heads-line-rewrite-2026-10-01

### When

1 Oct 2026

### Title

A heads line was replaced instead of appended

### What happened

On the main branch, one commit replaced the entry 42 heads line with a 409 error line by publishing the whole file, even though the head bytes and the signature were the same and the Rekor entry was never lost.

### Effect

Both versions remain in git history, and a later commit appended the log index again.

### What changed

Publishing now refuses a heads file that is not an addition to the end of the previous file, and there was no force-push.

### Technical details

On `main`, commit `ff75e915` replaced the seq 42 heads line from `0800c1f5` (with `log_index` 3027764712) with a 409 error line via a wholesale file publish. Head and signature bytes were identical in both. The Rekor entry was never lost. `93211084` re-appended the index. Both versions remain in git history. Digest note `heads-line-rewrite-2026-10-01`. Commit `fcaecb5`. Publish is now prefix-checked and refuses a non-append. No force-push.

## batch-listing-only-prior

### When

1 Oct 2026

### Title

A canary batch moved and the log recorded nothing

### What happened

At 02:18 UTC on 1 October 2026 the canary tags v9.0.0, v9.0.1, and v9.0.2 moved while both the old tip and the new tip were still listing-only, and priority peel required a peeled prior binding.

### Effect

The log recorded no moves and no correlation for that batch.

### What changed

The forward fix peels on a target change, the archive was peeled to recover the batch, a regression covers this shape, scheduler lag counts as a gap, and the canary bootstrap stops the rotation.

### Technical details

Canary batch `v9.0.0`, `v9.0.1`, and `v9.0.2` at 02:18 UTC: tips moved from listing-only to listing-only. Priority peel required a peeled prior binding, so there were no Moves and no Correlation. Commit `e3f3a7d`. Forward fix plus archive recovery peel. A reality-based `run_once` regression was added. Scheduler lag is scored as a gap. The canary bootstrap stops the rotation.

## tree-invariant-2026-10-02

### When

2 Oct 2026

### Title

A canary tag change was signed as a content change

### What happened

Entry 52 signed a content change for canary tag v2 when the tag went from lightweight to annotated on the same commit, because an earlier observation had stored the commit id in the tree field and that invented tree was treated as already peeled.

### Effect

The entry overstated the change, the same invented tree still sits on the deletion in entry 40 and the previous side of the recreation in entry 43, and entry 53 was already correct because it compared against entry 52's new tree.

### What changed

Corrections were appended and the entries were not edited, poller 0.1.1 trusts trees only from the object cache, and the measurement window restarted under a freeze exception.

### Technical details

Seq 52 signed `content_change` at severity low for canary `refs/tags/v2`, lightweight to annotated, on commit `f77ccacd8e1ae035fba46dd24456469832ea7c36`. The `from` tree was that commit SHA. `rebuild_repo_state` classified archived Ok rows through `binding_from_ref`, which copied the observation field. Observation `01M3R8GXRYD65WM7MAW5NPZ1KR` (2026-09-30T04:17:49.805Z, poller 0.1.0) still carried the pre-fix listing value where `tree_sha` equalled `commit_sha`. `needs_peel` only fires when both commit and tree are absent, so the invented tree was treated as peeled. The next Ok (`01M3RBCK…`, tree `2a568525155695c904bafd7cd982565935ca97f9`) did not refresh the tree because the target SHA was unchanged. The annotated move then compared that invented tree to the real one. Seq 53 (annotated to lightweight) was correct because its `from` tree was seq 52's `to` tree. The same invented tree sits on seq 40 (deletion) and seq 43 (recreation `from`). Corrections were appended and entries were not edited. Digest note `tree-invariant-2026-10-02`. Commit `2206c0b`. Poller 0.1.1 trusts trees only from the object cache. Freeze exception. The M1 window restarted.
