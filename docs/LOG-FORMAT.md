# Refledger log format

This document is the written specification that the signer and the independently written verifier both implement. Two people who have never spoken, implementing only from this document and the conformance vectors, must produce identical bytes for identical inputs.

They share this specification and the JSON wire format. They share no code.

---

## 1. Canonical JSON serialisation

Every value that is hashed or signed is first reduced to a single canonical UTF-8 byte string by the rules in this section. Any two conforming implementations must emit identical bytes for identical abstract values.

### 1.1 Objects

Object keys are sorted by Unicode code point, ascending. Sorting is over the key strings as sequences of Unicode scalar values; it is not locale-aware and does not use Unicode collation.

After sorting, the object is serialised as a JSON object with those keys in that order.

### 1.2 Whitespace

There is no insignificant whitespace. No spaces, tabs, newlines, or carriage returns appear except inside JSON string values where they are part of the string content (and then only if they are allowed unescaped content under the string rules below; control characters below U+0020 are never emitted unescaped).

Between tokens there is nothing. After `{` or `[` there is immediately the next token or the matching closer. After `:` there is immediately the value. After `,` there is immediately the next key or element. Before `}` or `]` there is immediately the previous token.

### 1.3 Strings

Strings are escaped per RFC 8259, with these additional constraints that remove RFC 8259’s remaining discretion:

- Use the shortest valid escape for each character that must be escaped.
- When a `\uXXXX` escape is used, the four hex digits are lowercase (`a`–`f`, not `A`–`F`).
- Only the following are escaped: quotation mark (`"`), reverse solidus (`\`), and control characters below U+0020.
- Control characters below U+0020 are escaped as `\u00XX` with lowercase hex, except where RFC 8259 defines a one-character escape that is shorter; then that shorter form is required: `\b`, `\f`, `\n`, `\r`, `\t`. (U+0008, U+000C, U+000A, U+000D, U+0009 respectively.)
- Solidus (`/`, U+002F) is never escaped.
- Characters U+0020 and above other than `"` and `\` are emitted as themselves in UTF-8, never as `\u` escapes.

### 1.4 Numbers

There are no floating point numbers anywhere in this format. Integers only.

Integer serialisation rules:

- Decimal digits only; no leading zeros (the integer zero is the single character `0`).
- No `+` prefix.
- No exponent notation.
- Negative integers begin with `-` followed by the decimal representation of the absolute value under the same rules (no `-0`; negative zero is not used).

### 1.5 Timestamps

Timestamps are RFC 3339 strings in UTC with exactly millisecond precision and a trailing `Z`. They are never numbers.

Required form: `YYYY-MM-DDTHH:MM:SS.sssZ`

- Four-digit year, two-digit month, two-digit day.
- Literal `T`.
- Two-digit hour (00–23), two-digit minute, two-digit second.
- Literal `.` followed by exactly three decimal digits for milliseconds.
- Literal `Z`. No other offset is permitted. No `+00:00`. No omission of fractional seconds. No more or fewer than three fractional digits.

### 1.6 Optional fields

Absent optional fields are omitted from the serialised object. They are never serialised as `null`.

A field that is present with a defined value is always included. A field that is absent is not a key in the object at all.

### 1.7 Encoding

The canonical byte string is UTF-8 with no BOM. No UTF-16. No UTF-32. No leading U+FEFF.

### 1.8 Why this section exists

This is the single most common place a hash-chained log silently breaks. Two implementations that disagree about key order or about null-versus-absent produce different hashes for identical data and the chain fails for a reason that looks like corruption.

---

## 2. Entry hashing

An entry is a JSON object that includes, among other fields specified by the log schema, `prev_hash` and (after hashing) `entry_hash`.

Computation:

1. Take the entry as an abstract object with the `entry_hash` field omitted (absent, not null).
2. Serialise that object under Section 1 to obtain `canonical_json(...)`.
3. Compute SHA-256 over those exact bytes.
4. Express the digest as the ASCII string `"sha256:"` concatenated with the 64-character lowercase hexadecimal encoding of the 32-byte digest.

That string is the entry’s `entry_hash`.

`entry_hash` is computed over the entry including `prev_hash`. The previous-link field is inside the hashed content. Omitting `entry_hash` from the hashed object is the only omission for this step.

---

## 3. Chain linkage

- Entry `seq` 0 is genesis. Its `prev_hash` is the string `"sha256:"` followed by exactly sixty-four ASCII zero characters (`0`). That is the fixed genesis predecessor; it is not the hash of any prior entry.
- For every entry with `seq` = n where n ≥ 1, that entry’s `prev_hash` equals the `entry_hash` of the entry with `seq` = n − 1.
- `seq` is a contiguous ascending integer starting at 0. There are no gaps ever. The next entry after `seq` = n is always `seq` = n + 1.

A verifier that sees a gap, a mismatched `prev_hash`, or a genesis `prev_hash` that is not the fixed sixty-four-zero form must reject the chain.

---

## 4. Head signing

A head object is exactly these four fields, no others:

- `seq` — integer; the sequence number of the latest entry covered by this head
- `entry_hash` — string; that entry’s `entry_hash`
- `recorded_at` — timestamp string per Section 1.5
- `log_id` — string; identifies the log instance

Serialise the head under Section 1. Sign those exact bytes with Ed25519. Encode the 64-byte signature as lowercase hexadecimal (128 hex characters, no `"sha256:"` prefix, no other framing).

The signed head is published as an object with exactly three fields:

- `head` — the head object above
- `signature` — the hex-encoded Ed25519 signature
- `public_key` — the corresponding Ed25519 public key, hex-encoded (32 bytes → 64 lowercase hex characters)

Verification: recompute `canonical_json(head)`, verify the Ed25519 signature over those bytes with `public_key`, and check that `head.entry_hash` and `head.seq` match the addressed log entry.

### 4.1 Key identifier

`key_id` is not a field of the signed head and is not covered by the signature in §4.

It is SHA-256 over the raw 32-byte Ed25519 public key — the key bytes, not their hex encoding and not a PEM wrapping — written as `sha256:` followed by 64 lowercase hex characters.

### 4.2 Key rotation

Key rotation is unspecified in v1. It must be designed before the first rotation, not during it.

A verifier that sees a `key_id` it has no pre-published rule for must fail closed. It must not invent a rotation procedure from the key material in the log.

### 4.3 Daily heads file

`heads.jsonl` (path `data/log/heads.jsonl`, beside the day files) is append-only. One JSON object per line. Each line contains:

- `head`, `signature`, `public_key` — the signed head from this section
- `key_id` — §4.1, required
- `rekor` — unsigned witness metadata. Not part of the signature. When a submission has been accepted, `log_index` is the integer index returned by the witness log. A line may instead carry `error` and omit `log_index`; that is a recorded failed attempt. Retries append a new line. They do not rewrite an earlier one, and a failed witness is not a reason to stop appending the chain.

Day D’s published head is the head over day D’s `observation_digest` entry (the first entry of day D+1’s log file). `head.entry_hash` equals that entry’s `entry_hash`. The head is not required to be the chain tip.

A verifier reading this file checks every line: the Ed25519 signature over `canonical_json(head)`, `key_id` against `public_key`, and `head.seq` / `head.entry_hash` against the log entry with that `seq`.

The witness submission itself is Sigstore Rekor `hashedrekord` `0.0.1`, `POST /api/v1/log/entries`. Current Rekor verifies Ed25519 hashedrekord signatures as Ed25519ph and accepts only a SHA-512 prehash for that algorithm, so the artifact hash sent to Rekor is SHA-512 of the canonical head bytes. The chain’s `entry_hash` remains SHA-256 as defined in §2. `rekor` on the heads line records that witness; it does not replace the §4 signature.

---

## 5. Corrections

A correction is a new entry with `event` equal to `"correction"`. It carries at least:

- `corrects_seq` — integer; the `seq` of the entry being corrected
- `reason` — string; human-readable explanation

Entries are never edited in place. The original entry remains in the chain with its original `entry_hash` and its original linkage.

A verifier that encounters a correction must still verify the corrected entry’s hash. The original entry remains part of the chain; the correction does not replace it, remove it, or absolve it of hash verification. The correction is an additional signed claim about that earlier entry, itself subject to the same hashing and linkage rules as every other entry.

---

## 6. Conformance vectors

The directory `tests/vectors/` contains input / expected-hash pairs that both the signer and the verifier must reproduce.

Adding a vector is mandatory whenever this format changes. A format change without a new vector is incomplete. Implementations that diverge from a published vector are non-conforming, even if they believe they follow this prose.

---

## 7. Format version (frozen at genesis)

Every entry carries a required top-level integer field `format_version`.

- For this specification, `format_version` is always `1`.
- **v1 is frozen at the genesis entry.** Changes before genesis are edits; changes after genesis are a new `format_version`, never an in-place edit of v1.
- A verifier that sees any value other than `1` on a v1 log must reject the entry.

### 7.1 Event types (v1)

`event` is one of:

- `move`, `deletion`, `recreation` — tag binding changes; require `from`, `to`, `classification`, `severity`, `observation_window_seconds` > 0, and `source_observations` (array of observation ids, length ≥ 2).
- `recreation` additionally requires `gap_seconds`.
- `repo_unavailable`, `repo_redirected` — repository identity events; require `http_status`. Redirected also requires `redirect_location`.
- `correlation` — batch correlation as its **own** entry (never an edit of earlier Move entries). Requires a `correlation` object with `batch_id`, `member_seqs` (every element strictly less than this entry’s `seq`), `refs_moved_together`, `all_to_same_target`, and optional fixed-string `note`.
- `observation_digest` — one per UTC day; commits to that day’s observation JSONL files under signature. Requires an `observation_digest` object with `date`, `repos_polled`, `ok`, `not_modified`, `failed`, `skipped`, and `files` (sorted by `path` ascending; each `{ path, sha256 }`). Optional `note` records a recovery annotation (a torn write preserved beside the log). Omitted when absent; never null. A day with no observations still has a digest — all counts zero and `files` empty. Absence of the digest is not how a quiet day is recorded.
- `population_change` — watched-population membership change. Requires top-level `repo` and a `population_change` object with `change` (`added` | `removed`), `reason` (`seed` | `transitive` with `via` = `owner/repo[@path]@commit` | `manual` | `restored`), optional `path` (subdirectory action), and optional `note`. Removal never deletes history; it records that observation of this `(repo, path)` stops here.
- `correction` — as in §5.

### 7.2 Severity and ref form (v1)

- `severity`: `high` | `medium` | `low` | `info`
- `ref_form` (optional on binding events): `floating_major` | `floating_minor` | `exact` | `named_channel` | `other`
- `ancestry` (optional on moves): `ahead` | `behind` | `diverged` | `identical`

### 7.3 Diff truncation flag (v1)

`diff.diff_possibly_truncated` is a required boolean on every Diff object. When true, `paths` must not be treated as a complete file list (GitHub’s compare API caps page-1 files; exactly 300 files is indistinguishable from truncation).
