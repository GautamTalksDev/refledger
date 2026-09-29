//! Canonical JSON serialisation (LOG-FORMAT.md §1).
//!
//! Core entry point: [`canonical_json`]. Do not enable `serde_json`'s
//! `preserve_order` feature — we sort object keys explicitly. A feature
//! flag that silently changes ordering behaviour is exactly the kind of
//! dependency that breaks a hash chain six months later.

use std::io::Write;

use serde_json::{Map, Number, Value};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use crate::Entry;

/// Errors produced while building canonical JSON or formatting timestamps.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CanonError {
    #[error("floating-point numbers are not permitted in canonical JSON")]
    FloatNotPermitted,
    #[error("null is not permitted in canonical JSON; omit the field upstream")]
    NullNotPermitted,
    #[error("timestamp has sub-millisecond precision; refuse to round")]
    SubMillisecondTimestamp,
    #[error("timestamp is not UTC")]
    TimestampNotUtc,
    #[error("failed to parse canonical JSON: {0}")]
    Parse(String),
    #[error("canonical value is not a valid Entry: {0}")]
    EntryShape(String),
}

/// Alias used by early tests; prefer [`CanonError`].
pub type CanonicalError = CanonError;

/// Canonicalise a `serde_json::Value` per LOG-FORMAT.md section 1.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CanonError> {
    let mut out = Vec::new();
    write_value(&mut out, value)?;
    debug_assert!(
        std::str::from_utf8(&out).is_ok(),
        "canonical JSON output must be valid UTF-8"
    );
    Ok(out)
}

/// Canonicalise a [`CanonicalValue`] (test/construction helper).
pub fn canonicalise(value: &CanonicalValue) -> Result<Vec<u8>, CanonError> {
    canonical_json(&value.to_serde_json()?)
}

/// Parse canonical JSON bytes back into a [`CanonicalValue`].
pub fn parse_canonical(bytes: &[u8]) -> Result<CanonicalValue, CanonError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| CanonError::Parse(e.to_string()))?;
    CanonicalValue::from_serde_json(&value)
}

/// Convert an external instant to UTC and floor it to whole milliseconds.
///
/// Call this at every boundary where a time enters from outside the process
/// (system clock, env vars, HTTP/GitHub schedule strings, wall-clock APIs).
/// Flooring never rounds up: sub-millisecond remainder is discarded.
/// Whole-second inputs keep nanosecond `0`, which formats as `.000Z`.
///
/// Downstream validators ([`format_timestamp`], observation/`entry` timestamps)
/// stay strict — they still reject unnormalised values. This function is the
/// only place that absorbs messy real-world precision.
pub fn normalize_to_utc_millis(t: OffsetDateTime) -> OffsetDateTime {
    let utc = t.to_offset(UtcOffset::UTC);
    let floored = (utc.nanosecond() / 1_000_000) * 1_000_000;
    utc.replace_nanosecond(floored)
        .expect("millisecond-aligned nanosecond is always a valid OffsetDateTime")
}

/// Format an instant as `YYYY-MM-DDTHH:MM:SS.sssZ`.
///
/// Zero milliseconds become `.000Z` (never truncated). Sub-millisecond
/// precision is an error — never silently rounded.
pub fn format_timestamp(t: OffsetDateTime) -> Result<String, CanonError> {
    if t.offset() != UtcOffset::UTC {
        return Err(CanonError::TimestampNotUtc);
    }
    let nanos = t.nanosecond();
    if nanos % 1_000_000 != 0 {
        return Err(CanonError::SubMillisecondTimestamp);
    }
    let ms = nanos / 1_000_000;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{ms:03}Z",
        year = t.year(),
        month = u8::from(t.month()),
        day = t.day(),
        hour = t.hour(),
        min = t.minute(),
        sec = t.second(),
        ms = ms,
    ))
}

fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<(), CanonError> {
    match value {
        Value::Null => Err(CanonError::NullNotPermitted),
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
        Value::Array(arr) => {
            out.push(b'[');
            for (i, elt) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, elt)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(map) => write_object(out, map),
    }
}

fn write_number(out: &mut Vec<u8>, n: &Number) -> Result<(), CanonError> {
    if n.is_f64() {
        return Err(CanonError::FloatNotPermitted);
    }
    if let Some(u) = n.as_u64() {
        // write! appends decimal digits directly into `out` — no intermediate String.
        write!(out, "{u}").expect("write to Vec<u8> cannot fail");
        return Ok(());
    }
    if let Some(i) = n.as_i64() {
        write!(out, "{i}").expect("write to Vec<u8> cannot fail");
        return Ok(());
    }
    // serde_json::Number that is neither f64, u64, nor i64 should not occur.
    Err(CanonError::FloatNotPermitted)
}

fn write_object(out: &mut Vec<u8>, map: &Map<String, Value>) -> Result<(), CanonError> {
    let mut keys: Vec<&String> = map.keys().collect();
    // Sort by UTF-8 bytes. For valid UTF-8 this is equivalent to code-point
    // order (UTF-8 preserves Unicode scalar-value order), so a byte compare
    // matches LOG-FORMAT.md §1.1 without decoding to char.
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
                out.push(hex_digit_lowercase(n >> 4));
                out.push(hex_digit_lowercase(n & 0x0f));
            }
            c => {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                out.extend_from_slice(encoded.as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn hex_digit_lowercase(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'a' + (n - 10),
        _ => unreachable!("hex digit 0..=15"),
    }
}

/// Construction / round-trip helper used by tests and callers that need
/// insertion-order object building before canonical sort.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Array(Vec<CanonicalValue>),
    /// Pairs in construction order; [`canonical_json`] sorts keys.
    Object(Vec<(String, CanonicalValue)>),
}

impl CanonicalValue {
    pub fn object<I, K>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, CanonicalValue)>,
        K: Into<String>,
    {
        Self::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn array<I>(elts: I) -> Self
    where
        I: IntoIterator<Item = CanonicalValue>,
    {
        Self::Array(elts.into_iter().collect())
    }

    pub fn string(s: impl Into<String>) -> Self {
        Self::String(s.into())
    }

    pub fn i64(n: i64) -> Self {
        Self::I64(n)
    }

    pub fn u64(n: u64) -> Self {
        Self::U64(n)
    }

    pub fn f64(n: f64) -> Self {
        Self::F64(n)
    }

    pub fn from_serde_json(value: &Value) -> Result<Self, CanonError> {
        Ok(match value {
            Value::Null => return Err(CanonError::NullNotPermitted),
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                if n.is_f64() {
                    Self::F64(n.as_f64().expect("is_f64"))
                } else if let Some(u) = n.as_u64() {
                    Self::U64(u)
                } else if let Some(i) = n.as_i64() {
                    Self::I64(i)
                } else {
                    return Err(CanonError::FloatNotPermitted);
                }
            }
            Value::String(s) => Self::String(s.clone()),
            Value::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for elt in arr {
                    out.push(Self::from_serde_json(elt)?);
                }
                Self::Array(out)
            }
            Value::Object(map) => {
                // Preserve map iteration order (BTreeMap: sorted). Equality
                // with from_entry therefore compares key-sorted objects.
                let mut pairs = Vec::with_capacity(map.len());
                for (k, v) in map {
                    pairs.push((k.clone(), Self::from_serde_json(v)?));
                }
                Self::Object(pairs)
            }
        })
    }

    pub fn from_entry(entry: &Entry) -> Self {
        let value = serde_json::to_value(entry).expect("Entry serialises to JSON");
        Self::from_serde_json(&value).expect("Entry JSON contains no null/float")
    }

    fn to_serde_json(&self) -> Result<Value, CanonError> {
        Ok(match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::I64(n) => Value::Number((*n).into()),
            Self::U64(n) => Value::Number((*n).into()),
            Self::F64(n) => {
                let number = Number::from_f64(*n).ok_or(CanonError::FloatNotPermitted)?;
                Value::Number(number)
            }
            Self::String(s) => Value::String(s.clone()),
            Self::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for elt in arr {
                    out.push(elt.to_serde_json()?);
                }
                Value::Array(out)
            }
            Self::Object(pairs) => {
                let mut map = Map::new();
                for (k, v) in pairs {
                    map.insert(k.clone(), v.to_serde_json()?);
                }
                Value::Object(map)
            }
        })
    }
}
