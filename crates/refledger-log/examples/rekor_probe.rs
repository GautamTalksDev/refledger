//! One-shot live probe: submit a hashedrekord to Rekor and fetch it back.
//!
//! Run: cargo run -p refledger-log --example rekor_probe
//!
//! Confirms Ed25519ph + SHA-512 artifact hash is accepted by production Rekor,
//! and that the returned logIndex resolves via GET /api/v1/log/entries?logIndex=.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use refledger_log::entry::{HashRef, Timestamp};
use refledger_log::{
    canonical_json, key_id, public_key_pkix_pem, sign_ed25519ph, sign_head, Head, SigningKey,
};
use sha2::{Digest, Sha512};
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
    let time = Time::from_hms_milli(hour, min, sec, millisecond).unwrap();
    let date = time::Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn main() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seed = {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&(nonce as u64).to_le_bytes());
        s[8..].copy_from_slice(b"refledger-rekor-probe!!!!");
        s
    };
    let key = SigningKey::from_seed_bytes(&seed).expect("key");
    let head = Head {
        seq: 0,
        entry_hash: HashRef::parse(format!(
            "sha256:{}",
            hex::encode(Sha512::digest(format!("probe-{nonce}").as_bytes()))
                .chars()
                .take(64)
                .collect::<String>()
        ))
        .unwrap(),
        recorded_at: Timestamp::from_offset_datetime(odt(2026, Month::September, 28, 18, 0, 0, 0))
            .unwrap(),
        log_id: format!("refledger-rekor-probe-{nonce}"),
    };
    let signed = sign_head(&head, &key).expect("sign_head (pure Ed25519)");
    let canonical = canonical_json(&serde_json::to_value(&head).unwrap()).unwrap();
    let hash = Sha512::digest(&canonical);
    let hex_hash = hex::encode(hash);
    let ph_sig = sign_ed25519ph(&key, &canonical).expect("ed25519ph");
    let pem = public_key_pkix_pem(&key.verifying_key_bytes());
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "apiVersion": "0.0.1",
        "kind": "hashedrekord",
        "spec": {
            "data": {
                "hash": {
                    "algorithm": "sha512",
                    "value": hex_hash
                }
            },
            "signature": {
                "content": b64.encode(ph_sig),
                "publicKey": {
                    "content": b64.encode(pem.as_bytes())
                }
            }
        }
    });

    println!("key_id={}", key_id(&key.verifying_key_bytes()));
    println!("pure_ed25519_sig_len={}", signed.signature.len());
    println!("artifact_hash=sha512:{hex_hash}");
    println!("submitting to https://rekor.sigstore.dev/api/v1/log/entries …");

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let resp = agent
        .post("https://rekor.sigstore.dev/api/v1/log/entries")
        .set("Content-Type", "application/json")
        .set("User-Agent", "refledger/0.1.0-rekor-probe")
        .send_json(&body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            eprintln!("REJECTED status={code}\n{text}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("transport error: {e}");
            std::process::exit(2);
        }
    };
    let text = resp.into_string().expect("body");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    let (uuid, entry) = value
        .as_object()
        .expect("object")
        .iter()
        .next()
        .expect("entry");
    let log_index = entry["logIndex"].as_u64().expect("logIndex");
    println!("ACCEPTED uuid={uuid} logIndex={log_index}");

    let fetch = agent
        .get(&format!(
            "https://rekor.sigstore.dev/api/v1/log/entries?logIndex={log_index}"
        ))
        .set("User-Agent", "refledger/0.1.0-rekor-probe")
        .call();
    match fetch {
        Ok(r) => {
            let back = r.into_string().unwrap();
            let v: serde_json::Value = serde_json::from_str(&back).unwrap();
            let found = v
                .as_object()
                .map(|o| {
                    o.contains_key(uuid)
                        || o.values()
                            .any(|e| e["logIndex"].as_u64() == Some(log_index))
                })
                .unwrap_or(false);
            if found {
                println!("FETCH_OK logIndex={log_index} resolves");
            } else {
                eprintln!("FETCH_MISS body={back}");
                std::process::exit(3);
            }
        }
        Err(e) => {
            eprintln!("fetch failed: {e}");
            std::process::exit(4);
        }
    }
}
