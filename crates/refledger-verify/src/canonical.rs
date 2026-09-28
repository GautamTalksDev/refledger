//! Canonical JSON per docs/LOG-FORMAT.md §1.
//!
//! Written from the specification only — not adapted from refledger-log.

use std::io::Write;

use serde_json::{Map, Number, Value};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CanonError {
    #[error("floating-point numbers are not permitted")]
    Float,
    #[error("null is not permitted; omit the field instead")]
    Null,
}

/// Serialise `value` to canonical JSON bytes (LOG-FORMAT.md §1).
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CanonError> {
    let mut out = Vec::new();
    write_value(&mut out, value)?;
    debug_assert!(std::str::from_utf8(&out).is_ok());
    Ok(out)
}

fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<(), CanonError> {
    match value {
        Value::Null => Err(CanonError::Null),
        Value::Bool(true) => {
            out.extend_from_slice(b"true");
            Ok(())
        }
        Value::Bool(false) => {
            out.extend_from_slice(b"false");
            Ok(())
        }
        Value::Number(n) => write_number(out, n),
        Value::String(s) => {
            write_string(out, s);
            Ok(())
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, item)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(map) => write_object(out, map),
    }
}

fn write_number(out: &mut Vec<u8>, n: &Number) -> Result<(), CanonError> {
    if n.is_f64() {
        return Err(CanonError::Float);
    }
    if let Some(u) = n.as_u64() {
        write!(out, "{u}").expect("Vec write");
        return Ok(());
    }
    if let Some(i) = n.as_i64() {
        write!(out, "{i}").expect("Vec write");
        return Ok(());
    }
    Err(CanonError::Float)
}

fn write_object(out: &mut Vec<u8>, map: &Map<String, Value>) -> Result<(), CanonError> {
    let mut keys: Vec<&String> = map.keys().collect();
    // UTF-8 byte order ≡ Unicode code-point order for valid UTF-8.
    keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    out.push(b'{');
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_string(out, key);
        out.push(b':');
        write_value(out, &map[*key])?;
    }
    out.push(b'}');
    Ok(())
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for ch in s.chars() {
        match ch {
            '"' => out.extend_from_slice(br#"\""#),
            '\\' => out.extend_from_slice(br#"\\"#),
            '\u{0008}' => out.extend_from_slice(br#"\b"#),
            '\u{000C}' => out.extend_from_slice(br#"\f"#),
            '\n' => out.extend_from_slice(br#"\n"#),
            '\r' => out.extend_from_slice(br#"\r"#),
            '\t' => out.extend_from_slice(br#"\t"#),
            c if (c as u32) < 0x20 => {
                let n = c as u8;
                out.extend_from_slice(b"\\u00");
                out.push(LOWER_HEX[(n >> 4) as usize]);
                out.push(LOWER_HEX[(n & 0x0f) as usize]);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
