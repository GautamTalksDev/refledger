//! Hash-chain tests (LOG-FORMAT.md §§2–3, 5).

use proptest::prelude::*;
use refledger_log::chain::{verify, Chain, ChainError, UnhashedEntry};
use refledger_log::entry::{Binding, Classification, Diff, Event, HashRef, RefType, Severity};
use serde_json::Value;
use tempfile::tempdir;
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

fn odt(
    year: i32,
    month: Month,
    day: u8,
    hour: u8,
    min: u8,
    sec: u8,
    millisecond: u16,
) -> OffsetDateTime {
    let time = Time::from_hms_milli(hour, min, sec, millisecond).expect("valid time");
    let date = time::Date::from_calendar_date(year, month, day).expect("valid date");
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn sha(digit: char) -> String {
    std::iter::repeat(digit).take(40).collect()
}

fn sample_from() -> Binding {
    let first = odt(2026, Month::January, 1, 0, 0, 0, 0);
    Binding::builder()
        .target_sha(sha('1'))
        .commit_sha(sha('2'))
        .tree_sha(sha('3'))
        .first_observed(first)
        .last_observed(first + Duration::days(10))
        .observation_count(4)
        .build()
        .expect("from")
}

fn sample_to() -> Binding {
    Binding::builder()
        .target_sha(sha('4'))
        .commit_sha(sha('5'))
        .tree_sha(sha('6'))
        .first_observed(odt(2026, Month::January, 11, 12, 0, 0, 0))
        .observation_count(1)
        .build()
        .expect("to")
}

fn draft_correction(corrects_seq: u64, reason: &str) -> UnhashedEntry {
    UnhashedEntry::correction(odt(2026, Month::June, 1, 0, 0, 0, 0), corrects_seq, reason)
}

fn draft_move(repo: &str) -> UnhashedEntry {
    UnhashedEntry::move_event(refledger_log::MoveDraft {
        recorded_at: odt(2026, Month::June, 2, 0, 0, 0, 0),
        classification: Classification::ContentChange,
        severity: Severity::High,
        repo: repo.to_owned(),
        ref_name: "refs/tags/v1".into(),
        ref_type_before: RefType::Annotated,
        ref_type_after: RefType::Annotated,
        from: sample_from(),
        to: sample_to(),
        observation_window_seconds: 3600,
        source_observations: vec![
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAW".into(),
        ],
        diff: Some(Diff {
            files_added: 1,
            files_removed: 0,
            files_modified: 0,
            files_renamed: 0,
            paths: vec!["a.rs".into()],
            diff_possibly_truncated: false,
        }),
    })
}

fn first_bad_seq(err: &ChainError) -> u64 {
    err.first_bad_seq()
        .expect("verification errors must name the first bad seq")
}

fn new_chain(name: &str) -> Chain {
    Chain::ephemeral(name)
}

fn entries_to_vec(chain: &Chain) -> Vec<u8> {
    serde_json::to_vec(chain.entries()).expect("serialise entries")
}

fn verify_bytes(bytes: &[u8]) -> Result<(), ChainError> {
    let entries: Vec<refledger_log::Entry> =
        serde_json::from_slice(bytes).map_err(|e| ChainError::Serde(e.to_string()))?;
    verify(&entries)
}

#[test]
fn genesis_has_zero_prev_hash_and_appending_genesis_twice_fails() {
    let mut chain = new_chain("genesis");
    let genesis = chain
        .append_genesis(draft_correction(0, "genesis note"))
        .expect("first genesis");
    assert_eq!(genesis.seq, 0);
    assert_eq!(genesis.prev_hash.as_str(), HashRef::GENESIS);
    assert!(
        genesis.entry_hash.is_some(),
        "genesis must carry a computed entry_hash"
    );

    let err = chain
        .append_genesis(draft_correction(0, "again"))
        .expect_err("second genesis must fail");
    assert!(
        matches!(err, ChainError::GenesisAlreadyPresent),
        "got {err:?}"
    );
}

#[test]
fn append_sets_prev_hash_to_previous_entry_hash() {
    let mut chain = new_chain("link");
    let g = chain
        .append_genesis(draft_correction(0, "g"))
        .expect("genesis")
        .clone();
    let n = chain
        .append(draft_move("acme/widgets"))
        .expect("seq 1")
        .clone();

    assert_eq!(n.seq, 1);
    assert_eq!(
        n.prev_hash,
        g.entry_hash.expect("genesis hashed"),
        "entry N prev_hash must equal entry N-1 entry_hash"
    );
}

#[test]
fn chain_of_1000_entries_verifies_from_genesis() {
    let mut chain = new_chain("thousand");
    chain
        .append_genesis(draft_correction(0, "g"))
        .expect("genesis");
    for i in 1..1000 {
        let draft = if i % 7 == 0 {
            draft_correction(i as u64 - 1, "periodic correction")
        } else {
            draft_move(&format!("acme/repo-{i}"))
        };
        chain.append(draft).expect("append");
    }
    assert_eq!(chain.len(), 1000);
    verify(chain.entries()).expect("1000-entry chain must verify");
}

#[test]
fn durable_append_writes_jsonl_with_fsync() {
    let dir = tempdir().expect("tempdir");
    let mut chain = Chain::open("durable", dir.path());
    chain
        .append_genesis(draft_correction(0, "g"))
        .expect("genesis");
    chain.append(draft_move("acme/widgets")).expect("append");

    let day = odt(2026, Month::June, 1, 0, 0, 0, 0);
    // Genesis recorded_at is June 1; move is June 2 — two day files.
    let path_g = dir.path().join("2026/06/01.jsonl");
    let path_m = dir.path().join("2026/06/02.jsonl");
    assert!(path_g.is_file(), "missing {path_g:?}");
    assert!(path_m.is_file(), "missing {path_m:?}");
    let g_lines = std::fs::read_to_string(&path_g).expect("read g");
    let m_lines = std::fs::read_to_string(&path_m).expect("read m");
    assert_eq!(g_lines.lines().count(), 1);
    assert_eq!(m_lines.lines().count(), 1);
    let _ = day;
}

#[test]
fn tamper_mutate_middle_repo_fails_naming_bad_seq() {
    let chain = build_chain(8);
    let bytes = entries_to_vec(&chain);
    let mut value: Value = serde_json::from_slice(&bytes).expect("json");
    let entries = value.as_array_mut().expect("array of entries");
    let middle = entries.get_mut(3).expect("seq 3");
    middle
        .as_object_mut()
        .expect("object")
        .insert("repo".into(), Value::String("tampered/repo".into()));

    let tampered = serde_json::to_vec(&value).expect("reserialise");
    let err = verify_bytes(&tampered).expect_err("repo mutation must fail verification");
    assert_eq!(first_bad_seq(&err), 3);
}

#[test]
fn tamper_swap_adjacent_entries_fails_naming_bad_seq() {
    let chain = build_chain(6);
    let bytes = entries_to_vec(&chain);
    let mut value: Value = serde_json::from_slice(&bytes).expect("json");
    let entries = value.as_array_mut().expect("array");
    entries.swap(2, 3);

    let tampered = serde_json::to_vec(&value).expect("reserialise");
    let err = verify_bytes(&tampered).expect_err("swap must fail");
    assert_eq!(first_bad_seq(&err), 2);
}

#[test]
fn tamper_delete_middle_entry_fails_naming_bad_seq() {
    let chain = build_chain(6);
    let bytes = entries_to_vec(&chain);
    let mut value: Value = serde_json::from_slice(&bytes).expect("json");
    let entries = value.as_array_mut().expect("array");
    entries.remove(2);

    let tampered = serde_json::to_vec(&value).expect("reserialise");
    let err = verify_bytes(&tampered).expect_err("delete must fail");
    assert_eq!(first_bad_seq(&err), 2);
}

#[test]
fn tamper_duplicate_entry_fails_naming_bad_seq() {
    let chain = build_chain(5);
    let bytes = entries_to_vec(&chain);
    let mut value: Value = serde_json::from_slice(&bytes).expect("json");
    let entries = value.as_array_mut().expect("array");
    let dup = entries[2].clone();
    entries.insert(3, dup);

    let tampered = serde_json::to_vec(&value).expect("reserialise");
    let err = verify_bytes(&tampered).expect_err("duplicate must fail");
    assert_eq!(first_bad_seq(&err), 3);
}

#[test]
fn tamper_change_seq_number_fails_naming_bad_seq() {
    let chain = build_chain(5);
    let bytes = entries_to_vec(&chain);
    let mut value: Value = serde_json::from_slice(&bytes).expect("json");
    let entries = value.as_array_mut().expect("array");
    entries[2]
        .as_object_mut()
        .expect("object")
        .insert("seq".into(), Value::from(99u64));

    let tampered = serde_json::to_vec(&value).expect("reserialise");
    let err = verify_bytes(&tampered).expect_err("seq mutation must fail");
    assert_eq!(first_bad_seq(&err), 2);
}

#[test]
fn correction_links_normally_and_does_not_invalidate_corrected_entry() {
    let mut chain = new_chain("correction");
    chain
        .append_genesis(draft_move("acme/widgets"))
        .expect("genesis move");
    let corrected_hash = chain.entries()[0].entry_hash.clone().expect("hashed");

    chain
        .append(draft_correction(0, "misclassified"))
        .expect("correction");

    verify(chain.entries()).expect("chain with correction verifies");

    let original = &chain.entries()[0];
    assert_eq!(original.event, Event::Move);
    assert_eq!(original.entry_hash.as_ref(), Some(&corrected_hash));
    assert_eq!(chain.entries()[1].event, Event::Correction);
    assert_eq!(chain.entries()[1].corrects_seq, Some(0));
    assert_eq!(
        chain.entries()[1].prev_hash,
        corrected_hash,
        "correction links like any other entry"
    );
}

#[test]
fn append_is_only_mutation_entries_exposed_immutably() {
    let mut chain = new_chain("immutable");
    let tip: &refledger_log::Entry = chain
        .append_genesis(draft_correction(0, "g"))
        .expect("genesis");
    let _: &refledger_log::Entry = tip;

    let entries: &[refledger_log::Entry] = chain.entries();
    assert_eq!(entries.len(), 1);

    let _: fn(&mut Chain, UnhashedEntry) -> Result<&refledger_log::Entry, ChainError> =
        Chain::append;
    let _: fn(&mut Chain, UnhashedEntry) -> Result<&refledger_log::Entry, ChainError> =
        Chain::append_genesis;
    let _: fn(&Chain) -> &[refledger_log::Entry] = Chain::entries;
    let _: fn(&[refledger_log::Entry]) -> Result<(), ChainError> = verify;
}

fn build_chain(n: usize) -> Chain {
    let mut chain = new_chain(&format!("build-{n}"));
    chain
        .append_genesis(draft_correction(0, "g"))
        .expect("genesis");
    for i in 1..n {
        chain
            .append(draft_move(&format!("acme/r{i}")))
            .expect("append");
    }
    verify(chain.entries()).expect("fresh chain verifies");
    chain
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_any_sequence_of_valid_appends_verifies(
        kinds in prop::collection::vec(any::<bool>(), 0..40)
    ) {
        let mut chain = new_chain("prop-append");
        chain
            .append_genesis(draft_correction(0, "g"))
            .expect("genesis");
        for (i, is_correction) in kinds.iter().enumerate() {
            let draft = if *is_correction {
                draft_correction(i as u64, "arb")
            } else {
                draft_move(&format!("acme/p{i}"))
            };
            chain.append(draft).expect("append");
        }
        prop_assert!(verify(chain.entries()).is_ok());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn prop_single_byte_mutation_anywhere_fails_verification(
        extra in 0usize..12,
        is_correction in prop::collection::vec(any::<bool>(), 0..12),
        byte_index in any::<usize>(),
        xor_bit in 1u8..=255u8,
    ) {
        let mut chain = new_chain("prop-mutate");
        chain
            .append_genesis(draft_correction(0, "g"))
            .expect("genesis");
        for (i, flag) in is_correction.iter().enumerate().take(extra) {
            let draft = if *flag {
                draft_correction(i as u64, "m")
            } else {
                draft_move(&format!("acme/m{i}"))
            };
            chain.append(draft).expect("append");
        }
        verify(chain.entries()).expect("pre-mutation");

        let mut bytes = entries_to_vec(&chain);
        prop_assume!(!bytes.is_empty());
        let idx = byte_index % bytes.len();
        bytes[idx] ^= xor_bit;

        let result = verify_bytes(&bytes);
        prop_assert!(
            result.is_err(),
            "single-byte mutation at {idx} must fail verify/parse, got {result:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_seq_values_contiguous_from_zero(
        kinds in prop::collection::vec(any::<bool>(), 0..50)
    ) {
        let mut chain = new_chain("prop-seq");
        chain
            .append_genesis(draft_correction(0, "g"))
            .expect("genesis");
        for (i, is_correction) in kinds.iter().enumerate() {
            let draft = if *is_correction {
                draft_correction(i as u64, "c")
            } else {
                draft_move(&format!("acme/c{i}"))
            };
            chain.append(draft).expect("append");
        }
        for (i, entry) in chain.entries().iter().enumerate() {
            prop_assert_eq!(entry.seq, i as u64);
        }
        if let Some(last) = chain.entries().last() {
            prop_assert_eq!(last.seq + 1, chain.len() as u64);
        }
    }
}
