# Rekor witnessing

## Live confirmation — 2026-09-28

Submitted `hashedrekord` `0.0.1` to `https://rekor.sigstore.dev/api/v1/log/entries`
with:

- artifact hash: **SHA-512** of `canonical_json(head)` (Ed25519ph only accepts a SHA-512 prehash under Rekor's verifier)
- signature: **Ed25519ph** (empty context) over those same head bytes, with the same Ed25519 key that signs heads with **pure Ed25519** for `heads.jsonl`
- public key: PKIX PEM (`BEGIN PUBLIC KEY`)

Result: **ACCEPTED** `logIndex=2985961439`, UUID
`108e9186e8c5677a6a8cd58941f884659d5d46d55419a4719593b7e3a90dfc7cf024faa4fc128e00`.
`GET /api/v1/log/entries?logIndex=2985961439` resolved the entry.

The pure Ed25519 signature used in `heads.jsonl` is a different signature and
must not be submitted to Rekor. A failed witness is non-fatal and retried; a
backlog older than 48 hours is recorded in the next ObservationDigest note and
fails `refledger-verify --strict`.

Reproduce: `cargo run --manifest-path tools/rekor-probe/Cargo.toml`.
