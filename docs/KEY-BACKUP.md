# Signing key backup

v1 has no key-rotation procedure. Losing the Ed25519 seed forces an undesigned
rotation during an incident. Back the key up **offline, encrypted, off the
runner**, before genesis.

## What is backed up

- The 32-byte Ed25519 seed, as 64 lowercase hex characters (the same encoding
  `REFLEDGER_SIGNING_KEY` uses).
- Never the plaintext seed on any network share, chat, or git remote.

## Procedure (before day one)

1. Confirm the live key is only the Environment secret `REFLEDGER_SIGNING_KEY`
   on the GitHub Environment named `ledger` (main branch only), and that the
   public key hex matches [`docs/PUBLIC-KEY.md`](PUBLIC-KEY.md).
2. From an offline operator machine that briefly holds the seed for backup
   only, run:

   ```bash
   ./tools/key-backup.sh /path/to/signing.key ./refledger-signing-key.age
   ```

   The script encrypts with [age](https://age-encryption.org/) to a recipient
   you control (`AGE_RECIPIENT`, or pass `-r` / an identity file).
3. Copy `refledger-signing-key.age` to **two** offline locations that are not
   any Actions runner disk (encrypted USB + printed QR of the age ciphertext,
   or a second age-encrypted copy in a password manager vault that never
   touches CI).
4. Verify restore on a throwaway machine:

   ```bash
   age -d -i "$AGE_IDENTITY" refledger-signing-key.age | wc -c   # expect 64 + newline
   ```

   Confirm the derived public key matches production. Do not leave the
   plaintext on that machine.
5. Record the backup date and locations in the operator runbook (not in git).

## What this does not do

- It does not rotate keys. Rotation remains unspecified in v1.
- It does not replace Rekor witnessing or publishing observations on the data branch.
