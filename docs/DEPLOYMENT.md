# Deployment

## Open question 2 - closed 2026-09-28, re-closed 2026-09-29

**Question:** can the poller run on an ephemeral serverless substrate (for example Cloudflare Durable Objects), or does it need a VM with a persistent disk?

**Answer (2026-09-28):** a VM with a persistent disk. Closed by the store implementation, not by preference.

The store takes an exclusive lock file on open (`AlreadyLocked` if a second process tries). Every append fsyncs the file; creating a new daily file also fsyncs the directory. Crash recovery moves a torn final line aside and refuses to start on any other corrupt tail. Those three rules assume a single writer and durable local directories. Durable Objects, and any multi-replica ephemeral filesystem, cannot provide them. Deploying there is a redesign of the store, not a config change.

**Answer (2026-09-29):** GitHub Actions, not a long-lived VM. Each run checks out the `data` branch onto a real runner disk, runs `refledger-poller once --data ./state` (one sweep, then exit), and fast-forward commits the state back. Concurrency group `refledger-poller` with `cancel-in-progress: false` replaces the file lock: never two writers at once. The store code is unchanged; only the runtime around it moved. The primary trigger is the Cloudflare Worker clock; the workflow `schedule` is backup.

M1 measures the seven-day exit on this Actions cadence (every 5 minutes). **Code on `main` is frozen** for those seven days from the first clean seal (`FREEZE.md`); docs may still change.

## Operator checklist (post-genesis)

1. **Name:** product is Refledger (`log_id` = `refledger`). Repos live under personal account `GautamTalksDev` (see `docs/NAMING.md`).
2. **Policy URL:** User-Agent contact must resolve to `OPERATIONS.md`. Default is the raw GitHub URL on `GautamTalksDev/refledger` until `refledger.dev` is live.
3. **Canary:** [`GautamTalksDev/canary`](https://github.com/GautamTalksDev/canary) under the same account as its own repository (do not nest it inside Refledger; do not move it later, a transfer is a `RepoRedirected` mid-run).
4. **Signing key:** Environment secret `REFLEDGER_SIGNING_KEY` on the GitHub Environment named `ledger` (main branch only). Offline age-encrypted backup (`docs/KEY-BACKUP.md`). Never a repository-level secret once the Environment is live. Never echo the secret in logs.
5. **Read token:** repository secret `REFLEDGER_GITHUB_TOKEN` (fine-grained PAT, public-repo contents read). Used only for GitHub API reads so ETags survive across runs.
6. **Observation store:** the `data` branch holds observation JSONL and poller state publicly. There is no R2 mirror.
7. **Ledger publish:** after each seal, sealed day files and `heads.jsonl` are committed under `data/log/` on `main` using `GITHUB_TOKEN` (contents: write). Fast-forward only; never force. See [Ledger publish](#ledger-publish) below.
8. **Enable:** repository variable `REFLEDGER_ENABLED` equals the string `true`.

## Clock

GitHub's Actions scheduler alone was not enough for M1. After genesis, scheduled `poll` runs were unreliable while the canary rotator also missed slots. Archive days cannot be recovered, so an external clock triggers each poll and each canary rotation.

`clock/` is a Cloudflare Worker (`refledger-clock`) on the operator's main Cloudflare account. It has no `workers.dev` hostname, no routes, and no public HTTP API: every `fetch` returns 404. It registers two crons and switches on `event.cron`:

| Cron | Dispatches |
|---|---|
| `2-57/5 * * * *` (never the `:00` minute) | `GautamTalksDev/refledger` → `poll.yml` on `main` |
| `17 */4 * * *` | `GautamTalksDev/canary` → `canary.yml` on `main` |

Same `DISPATCH_TOKEN`, same retry (one retry after 5s on 5xx or network error; never on 4xx), same logging rules (status, target, attempt only; never the token).

The Actions `schedule` trigger in `.github/workflows/poll.yml` stays as a backup. Concurrency group `refledger-poller` with `cancel-in-progress: false` already prevents overlapping writers if both fire. The canary workflow has **no** Actions `schedule:`; only `workflow_dispatch` from this clock (plus manual runs).

### Token (`DISPATCH_TOKEN`)

Fine-grained personal access token with repository access to **both** `GautamTalksDev/refledger` and `GautamTalksDev/canary`:

| Permission | Access |
|---|---|
| Actions | Read and write |
| Contents | No access |
| Metadata | Read-only (required by GitHub) |

That is enough for `POST .../actions/workflows/{poll,canary}.yml/dispatches` and nothing else. Do not use `GITHUB_TOKEN` from Actions here; this secret lives in the Worker. Never commit the token. Never log it.

Fine-grained tokens expire in at most one year. Before expiry: mint a replacement with the same scope, set it with `wrangler secret put DISPATCH_TOKEN` from inside `clock/`, then revoke the old token.

Deploy and secret rotation are operator steps run from `clock/`. Agents must not run `wrangler deploy` or `wrangler secret put` against this account.

## Tokens and the ledger Environment

| Secret / var | Where | Role |
|---|---|---|
| `REFLEDGER_GITHUB_TOKEN` | repository secret | Read-only GitHub API (listings, peels) |
| `GITHUB_TOKEN` | Actions job token | Push `data` branch; push sealed `data/log/` on `main` |
| `REFLEDGER_SIGNING_KEY` | Environment `ledger` only | Ed25519 seed for daily heads |
| `REFLEDGER_ENABLED` | repository variable | Must be the string `true` |
| `DISPATCH_TOKEN` | Cloudflare Worker secret | Clock `workflow_dispatch` only |

## Ledger publish

The public repository is a witness. Sealed days must reach `main` or `cargo run --locked -p refledger-verify -- data/log --strict` on a fresh clone has nothing to check. **Until the first seal**, verify from the `data` branch day files instead (`docs/VERIFY.md`).

On Actions, authentication is the job's `GITHUB_TOKEN` with `contents: write`. Do not use a personal access token for pushes. A deploy key remains valid for a future VM path but is not required for the Actions poller.

Rules enforced in code:

- Commits touch only `data/log/` on `main` (day JSONL and `heads.jsonl`).
- Commit message is exactly `ledger: seal <YYYY-MM-DD> seq <n>`.
- Push is fast-forward only. A rejected push is never force-pushed; it is recorded in `state/publish_failures.jsonl`, retried on every subsequent poll until it succeeds, then noted on the next ObservationDigest (success after retry). Never wait for the next seal alone.
- A dirty tree outside `data/log/` refuses to publish.
- Publish failure never stops polling.
- Repository `.gitignore` must never ignore `data/log/` (CI checks `git check-ignore`).

Poller state (observations, ETag journal, object cache, in-progress day log) is committed on the separate `data` branch once per run with message `poll <actual start UTC> seq <n>`.
