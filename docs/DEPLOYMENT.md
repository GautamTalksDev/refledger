# Deployment

## Open question 2 — closed 2026-09-28

**Question:** can the poller run on an ephemeral serverless substrate (for example Cloudflare Durable Objects), or does it need a VM with a persistent disk?

**Answer:** a VM with a persistent disk. Closed by the store implementation, not by preference.

The store takes an exclusive lock file on open (`AlreadyLocked` if a second process tries). Every append fsyncs the file; creating a new daily file also fsyncs the directory. Crash recovery moves a torn final line aside and refuses to start on any other corrupt tail. Those three rules assume a single writer and durable local directories. Durable Objects — and any multi-replica ephemeral filesystem — cannot provide them. Deploying elsewhere is a redesign of the store, not a config change.

M1 runs on one VM. The seven-day exit condition is measured there.

## Pre-genesis operator checklist

1. **Name:** product is Refledger (`log_id` = `refledger`). Repos live under personal account `GautamTalksDev` (see `docs/NAMING.md`); claim domain, npm, and PyPI before genesis.
2. **Policy URL:** User-Agent contact must resolve to `OPERATIONS.md`. Default is the raw GitHub URL on `GautamTalksDev/refledger` until `refledger.dev` is live. The poller refuses to start otherwise (fatal only pre-genesis).
3. **Canary:** create [`GautamTalksDev/canary`](https://github.com/GautamTalksDev/canary) under the same account as its own repository (do not nest it inside Refledger; do not move it later, a transfer is a `RepoRedirected` mid-archive).
4. **Signing key:** offline age-encrypted backup off the VM (`docs/KEY-BACKUP.md`).
5. **Observation archive:** configure R2 (`REFLEDGER_R2_*`) so each seal uploads that day's observation JSONL. Failures appear in the next ObservationDigest note.

