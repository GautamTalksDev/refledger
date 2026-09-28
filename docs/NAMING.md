# Naming — Refledger

The product name is Refledger. Defaults that genesis will bake in:

- Crates: `refledger-log`, `refledger-verify`, `refledger-poller`
- `log_id`: `"refledger"`
- User-Agent: `refledger/<version> (+<contact URL>)`

The reason an earlier working name was abandoned before the first entry — and
why this document must not reintroduce it — is recorded in the dated note at
the top of [`LOG-FORMAT.md`](LOG-FORMAT.md).

## Claims (do before first entry)

| Surface | Name | Status to confirm |
|---------|------|-------------------|
| GitHub org | `refledger` | create org; repos `refledger/refledger`, `refledger/canary` |
| Domain | `refledger.dev` (preferred) or `refledger.org` | register; point `/operations` at OPERATIONS.md |
| npm | `refledger` | reserve / publish placeholder |
| PyPI | `refledger` | reserve / publish placeholder |

As of 2026-09-28 (unauthenticated probe): GitHub org/user `refledger` returned
404; npm and PyPI had no `refledger` package. Domains were not registered from
this environment — confirm at the registrar before relying on them.
