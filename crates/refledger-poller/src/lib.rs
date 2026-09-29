//! Poller crate: observation model, conditional-request ETag store, rate-limit
//! accounting, content-addressed tag resolution, and pure event classification.
//!
//! Observations are raw material and are **not** signed. Only derived log
//! entries enter the hash chain — chaining observations themselves is
//! impractical at poller volume and is not the design.

#[path = "observe.rs"]
pub mod observation;

pub mod archive;
pub mod classify;
pub mod derive;
pub mod enrich;
pub mod github;
pub mod identity;
pub mod once;
pub mod population;
pub mod publish;
pub mod scheduler;
pub mod store;

pub use archive::{
    format_archive_failure_note, ArchiveFailure, DayArchive, MirrorArchive, NoopArchive,
    ObservationArchive,
};
pub use identity::{
    apply_contact_reachability, contact_url_from_ua, ensure_contact_resolves,
    format_contact_warning, probe_contact_url, refuse_to_poll_unless_identified, user_agent,
    user_agent_with, validate_user_agent, IdentityError, DEFAULT_CONTACT_URL, DEFAULT_LOG_ID,
    VERSION,
};
pub use once::{
    infer_scheduled_slot, poller_enabled, run_once, run_once_with, scheduled_time_from_env,
    CountingTransport, OnceArgs, OnceError, OnceReport, UreqTransport, ENABLED_VAR,
};
pub use publish::{
    format_publish_failure_note, GitLedgerPublisher, LedgerPublishPayload, LedgerPublisher,
    NoopPublisher, PublishFailure,
};

pub use derive::{derive, derive_observation_digest, ChainTip, DeriveError, ObservationDayStats};
pub use observation::*;
pub use population::{
    derive_population_change, derive_population_change_with_sources, earliest_for_key,
    expand_closure, extract_external_uses, fair_skip_offset, genesis_added_entries, load_watched,
    poll_groups, rotate_groups, save_watched, ActionRef, ClosureInput, ClosureResult,
    EarliestObservation, PopulationError, SeedSource, WatchedEntry, WatchedKey, WatchedReason,
    CLOSURE_DEPTH_CAP, PER_KEY_ADDED_FIX_COMMIT,
};
