# Public signing key

Verifiers pin the Refledger Ed25519 public key with `refledger-verify --pubkey <hex>`.
The value below is the genesis key; replace the placeholders after running
`refledger-poller keygen` and before the first sealed head is published.

| Field | Value |
|---|---|
| `public_key` (64 lowercase hex) | `_REPLACE_WITH_PUBLIC_KEY_HEX_` |
| `key_id` | `_REPLACE_WITH_KEY_ID_` |

Generate a key outside the repository, for example:

```bash
cargo run --release -p refledger-poller -- keygen --out ~/.refledger/signing.key
```

Then paste the printed `public_key` and `key_id` into this table. Keep the seed
file offline and encrypted (`docs/KEY-BACKUP.md`); never commit it.
