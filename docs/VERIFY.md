# Verify the ledger

The verifier is an independent implementation of [`LOG-FORMAT.md`](LOG-FORMAT.md). It shares no code with `refledger-log`. That is deliberate: a bug in the signer that also lives in a shared helper would pass every check. Two crates, one written specification, and the conformance vectors under `tests/vectors/` are the whole contract.

## Quick check

```bash
cargo run --release -p refledger-verify -- data/log --strict \
  --pubkey b3e7e795c35dee53731e039b76da930fc54e87e2edc632449a8a2e55252e276a
```

Pin the key from [`PUBLIC-KEY.md`](PUBLIC-KEY.md). On success the tool prints at most five lines: chain status, entry count and sequence range, time span, signed head status, and how many coverage gaps were recorded. On failure it names the check and, when possible, the sequence number.

Exit code `0` means the verdict is OK. Exit code `1` means a verification failure. Exit code `2` means I/O, parse, or usage error.

## What it checks

| Check | What fails it |
|---|---|
| Contiguous `seq` from genesis | A gap, a duplicate, or a reorder |
| Genesis shape | First entry is not `seq` 0 with the fixed sixty four zero `prev_hash` |
| `prev_hash` linkage | Any entry whose predecessor hash is wrong |
| `entry_hash` | Canonical JSON of the entry (without `entry_hash`) does not hash to the stored value |
| `format_version` | Anything other than `1` |
| Correlation members | A `member_seqs` value greater than or equal to the correlation's own `seq` |
| Observation digests | Optional: when `--observations` is set, file hashes and counts disagree with a digest entry |
| Signed heads | Ed25519 signature over `canonical_json(head)` fails, or `seq` / `entry_hash` disagree with the log |
| `key_id` | Does not match SHA 256 of the raw 32 byte public key |
| `--strict` witness backlog | A head older than 48 hours still has no Rekor `log_index`, or a digest note records that backlog |

`--strict` also reads `heads.jsonl` beside the log directory when `--head` is not set, and verifies every line in that file.

## Flags

| Flag | Meaning |
|---|---|
| `<LOG_DIR>` | Directory of day JSONL files (for example `data/log`) |
| `--from` / `--to` | Inclusive sequence range |
| `--head` | A single signed head JSON file, or a `heads.jsonl` path |
| `--pubkey` | Expected public key (64 lowercase hex chars); requires `--head`, or `--strict` which reads `heads.jsonl` |
| `--observations` | Root of observation JSONL archives for digest checks |
| `--json` | Machine readable verdict |
| `--strict` | Fail on a 48 hour Rekor witness backlog |

## Independence

`refledger-verify` must not depend on `refledger-log`, not as a path dependency and not through a shared helper crate. Both implementations must reproduce `tests/vectors/*.json` and `tests/vectors/heads/*.json`. Divergence between the two suites is the most valuable failure this repository can produce.

## Conformance

```bash
cargo test -p refledger-log --test conformance --test head_conformance
cargo test -p refledger-verify --test conformance --test head_conformance
```

If either suite fails, do not trust a green `--strict` run until both are green again.
