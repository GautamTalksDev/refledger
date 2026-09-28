# Refledger

Append-only transparency log for GitHub git tag movement.

## Conformance vectors

The directory [`tests/vectors/`](tests/vectors/) holds input / expected-canonical /
expected-hash pairs that **both** `refledger-log` and `refledger-verify` must
reproduce. They share only this JSON wire format and [`docs/LOG-FORMAT.md`](docs/LOG-FORMAT.md)
— never code.

**Adding a vector is mandatory for any format change.** A change that cannot be
expressed as a new vector is a change that will break someone's verifier.

Regenerate entry-level vectors after an intentional format change:

```bash
cargo run -p refledger-log --example gen_entry_vectors
cargo test -p refledger-log --test conformance
cargo test -p refledger-verify --test conformance
```
