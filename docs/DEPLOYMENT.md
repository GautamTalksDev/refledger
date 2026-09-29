# Deployment

## Open question 2 - closed 2026-09-28, re-closed 2026-09-29

**Question:** can the poller run on an ephemeral serverless substrate (for example Cloudflare Durable Objects), or does it need a VM with a persistent disk?

**Answer (2026-09-28):** a VM with a persistent disk. Closed by the store implementation, not by preference.

The store takes an exclusive lock file on open (`AlreadyLocked` if a second process tries). Every append fsyncs the file; creating a new daily file also fsyncs the directory. Crash recovery moves a torn final line aside and refuses to start on any other corrupt tail. Those three rules assume a single writer and durable local directories. Durable Objects, and any multi-replica ephemeral filesystem, cannot provide them. Deploying there is a redesign of the store, not a config change.

**Answer (2026-09-29):** GitHub Actions, not a long-lived VM. Each scheduled run checks out the `data` branch onto a real runner disk, runs `refledger-poller once --data ./state` (one sweep, then exit), and fast-forward commits the state back. Concurrency group `refledger-poller` with `cancel-in-progress: false` replaces the file lock: never two writers at once. The store code is unchanged; only the runtime around it moved.

M1 measures the seven-day exit on this Actions cadence (every 5 minutes).

## Pre-genesis operator checklist

1. **Name:** product is Refledger (`log_id` = `refledger`). Repos live under personal account `GautamTalksDev` (see `docs/NAMING.md`); claim domain, npm, and PyPI before genesis.
2. **Policy URL:** User-Agent contact must resolve to `OPERATIONS.md`. Default is the raw GitHub URL on `GautamTalksDev/refledger` until `refledger.dev` is live. The poller refuses to start otherwise (fatal only pre-genesis).
3. **Canary:** create [`GautamTalksDev/canary`](https://github.com/GautamTalksDev/canary) under the same account as its own repository (do not nest it inside Refledger; do not move it later, a transfer is a `RepoRedirected` mid-archive).
4. **Signing key:** store as repository secret `REFLEDGER_SIGNING_KEY` (hex seed). Keep an offline age-encrypted backup (`docs/KEY-BACKUP.md`). Never echo the secret in logs.
5. **Observation store:** the `data` branch holds observation JSONL and poller state publicly. There is no R2 mirror for M1.
6. **Ledger publish:** after each seal, sealed `log/` files are committed under `data/log/` on `main` using `GITHUB_TOKEN` (contents: write). Fast-forward only; never force. See [Ledger publish](#ledger-publish) below.
7. **Enable:** leave workflow `.github/workflows/poll.yml` inert until ready. Genesis is flipping repository variable `REFLEDGER_ENABLED` to the string `true`. Do not set it in the workflow file.

## Ledger publish

The public repository is a witness. Sealed days must reach `main` or `cargo run -p refledger-verify -- data/log --strict` on a fresh clone checks an empty folder.

On Actions, authentication is the job's `GITHUB_TOKEN` with `contents: write`. Do not use a personal access token. A deploy key remains valid for a future VM path but is not required for the Actions poller.

Rules enforced in code:

- Commits touch only `data/log/` on `main`.
- Commit message is exactly `ledger: seal <YYYY-MM-DD> seq <n>`.
- Push is fast-forward only. A rejected push is never force-pushed; it is recorded in the next ObservationDigest note and retried on the next seal.
- A dirty tree outside `data/log/` refuses to publish.
- Publish failure never stops polling.

Poller state (observations, ETag journal, object cache) is committed on the separate `data` branch once per run with message `poll <actual start UTC> seq <n>`.
