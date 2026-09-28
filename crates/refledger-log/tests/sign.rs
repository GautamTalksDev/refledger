//! Head signing tests (LOG-FORMAT.md §4).

use refledger_log::entry::{HashRef, Timestamp};
use refledger_log::sign::{
    load_signing_key, sign_head, verify_head, Head, KeySource, SignError, SigningKey,
};
use tempfile::NamedTempFile;
use time::{Month, OffsetDateTime, PrimitiveDateTime, Time};

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

fn sample_head() -> Head {
    Head {
        seq: 7,
        entry_hash: HashRef::parse(format!("sha256:{}", "ab".repeat(32))).unwrap(),
        recorded_at: Timestamp::from_offset_datetime(odt(2026, Month::September, 21, 12, 0, 0, 0))
            .unwrap(),
        log_id: "refledger-test".into(),
    }
}

fn key_a() -> SigningKey {
    SigningKey::from_seed_bytes(&[1u8; 32]).expect("key a")
}

fn key_b() -> SigningKey {
    SigningKey::from_seed_bytes(&[2u8; 32]).expect("key b")
}

#[test]
fn head_signs_and_verifies_with_matching_public_key() {
    let head = sample_head();
    let signed = sign_head(&head, &key_a()).expect("sign");
    verify_head(&signed).expect("verify with matching key material in SignedHead");
    assert_eq!(signed.head, head);
    assert_eq!(
        signed.public_key,
        hex::encode(key_a().verifying_key_bytes())
    );
}

#[test]
fn head_signed_with_key_a_fails_verification_with_key_b() {
    let signed = sign_head(&sample_head(), &key_a()).expect("sign");
    let mut mismatched = signed.clone();
    mismatched.public_key = hex::encode(key_b().verifying_key_bytes());
    let err = verify_head(&mismatched).expect_err("wrong public key");
    assert!(
        matches!(
            err,
            SignError::VerificationFailed | SignError::InvalidSignature
        ),
        "got {err:?}"
    );
}

#[test]
fn mutating_any_head_field_after_signing_fails_verification() {
    let signed = sign_head(&sample_head(), &key_a()).expect("sign");

    let mut seq_mut = signed.clone();
    seq_mut.head.seq += 1;
    assert!(verify_head(&seq_mut).is_err());

    let mut hash_mut = signed.clone();
    hash_mut.head.entry_hash = HashRef::parse(format!("sha256:{}", "cd".repeat(32))).unwrap();
    assert!(verify_head(&hash_mut).is_err());

    let mut time_mut = signed.clone();
    time_mut.head.recorded_at =
        Timestamp::from_offset_datetime(odt(2026, Month::September, 22, 0, 0, 0, 0)).unwrap();
    assert!(verify_head(&time_mut).is_err());

    let mut id_mut = signed.clone();
    id_mut.head.log_id = "other-log".into();
    assert!(verify_head(&id_mut).is_err());
}

#[test]
fn signature_is_deterministic_for_same_head_and_key() {
    let head = sample_head();
    let key = key_a();
    let a = sign_head(&head, &key).expect("sign a");
    let b = sign_head(&head, &key).expect("sign b");
    assert_eq!(
        a.signature, b.signature,
        "Ed25519 signatures must be deterministic; a change here means the dependency is no longer Ed25519"
    );
    assert_eq!(a.public_key, b.public_key);
}

#[test]
fn key_loading_rejects_malformed_key_without_panic() {
    let err = load_signing_key(KeySource::HexSeed("not-hex".into())).expect_err("malformed");
    assert!(matches!(err, SignError::InvalidKey(_)), "got {err:?}");

    let err = load_signing_key(KeySource::HexSeed("aa".repeat(16))).expect_err("too short");
    assert!(matches!(err, SignError::InvalidKey(_)));

    let mut file = NamedTempFile::new().expect("temp key file");
    std::io::Write::write_all(&mut file, b"zzzz").expect("write");
    let err = load_signing_key(KeySource::File(file.path().to_path_buf())).expect_err("bad file");
    assert!(matches!(err, SignError::InvalidKey(_) | SignError::Io(_)));
}

#[test]
fn signing_key_debug_does_not_contain_secret_hex() {
    let seed = [0xdeu8, 0xad, 0xbe, 0xef].repeat(8);
    let seed: [u8; 32] = seed.try_into().unwrap();
    let key = SigningKey::from_seed_bytes(&seed).expect("key");
    let secret_hex = hex::encode(seed);
    let debug = format!("{key:?}");
    assert!(
        !debug.to_lowercase().contains(&secret_hex),
        "Debug output must not leak secret seed hex; got {debug}"
    );
    assert!(
        debug.contains("REDACTED") || debug.contains("SigningKey"),
        "expected a redacted Debug representation; got {debug}"
    );
}
