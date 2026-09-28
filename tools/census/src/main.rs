//! Census CLI stub — full expand lands when action.yml blob archives exist.
//!
//! See README.md for code-search limit encoding and seeding rules.

fn main() {
    eprintln!(
        "refledger-census: seed file is population/watched.jsonl; \
         run expand once blob cache is populated (see README)."
    );
    eprintln!(
        "code-search: docs=10rpm/1000 results; live unauth=401 (2026-09-28). \
         Not used for ranking."
    );
}
