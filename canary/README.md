# Canary — deliberately moved tags for detection measurement

Public repository: **`refledger/canary`**.

This directory is the contents to push to that repo. Creating the remote
requires GitHub authentication (`gh auth login` or a token); the files here
are ready to publish.

## Purpose

Without a canary, seven days of silence on 35 official actions proves nothing
about detection. This repo moves tags on a schedule through every §6.2 pattern
and appends ground truth to `canary/ledger.jsonl`. `tools/canary-score` joins
that ledger against the chain and writes `docs/DETECTION.md`.

Canary events are tagged `note: "canary"` in `population/watched.jsonl` and
must never be counted as ecosystem movement on the public site.

## Patterns (rotated by the scheduled workflow)

1. FloatingMajor forward (`v1` → newer commit, ahead)
2. Exact ContentChange (`v1.0.0` → different tree)
3. CommitMetadataOnly (same tree, amended commit)
4. lightweight → annotated and back
5. delete, then recreate at a different commit after ~15 min
6. batch: 3 Exact tags moved to one commit

Each action appends `{pattern, tag, from, to, performed_at}` to
`canary/ledger.jsonl` in this repository.
