//! Generate head conformance vectors under tests/vectors/heads/.
//!
//! Fixed test key seed is 32 bytes of 0x01 — never a production key.
//! Each vector records canonical head bytes, the pure Ed25519 signature,
//! and the SHA-512 Rekor prehash over those same bytes.

use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha512};
use refledger_log::canonical_json;
use refledger_log::entry::{HashRef, Timestamp};
use refledger_log::sign::{sign_head, Head, SigningKey};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

fn odt(
    year: i32,
    month: Month,
    day: u8,
    hour: u8,
    min: u8,
    sec: u8,
    millisecond: u16,
) -> OffsetDateTime {
    let time = Time::from_hms_milli(hour, min, sec, millisecond).unwrap();
    let date = Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn test_key() -> SigningKey {
    SigningKey::from_seed_bytes(&[0x01u8; 32]).expect("fixed test key")
}

fn write_vector(name: &str, description: &str, head: Head) {
    let key = test_key();
    let value = serde_json::to_value(&head).expect("head value");
    let canonical = canonical_json(&value).expect("canonical");
    let canonical_str = String::from_utf8(canonical.clone()).expect("utf8");
    let signed = sign_head(&head, &key).expect("sign");
    let prehash = hex::encode(Sha512::digest(&canonical));
    let seed_hex = hex::encode([0x01u8; 32]);

    let doc = serde_json::json!({
        "description": description,
        "test_key_seed_hex": seed_hex,
        "head": head,
        "expected_canonical": canonical_str,
        "expected_signature_hex": signed.signature,
        "expected_public_key_hex": signed.public_key,
        "expected_rekor_prehash_sha512_hex": prehash,
    });

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/vectors/heads");
    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{name}.json"));
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    println!("wrote {}", path.display());
}

fn main() {
    write_vector(
        "head_genesis",
        "Genesis-day head: seq 0, first signed tip.",
        Head {
            seq: 0,
            entry_hash: HashRef::parse(format!("sha256:{}", "11".repeat(32))).unwrap(),
            recorded_at: Timestamp::from_offset_datetime(odt(
                2026,
                Month::January,
                1,
                0,
                0,
                0,
                0,
            ))
            .unwrap(),
            log_id: "refledger".into(),
        },
    );

    write_vector(
        "head_mid_chain",
        "Mid-chain head: seq 7 after several sealed days.",
        Head {
            seq: 7,
            entry_hash: HashRef::parse(format!("sha256:{}", "ab".repeat(32))).unwrap(),
            recorded_at: Timestamp::from_offset_datetime(odt(
                2026,
                Month::September,
                21,
                12,
                0,
                0,
                0,
            ))
            .unwrap(),
            log_id: "refledger".into(),
        },
    );

    write_vector(
        "head_max_seq",
        "Head with maximum u64 seq — exercises large integer canonicalisation.",
        Head {
            seq: u64::MAX,
            entry_hash: HashRef::parse(format!("sha256:{}", "ff".repeat(32))).unwrap(),
            recorded_at: Timestamp::from_offset_datetime(odt(
                2099,
                Month::December,
                31,
                23,
                59,
                59,
                999,
            ))
            .unwrap(),
            log_id: "refledger".into(),
        },
    );

    write_vector(
        "head_unicode_log_id",
        "Head whose log_id contains non-ASCII Unicode (CJK + combining).",
        Head {
            seq: 42,
            entry_hash: HashRef::parse(format!("sha256:{}", "cd".repeat(32))).unwrap(),
            recorded_at: Timestamp::from_offset_datetime(odt(
                2026,
                Month::June,
                15,
                8,
                30,
                0,
                0,
            ))
            .unwrap(),
            log_id: "refledger-测试-ログ".into(),
        },
    );
}
