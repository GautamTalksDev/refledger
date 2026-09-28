//! Independent Refledger verifier library.
//!
//! Canonicalisation and chain replay are implemented from docs/LOG-FORMAT.md
//! alone. This crate must never depend on `refledger-log`.

pub mod canonical;
pub mod verify;

pub use canonical::{canonical_json, CanonError};
pub use verify::{
    entry_hash, failure_report, load_jsonl_dir, verify_chain, verify_heads_jsonl, verify_strict,
    verify_observation_digests, verify_signed_head, ChainVerdict, Failure, SignedHeadFile,
    VerifyError,
};
