# Security

Two distinct concerns. Do not conflate them.

---

## Part 1: Reporting a vulnerability in Refledger

If you believe you have found a security vulnerability in Refledger itself (the crawler, the log, the verifier, the hosted service, or related infrastructure), report it privately.

**Contact (preferred):** open a private vulnerability report at
[github.com/GautamTalksDev/refledger/security/advisories/new](https://github.com/GautamTalksDev/refledger/security/advisories/new).
That form reaches the maintainer without a public issue.

**Contact (fallback):** email the account owner via the address listed on
[github.com/GautamTalksDev](https://github.com/GautamTalksDev) (profile "Email"
or the public profile README contact, when published).

**Acknowledgement:** We aim to acknowledge receipt within 72 hours.

**Scope:** Refledger software and services under our control. Out of scope: third-party repositories we observe, GitHub itself, and issues that are solely about public tag movement we did not cause.

**Safe harbour:** We will not pursue or support legal action against researchers who make a good-faith effort to follow this process, avoid privacy violations, avoid destruction of data, and give us a reasonable opportunity to respond before public disclosure of a Refledger vulnerability.

---

## Part 2: What we do when WE detect something

When Refledger observes a high-severity or otherwise notable tag movement that may indicate compromise or supply-chain risk, we do the following. This is decided in advance. It is not reconsidered under pressure.

### Policy

1. Contact the repository maintainer via their published security contact immediately.
2. Contact GitHub Security.
3. Wait 72 hours before publishing analysis, correlation or commentary.
4. The raw log entry publishes automatically and immediately regardless.

### Why point 4 is not a disclosure

The log is append-only. An operator who can suppress entries is running a database with good manners, not a transparency log, and the property that makes the record worth anything is exactly the property that forbids suppression.

The raw fact (that a public tag in a public repository now points at a different public commit) is already visible to anyone on GitHub the instant it happens. We are not revealing anything an attacker does not already know they did.

What we withhold for 72 hours is our interpretation: correlation across refs, severity assessment, attribution of pattern, and any statement about impact.

### What we never publish

We never name a suspected attacker. We never assert that a repository or maintainer is compromised. We never grade an action. We state what moved, when we observed it, and what the content difference was.

### Precedent

The overwhelming majority of tag movement is legitimate release engineering following GitHub's own documented recommendation that maintainers move major version tags. The aws-actions/configure-aws-credentials v4.3.0 event of 4 August 2025 is a documented benign example.
