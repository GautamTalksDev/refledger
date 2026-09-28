# How Refledger works

From one HTTP request to a signed, publicly witnessed fact. This is the whole machine, stage by stage.

```mermaid
flowchart TD
    S["Scheduler<br/>the only thing that reads a clock"] --> P["Poll<br/>list tags with a stored ETag"]
    P -->|"304 nothing changed"| NM["NotModified observation"]
    P -->|"200 something changed"| R["Resolve<br/>peel tags to commits, trees, action.yml"]
    P -->|"403 or 429"| F["Failed observation<br/>plus one global pause"]
    R --> O[("Observation archive")]
    NM --> O
    F --> O
    O --> C["Classify<br/>pure function"]
    E["Enrich<br/>ancestry and diffs, cached forever"] --> C
    C --> D["Derive<br/>pure function"]
    D --> L[("Hash chain")]
    L --> H["Daily signed head"]
    H --> K["Sigstore Rekor"]
    O --> M[("Off machine mirror")]
```

Two design rules run through everything below:

1. **Record the boring stuff.** Unchanged polls, failed polls and skipped polls are all written down. A tag being stable for 148 days is only a claim if you can show the 148 days of observations.
2. **Keep the thinking pure.** Classification and derivation never touch the network or the clock. That's what makes the whole ledger reproducible from the raw observations.

---

## 1. Poll: asking GitHub cheaply, thousands of times

Every 60 seconds, for every watched repository, the poller asks GitHub one question: *what are your tags right now?*

It asks with the `ETag` from last time. If nothing changed, GitHub answers `304 Not Modified` with an empty body. That answer is almost free: it doesn't count against GitHub's hourly request budget, as long as the request is authenticated. Since tags rarely move, nearly every poll is a 304.

"Almost" is doing real work in that sentence. GitHub also enforces **secondary limits**, and a 304 still costs one point there. Those limits have no header, so you can't see how close you are. You only find out by being refused. So Refledger:

* runs a single points governor over *every* request it makes, capped at a third of GitHub's documented ceiling
* treats any refusal as one global pause, not a stampede of retries
* writes every refusal down as an observation, so the gap is visible in the data

At today's population that's about 33 points a minute against a ceiling of 900. Plenty of headroom, measured rather than hoped for.

### Every poll becomes an observation

```mermaid
flowchart LR
    Q["one poll"] --> A["Ok<br/>here are the tags"]
    Q --> B["NotModified<br/>same as before"]
    Q --> C["Failed<br/>GitHub refused or errored"]
    Q --> D["Skipped<br/>we chose not to ask, and why"]
```

Skipped observations carry a reason: budget exhausted, backing off after a refusal, scheduler running late, shutting down, or the poller simply wasn't running (recorded on restart as `PollerDown`). A gap is always a record, never a silence.

---

## 2. Resolve: turning names into content

A tag name points at an object. Refledger follows it all the way down:

```mermaid
flowchart LR
    N["tag v4.2.2"] --> TO["tag object<br/>(annotated tags only)"]
    TO --> CM["commit"]
    N -. "lightweight tags skip a hop" .-> CM
    CM --> TR["tree<br/>the actual files"]
    CM --> AY["action.yml<br/>what this action itself uses"]
```

Here's the trick that keeps this affordable: **git objects never change.** A commit hash always means the same commit. So every lookup from object to commit to tree is cached forever, with no expiry. Only the *tag to target* binding ever moves, and that's the one thing we poll for. A second sweep over unchanged tags costs zero extra requests.

`action.yml` is always fetched at the exact commit, never by tag name, so a tag moving mid request can't give us the wrong file.

---

## 3. Classify: what kind of move was that?

When a tag's target changes, Refledger compares the old and new at three depths:

| What differs | Classification | In plain words |
|---|---|---|
| The files | **Content change** | Different code runs now |
| The commit, not the files | **Commit metadata only** | History was rewritten, code is identical |
| Only the tag itself | **Release level only** | Re tagged, same commit, e.g. lightweight to annotated |

### The rule that actually separates attacks from maintenance

You might think "a tag that was stable for months suddenly moved" is the red flag. It isn't. Under GitHub's own convention, `v4` sits still for months and then moves forward on every release. That's the normal case. Flag it and you'd drown in false alarms.

The real signal is the **shape of the tag name**:

| Form | Examples | Supposed to move? |
|---|---|---|
| Floating major | `v4` | Yes, forward, on every release |
| Floating minor | `v4.2` | Yes, forward |
| Exact | `v4.2.2`, `v1.0.0-rc.1` | **Never** |
| Named channel | `main`, `latest` | Constantly |

And for floating tags, the second signal is **direction**. A floating tag moving to a newer commit that builds on the old one is maintenance. Moving backwards, or sideways onto unrelated history, is not.

### Severity

| Severity | When |
|---|---|
| **High** | An exact tag's content changes. A floating tag moves backwards or sideways. Any batch correlation. |
| **Medium** | An exact tag's commit metadata changes. A deleted tag comes back pointing somewhere new. |
| **Low** | A floating tag moves forward to new code: an ordinary release. |
| **Info** | Release level only changes, named channel moves, deletions. |

A routine `v4` release is Low no matter how long `v4` sat still. A single exact tag content change is High.

### Correlation: the attack signature

```mermaid
flowchart LR
    V1["v1.0.0"] --> X["one commit"]
    V2["v1.1.0"] --> X
    V3["v1.2.0"] --> X
    V4["...and 343 more"] --> X
    style X stroke:#d33,stroke-width:2px
```

When three or more **pre existing exact** tags in one repository move to the **same** commit within a 30 minute window, Refledger emits a correlation. That's the tj-actions shape and the Trivy shape. A normal release, where `v4` and `v4.2` jump to the commit that `v4.2.3` was just created on, doesn't trigger it, because new tags and floating tags don't count.

### Deleted, then back

A tag that disappears and later reappears isn't a new tag. Refledger keeps a tombstone for every deleted tag, indefinitely, and records the return as a single *recreation* carrying the gap and both bindings. Its stability history doesn't reset.

---

## 4. Derive and chain: facts become ledger entries

Classified events are turned into ledger entries by another pure function. Every entry carries `format_version: 1`. Move, deletion and recreation entries also:

* name at least two source observations (a move needs a before and an after)
* take their timestamp from the observation that detected them, never the wall clock
* state an **observation window**: we can only know a tag moved *between* two polls, never the exact second, and those entries say so

Entries are serialised as canonical JSON (sorted keys, no floats, no nulls, fixed timestamp precision), hashed with SHA 256, and linked:

```mermaid
flowchart LR
    G["entry 0<br/>genesis"] --> E1["entry 1<br/>prev = hash of 0"]
    E1 --> E2["entry 2<br/>prev = hash of 1"]
    E2 --> E3["entry 3<br/>prev = hash of 2"]
    E3 --> ED["..."]
```

Entries are never edited. A mistake gets a new *correction* entry that points at the old one, and the old one stays in the chain.

Correlations that build up across several polls can't be written as one entry after the fact. So the individual moves are logged as they happen, and a separate correlation entry is appended once the pattern is clear, pointing back at them.

---

## 5. The daily seal

Once a day, at midnight UTC, the day gets closed and signed:

```mermaid
sequenceDiagram
    participant S as Scheduler
    participant St as Store
    participant R as Rekor
    participant M as Mirror
    Note over S: 00:00 UTC
    S->>S: stop dispatching, drain in flight requests
    S->>St: seal day D
    St->>St: hash every observation file from day D
    St->>St: append ObservationDigest<br/>as first entry of day D+1
    St->>St: append buffered day D+1 events
    St->>St: sign a head over that digest
    St->>R: submit head for witnessing
    R-->>St: log index or recorded error
    St->>M: upload day D observation files
```

The **ObservationDigest** is the clever bit. Observations are far too numerous to chain one by one, so instead each day's digest records the hash of every observation file plus counts of ok, not modified, failed and skipped polls. That puts the raw evidence under the signature. Every stability claim in the ledger now traces to signed data.

A day with zero observations still gets a digest, with zeros in it. A missing day and a quiet day must never look the same.

---

## 6. Three witnesses

| Witness | What it protects against |
|---|---|
| **The hash chain** | Editing, deleting or reordering any entry |
| **Published git history** | Rewriting the chain and re signing it quietly once the log is published |
| **Sigstore Rekor** | Rewriting git too. Rekor is a public log we don't control |

A head that goes more than 48 hours without a Rekor witness is noted in the next digest and fails `refledger-verify --strict`.

---

## 7. Knowing it works: the canary

A week of silence from well behaved actions would prove nothing. So Refledger watches a repository we control, [`canary`](https://github.com/GautamTalksDev/canary), whose tags get moved on purpose, on a schedule, in every pattern above: forward releases, exact tag changes, rewritten commits, delete and recreate, and a full batch attack shape.

Each deliberate move is logged in the canary's own ledger. Joining that ground truth against the Refledger chain gives a real, measured detection latency, published in [`DETECTION.md`](DETECTION.md). Few monitors publish that number. This one does.

---

## 8. Honest limits

* **Timing is a window.** We know a tag moved between two polls, roughly a minute apart, not the exact moment.
* **Coverage is a defined population.** We watch what's in [`population/`](../population/), not all of GitHub, and we say exactly what that is.
* **Restarts leave gaps.** They're recorded, never hidden.
* **We report, we don't judge.** Refledger will never call an action safe or compromised.
