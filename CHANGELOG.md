# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- **2026-09-30 - attack-shaped tag moves would go unrecorded.** After the
  tree-SHA listing fix, a ref whose target changed (or reappeared after a
  tombstone) to a never-seen commit was stored listing-only, and classify
  skipped unpeeled refs. The tj-actions / Trivy attack shape — tags moved to
  a brand-new malicious commit — would therefore emit no Move. Every
  changed or reappeared tip is now peeled from the reserved budget before
  classify in the same run; confirm slots stay held aside from optional
  backfill. Correction entry for seq 40 (Deletion carried meaningless
  `content_change`; LOG-FORMAT v1 requires the field). Canary-score: retired
  patterns and manual-intervention corrections are listed separately and
  excluded from latency.

- **2026-09-30 - once never derived moves (gap-no-derive-2026-09-29).** The
  Actions `once` runner stored observations but never ran
  classify → enrich → derive → append, so the chain recorded no
  Move/Deletion/Recreation since genesis (tip stayed at population Adds).
  Archive replay found **0** ecosystem tag moves in that window; canary
  patterns 3 and 4 were not observationally recoverable. Forward-only fix:
  wire the same helper the library path uses into `run_once`; reserve
  confirm + enrich budget before phase-2 backfill; leave sealed days
  untouched. The next ObservationDigest carries note
  `gap-no-derive-2026-09-29`.

- **2026-09-30 - listing invented tree_sha.** Phase-1 Ok observations used
  the commit SHA as `tree_sha` (lightweight) or placeholder
  `000…001`/`000…002` (annotated). A commit's tree is immutable; those
  values were wrong. Listing now records cache trees only, otherwise
  listing-only refs with no invented tree. Affected archive observation
  ids are listed in `docs/tree-sha-affected-observations.txt` (not
  rewritten). The next ObservationDigest also notes that those
  `tree_sha` fields must not be relied on (same class as the false-422
  note).

- **2026-09-30 - seal publish to main.** `.gitignore` no longer ignores
  `data/log/` (it had blocked the first seal's `git add`). Pending publish
  failures retry on every poll until fast-forward succeeds, then the next
  ObservationDigest notes the recovery. CI guards `git check-ignore` for a
  sample `data/log/` path. Untracked parent `data/` on a fresh `main` is
  allowed during publish (git reports `?? data/` before `git add`). Publish
  auth strips embedded credentials from `origin` and uses
  `AUTHORIZATION: basic` (x-access-token) so the workflow's token URL and
  the header do not fight.

- **2026-09-30 - canary clock and verifier coverage line.** `refledger-clock`
  also dispatches the canary rotation (`17 */4 * * *`). Verifier coverage
  line sums `skipped`/`failed` from signed ObservationDigests.

### Changed

- **2026-09-30 - canary patterns 3 and 4 split across rotations.**
  `lightweight_to_annotated` / `annotated_to_lightweight` and
  `delete` / `recreate` each leave their intermediate state for at least
  one poll. Patterns act only on pre-existing bootstrap tags.
  `canary-score` scores only event-producing patterns and reports
  creation-only rows separately.

- **2026-09-29 - genesis, clock, hardening, freeze.** First continuous M1 day:
  genesis population entries and live polls on the `data` branch; Cloudflare
  Worker `refledger-clock` as the primary 5-minute dispatcher (Actions
  `schedule` as backup); read-only PAT for API reads and `GITHUB_TOKEN` for
  pushes; signing key only in Environment `ledger`; two-phase poll budget
  (detection before warm-up); zizmor-clean workflows and related security
  review follow-ups; dead R2/hmac archive path removed; store flock unlock
  made explicit for deterministic tests; Dependabot ignores for crypto and
  TypeScript majors; **`FREEZE.md`**: code on `main` frozen for seven days
  from the first clean seal (docs still allowed). Full verify of sealed
  `data/log/` on `main` waits until tonight's first seal.

- **2026-09-28 - project rename.** The product is now **Refledger**
  (`refledger-log`, `refledger-verify`, `refledger-poller`; default `log_id` =
  `"refledger"`). The earlier working name Tagwatch collided with
  [woefe/tagwatch](https://github.com/woefe/tagwatch) and was replaced before
  genesis so the signed chain never embeds the colliding name. The normative
  format specification remains `docs/LOG-FORMAT.md` (filename kept so existing
  citations continue to resolve); rename history lives here, not in the format
  doc.
