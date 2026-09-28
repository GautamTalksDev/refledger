# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **2026-09-28 — project rename.** The product is now **Refledger**
  (`refledger-log`, `refledger-verify`, `refledger-poller`; default `log_id` =
  `"refledger"`). The earlier working name Tagwatch collided with
  [woefe/tagwatch](https://github.com/woefe/tagwatch) and was replaced before
  genesis so the signed chain never embeds the colliding name. The normative
  format specification remains `docs/LOG-FORMAT.md` (filename kept so existing
  citations continue to resolve); rename history lives here, not in the format
  doc.
