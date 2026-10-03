# Incident summaries

Plain-language accounts of each row in [INCIDENTS.md](INCIDENTS.md),
plus the deferred-move disclosure from commit `bc6af090`.
The technical note repeats the evidence. These summaries do not add facts.

## false-422

### When

29 Sep 2026

### Title

We recorded an error GitHub never sent

### What happened

When our poller ran out of its request allowance, it wrote down an error code that GitHub never actually sent, on one observation of tj-actions/changed-files.

### Effect on watched actions

None. No tag move was signed from it. Entry 33 lists that observation when the repo was added to the watched set; that add does not use the error code.

### What changed

Running out of allowance is now recorded as exactly that (`e30f6c7`), and the first signed digest disclosed it.

### Technical details

Observation `01M3Q896RH64XBABMKK8AXKNJ1` recorded `http_status` 422 that was fabricated by a poller bug (budget exhaustion misreported). Disclosed in an ObservationDigest note naming the observation id. Commit `e30f6c7`. The observation was left unchanged. Entry 33 (`population_change` Added for `tj-actions/changed-files`) lists that observation in `source_observations`.

## tree-sha-listing

### When

29 Sep 2026

### Title

Some early records stored the wrong content fingerprint

### What happened

On the first day, 32 observations stored the wrong content fingerprint for tags: the commit id for lightweight tags, and made-up values for annotated tags. Those rows cover 2,711 tags (1,921 with the commit id stored as the fingerprint, and 790 with made-up values).

### Effect on watched actions

The wrong fingerprints remain on those early records. No move of a watched action was signed from them. A later canary mistake used the same class of bad fingerprint (see the canary tag change below).

### What changed

Observations are never edited, so they stay as they were. A signed note lists every affected one, and fingerprints are now only trusted when fetched directly.

### Technical details

Phase-1 listing used the commit SHA as `tree_sha` for lightweight tags, or invented placeholders for annotated tags. Disclosed in a digest note and in `docs/tree-sha-affected-observations.txt` (32 observation ids). Commits `07c049c` and note `ce469fd`. Observations were not edited. Poller 0.1.1 (commit `2206c0b`) trusts trees only from the object cache.

## invented-placeholder-shas

### When

29 Sep 2026

### Title

Placeholder values were stored as real ones

### What happened

The same early code wrote placeholder values where real commit ids belonged. They point at nothing that exists.

### Effect on watched actions

They could be treated as real commit ids. No move of a watched action was signed from them.

### What changed

A signed note names it, and every row is listed in the affected observations file.

### Technical details

The listing path invented `commit_sha=000…001` and `tree_sha=000…002`. They are not git objects. The skip landed in commit `b5e1b05`. The digest note was queued in `ea8aefb`. The rows are covered by `docs/tree-sha-affected-observations.txt` (placeholder column; 790 tags across 22 observations).

## gap-no-derive-2026-09-29

### When

29 Sep 2026

### Title

One version saved checks but never wrote moves down

### What happened

For a period on 29 September, the poller saved what it saw but skipped the step that turns changes into ledger entries.

### Effect on watched actions

Replaying the saved checks afterwards found no moves of watched actions in that window.

### What changed

Fixed, and a signed note names the gap. Days already sealed were not touched.

### Technical details

`once` stored observations but never ran classify, derive, and append, so there was no Move, Deletion, or Recreation until the fix. Commit `07c049c`. Digest note `gap-no-derive-2026-09-29`. Archive replay found 0 ecosystem tag moves in that window. Sealed days were not touched.

## enrich-outage

### When

1 Oct 2026

### Title

A failing step stopped the overnight checks

### What happened

From 00:02 UTC on 1 October, a failing step made every check stop early. The day's seal ran late, and one move of our own canary was lost from memory when the process exited.

### Effect on watched actions

None. Only the canary was affected, and its move was recovered as entry 44.

### What changed

Moves waiting to be recorded are now saved to disk, so a crash cannot lose them.

### Technical details

Polls from about 00:02 UTC aborted on enrich compare failure. 30 September stayed unsealed. The canary Move was buffered in memory and lost until the durable buffer and re-derive. Commits `b5e1b05`, `a367afc`, and `ea8aefb`. Disclosed in the changelog and the freeze record. The canary Move was recovered as seq 44.

## heads-line-rewrite-2026-10-01

### When

1 Oct 2026

### Title

A published file was rewritten instead of added to

### What happened

Publishing replaced one line of the signed heads file instead of adding a new one. The signed data was identical. Only the witness details changed, and the witness record itself was never lost.

### Effect on watched actions

None. Both versions remain in the public history, and nothing was force pushed.

### What changed

Publishing now refuses any file that is not the previous file plus new lines.

### Technical details

On `main`, commit `ff75e915` replaced the seq 42 heads line from `0800c1f5` (with `log_index` 3027764712) with a 409 error line via a wholesale file publish. Head and signature bytes were identical in both. The Rekor entry was never lost. `93211084` re-appended the index. Both versions remain in git history. Digest note `heads-line-rewrite-2026-10-01`. Commit `fcaecb5`. Publish is now prefix-checked and refuses a non-append. No force-push.

## batch-listing-only-prior

### When

1 Oct 2026

### Title

A canary batch move went unrecorded

### What happened

At 02:18 UTC, three canary tags moved together to one commit. We had not yet fully looked up where they pointed before, and the poller missed the move.

### Effect on watched actions

None. This is what the canary exists to catch. The batch was recovered from the saved checks.

### What changed

Any change now triggers a full lookup first, and a test covers this exact case.

### Technical details

Canary batch `v9.0.0`, `v9.0.1`, and `v9.0.2` at 02:18 UTC: tips moved from listing-only to listing-only. Priority peel required a peeled prior binding, so there were no Moves and no Correlation. Commit `e3f3a7d`. Forward fix plus archive recovery peel. A reality-based `run_once` regression was added. Scheduler lag is scored as a gap. The canary bootstrap stops the rotation.

## tree-invariant-2026-10-02

### When

2 Oct 2026

### Title

A canary tag change was signed as a code change

### What happened

Entry 52 said the code behind canary tag v2 changed. It had not. Only the tag's type changed, on the same commit. A wrong content fingerprint in an earlier record caused it, and entries 40 and 43 carry the same wrong value.

### Effect on watched actions

None this time, but the same mistake could have mislabelled a real maintainer's release as a High severity change.

### What changed

Corrections were added as entries 64, 65 and 66, and the originals were not edited. Fingerprints are now only trusted when fetched directly, and the seven day test restarted.

### Technical details

Seq 52 signed `content_change` at severity low for canary `refs/tags/v2`, lightweight to annotated, on commit `f77ccacd8e1ae035fba46dd24456469832ea7c36`. The `from` tree was that commit SHA. `rebuild_repo_state` classified archived Ok rows through `binding_from_ref`, which copied the observation field. Observation `01M3R8GXRYD65WM7MAW5NPZ1KR` (2026-09-30T04:17:49.805Z, poller 0.1.0) still carried the pre-fix listing value where `tree_sha` equalled `commit_sha`. `needs_peel` only fires when both commit and tree are absent, so the invented tree was treated as peeled. The next Ok (`01M3RBCK…`, tree `2a568525155695c904bafd7cd982565935ca97f9`) did not refresh the tree because the target SHA was unchanged. The annotated move then compared that invented tree to the real one. Seq 53 (annotated to lightweight) was correct because its `from` tree was seq 52's `to` tree. The same invented tree sits on seq 40 (deletion) and seq 43 (recreation `from`). Corrections were appended as seq 64, 65, and 66; entries were not edited. Digest note `tree-invariant-2026-10-02`. Commit `2206c0b`. Poller 0.1.1 trusts trees only from the object cache. Freeze exception. The M1 window restarted at 2026-10-03T00:00:00Z.

## peel-deferred-drop

### When

2 Oct 2026

### Title

Moves that could not be looked up in time were dropped

### What happened

If the poller saw a tag move but could not finish looking it up in the same run, the move was not signed, and nothing recorded that it happened. The new tip was archived without a full lookup. After the process exited, the next run rebuilt from that archive and no longer saw a tip change, so the move was never signed.

### Effect on watched actions

None in the production archive. Every tip change in the saved checks was compared with the signed chain, including changes that only appear when two runs are read in order. No watched action and no canary tag had a tip change of this shape that lacked a matching signed entry. The canary batch missed at 02:18 UTC on 1 October is a different defect, listed above. A test on the code from just before the fix shows the loss: run 1 fails the lookup, run 2 has budget, and run 2 still signs nothing.

### What changed

Such moves are now saved across runs, retried, and signed with their original time, and each delay is noted in the digest (`bc6af090`).

### Technical details

Regression test on main: `run_once_defers_move_when_peel_5xx_then_signs_with_first_seen`. Proven on `bc6af090^` with a `run_once` probe: seed a peeled tip, run 1 sees the tip move and peel returns 500, run 1 archives a listing-only Ok for the new tip and signs no Move, `state/pending_moves.jsonl` does not exist, run 2 opens a fresh `Store` with peel succeeding and still signs no Move. The loss is across process exit: rebuild from the archive does not retain a pending tip change, so the next process no longer sees OLD→NEW. Commit `bc6af090` persists tip changes in `state/pending_moves.jsonl`, soft-fails peel per tip on 5xx or budget exhaustion, signs later with the original `first_observed` and observation window, and records `pending-move-deferred:` notes for the next ObservationDigest. Archive replay: every Ok tip change (chronological per repo across all observation files) compared to signed Move/Deletion/Recreation; zero listing-only new-tip changes lacked a covering chain event for watched actions or the canary.
