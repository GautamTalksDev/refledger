# Deployment

## Open question 2 - closed 2026-09-28

**Question:** can the poller run on an ephemeral serverless substrate (for example Cloudflare Durable Objects), or does it need a VM with a persistent disk?

**Answer:** a VM with a persistent disk. Closed by the store implementation, not by preference.

The store takes an exclusive lock file on open (`AlreadyLocked` if a second process tries). Every append fsyncs the file; creating a new daily file also fsyncs the directory. Crash recovery moves a torn final line aside and refuses to start on any other corrupt tail. Those three rules assume a single writer and durable local directories. Durable Objects, and any multi-replica ephemeral filesystem, cannot provide them. Deploying elsewhere is a redesign of the store, not a config change.

M1 runs on one VM. The seven-day exit condition is measured there.

## Pre-genesis operator checklist

1. **Name:** product is Refledger (`log_id` = `refledger`). Repos live under personal account `GautamTalksDev` (see `docs/NAMING.md`); claim domain, npm, and PyPI before genesis.
2. **Policy URL:** User-Agent contact must resolve to `OPERATIONS.md`. Default is the raw GitHub URL on `GautamTalksDev/refledger` until `refledger.dev` is live. The poller refuses to start otherwise (fatal only pre-genesis).
3. **Canary:** create [`GautamTalksDev/canary`](https://github.com/GautamTalksDev/canary) under the same account as its own repository (do not nest it inside Refledger; do not move it later, a transfer is a `RepoRedirected` mid-archive).
4. **Signing key:** offline age-encrypted backup off the VM (`docs/KEY-BACKUP.md`).
5. **Observation archive:** configure R2 (`REFLEDGER_R2_*`) so each seal uploads that day's observation JSONL. Failures appear in the next ObservationDigest note.
6. **Ledger publish clone:** a dedicated checkout of `GautamTalksDev/refledger` on the VM (not the live data directory). After each seal, sealed `log/` files are copied into that clone under `data/log/`, committed as `ledger: seal <date> seq <n>`, and fast-forward pushed. See [Ledger publish (deploy key)](#ledger-publish-deploy-key) below.

## Ledger publish (deploy key)

The public repository is a witness. Sealed days must reach GitHub or `cargo run -p refledger-verify -- data/log --strict` on a fresh clone checks an empty folder.

**Never use a personal access token.** Create a deploy key with write access to this repository only.

1. On the VM (as the poller user):

   ```bash
   ssh-keygen -t ed25519 -f /var/lib/refledger/deploy_key -N "" -C "refledger-publish"
   ```

2. In GitHub: repo **Settings → Deploy keys → Add deploy key**. Paste
   `/var/lib/refledger/deploy_key.pub`. Enable **Allow write access**. Do not
   reuse this key on any other repository.

3. Clone once into the publish directory (separate from the live data root):

   ```bash
   GIT_SSH_COMMAND='ssh -i /var/lib/refledger/deploy_key -o IdentitiesOnly=yes' \
     git clone git@github.com:GautamTalksDev/refledger.git /var/lib/refledger/publish-clone
   ```

4. Configure the poller with:

   - `REFLEDGER_PUBLISH_CLONE=/var/lib/refledger/publish-clone`
   - `REFLEDGER_PUBLISH_DEPLOY_KEY=/var/lib/refledger/deploy_key`

5. Rules enforced in code:

   - Commits touch only `data/log/`.
   - Commit message is exactly `ledger: seal <YYYY-MM-DD> seq <n>`.
   - Push is fast-forward only. A rejected push is never force-pushed; it is
     recorded in the next ObservationDigest note and retried on the next seal.
   - A dirty tree outside `data/log/` refuses to publish.
   - Publish failure never stops polling.

