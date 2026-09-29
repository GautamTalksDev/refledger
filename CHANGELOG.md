# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **2026-09-29 - genesis, clock, hardening, freeze.** First continuous M1 day:
  genesis population entries and live polls on the `data` branch; Cloudflare
  Worker `refledger-clock` as the primary 5-minute dispatcher (Actions
  `schedule` as backup); read-only PAT for API reads and `GITHUB_TOKEN` for
  pushes; signing key only in Environment `ledger`; two-phase poll budget
  (detection before warm-up); zizmor-clean workflows and related security
  review follow-ups; dead R2/hmac archive path removed; store flock unlock
  made explicit for deterministic tests; Dependabot ignores for crypto and
  TypeScript majors; **`FREEZE.md`**: code on `main` frozen for seven days
  from the first clean seal (docs still allowed). Full verify of sealed
  `data/log/` on `main` waits until tonight's first seal.

- **2026-09-28 - project rename.** The product is now **Refledger**
  (`refledger-log`, `refledger-verify`, `refledger-poller`; default `log_id` =
  `"refledger"`). The earlier working name Tagwatch collided with
  [woefe/tagwatch](https://github.com/woefe/tagwatch) and was replaced before
  genesis so the signed chain never embeds the colliding name. The normative
  format specification remains `docs/LOG-FORMAT.md` (filename kept so existing
  citations continue to resolve); rename history lives here, not in the format
  doc.
