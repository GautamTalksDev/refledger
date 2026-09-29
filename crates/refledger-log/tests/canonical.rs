//! Canonical JSON serialisation tests (LOG-FORMAT.md §1).
//!
//! Written before the implementation. These tests define the
//! `refledger_log::canonical` API surface and must fail to compile until
//! that module exists.

use proptest::prelude::*;
use refledger_log::canonical::{
    canonicalise, format_timestamp, normalize_to_utc_millis, parse_canonical, CanonError,
    CanonicalValue,
};
use refledger_log::Entry;
use serde_json::json;
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

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

#[test]
fn key_ordering_reverse_insertion_sorts_ascending() {
    // Built with keys in reverse alphabetical insertion order.
    let value = CanonicalValue::object([
        ("z", CanonicalValue::string("last")),
        ("m", CanonicalValue::string("mid")),
        ("a", CanonicalValue::string("first")),
    ]);

    let bytes = canonicalise(&value).expect("canonicalise");
    assert_eq!(bytes, br#"{"a":"first","m":"mid","z":"last"}"#);
}

#[test]
fn unicode_key_ordering_by_code_point_not_locale() {
    let value = CanonicalValue::object([
        ("z", CanonicalValue::i64(1)),
        ("é", CanonicalValue::i64(2)),
        ("A", CanonicalValue::i64(3)),
        ("\u{1F600}", CanonicalValue::i64(4)),
        ("a", CanonicalValue::i64(5)),
        ("Z", CanonicalValue::i64(6)),
    ]);

    let bytes = canonicalise(&value).expect("canonicalise");
    // Code points ascending: A (U+0041), Z (U+005A), a (U+0061), z (U+007A),
    // é (U+00E9), 😀 (U+1F600).
    assert_eq!(
        bytes,
        "{\"A\":3,\"Z\":6,\"a\":5,\"z\":1,\"é\":2,\"\u{1F600}\":4}".as_bytes()
    );
}

#[test]
fn nested_objects_and_arrays_are_canonicalised_recursively_array_order_preserved() {
    let value = CanonicalValue::object([
        (
            "outer",
            CanonicalValue::object([("b", CanonicalValue::i64(2)), ("a", CanonicalValue::i64(1))]),
        ),
        (
            "items",
            CanonicalValue::array([
                CanonicalValue::object([
                    ("y", CanonicalValue::i64(2)),
                    ("x", CanonicalValue::i64(1)),
                ]),
                CanonicalValue::object([
                    ("n", CanonicalValue::i64(2)),
                    ("m", CanonicalValue::i64(1)),
                ]),
            ]),
        ),
    ]);

    let bytes = canonicalise(&value).expect("canonicalise");
    // Object keys sorted; array element order preserved (first object before second).
    assert_eq!(
        bytes,
        br#"{"items":[{"x":1,"y":2},{"m":1,"n":2}],"outer":{"a":1,"b":2}}"#
    );
}

#[test]
fn absent_optional_field_is_omitted_never_null() {
    use refledger_log::entry::{Entry, Event, HashRef};
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

    let with_some = Entry::builder()
        .seq(2)
        .prev_hash(HashRef::GENESIS)
        .recorded_at(odt(2026, Month::May, 1, 0, 0, 0, 0))
        .event(Event::Correction)
        .corrects_seq(7)
        .reason("fixture")
        .detection_latency_note("within poll interval")
        .build()
        .expect("correction with note");
    let with_none = Entry::builder()
        .seq(2)
        .prev_hash(HashRef::GENESIS)
        .recorded_at(odt(2026, Month::May, 1, 0, 0, 0, 0))
        .event(Event::Correction)
        .corrects_seq(7)
        .reason("fixture")
        .build()
        .expect("correction without note");

    let some_bytes = canonicalise(&CanonicalValue::from_entry(&with_some)).expect("some");
    let none_bytes = canonicalise(&CanonicalValue::from_entry(&with_none)).expect("none");

    assert_ne!(some_bytes.len(), none_bytes.len());
    assert!(
        !none_bytes.windows(4).any(|w| w == b"null"),
        "None must omit the field, not serialise null; got {}",
        String::from_utf8_lossy(&none_bytes)
    );
    let some_str = String::from_utf8(some_bytes).expect("utf8");
    assert!(
        some_str.contains("\"detection_latency_note\":"),
        "Some(x) must include the field; got {some_str}"
    );
}

#[test]
fn empty_object_array_string_round_trip() {
    for value in [
        CanonicalValue::object([] as [(&str, CanonicalValue); 0]),
        CanonicalValue::array([] as [CanonicalValue; 0]),
        CanonicalValue::string(""),
    ] {
        let bytes = canonicalise(&value).expect("canonicalise");
        let parsed = parse_canonical(&bytes).expect("parse");
        assert_eq!(parsed, value);
        assert_eq!(canonicalise(&parsed).expect("re-canonicalise"), bytes);
    }
}

#[test]
fn integer_edge_cases() {
    assert_eq!(canonicalise(&CanonicalValue::i64(0)).unwrap(), b"0");
    // -0 as an integer is 0 in two's complement; must serialise as "0", never "-0".
    assert_eq!(canonicalise(&CanonicalValue::i64(-0)).unwrap(), b"0");
    assert_eq!(
        canonicalise(&CanonicalValue::i64(i64::MIN)).unwrap(),
        b"-9223372036854775808"
    );
    assert_eq!(
        canonicalise(&CanonicalValue::i64(i64::MAX)).unwrap(),
        b"9223372036854775807"
    );
    assert_eq!(
        canonicalise(&CanonicalValue::u64(u64::MAX)).unwrap(),
        b"18446744073709551615"
    );
}

#[test]
fn float_returns_error_not_silent_serialisation() {
    let err = canonicalise(&CanonicalValue::f64(1.5)).expect_err("float must error");
    assert!(
        matches!(err, CanonError::FloatNotPermitted),
        "expected FloatNotPermitted, got {err:?}"
    );

    // Also reject floats that arrive via serde_json::Value.
    let via_json = CanonicalValue::from_serde_json(&json!(2.5)).expect("keep float node");
    let err = canonicalise(&via_json).expect_err("json float must error");
    assert!(matches!(err, CanonError::FloatNotPermitted));
}

#[test]
fn timestamp_zero_milliseconds_emits_dot_000z() {
    let t = odt(2026, Month::September, 21, 12, 0, 0, 0);
    let s = format_timestamp(t).expect("exactly ms precision");
    assert_eq!(s, "2026-09-21T12:00:00.000Z");
    assert!(
        s.ends_with(".000Z"),
        "zero milliseconds must not truncate fractional seconds: {s}"
    );
}

#[test]
fn timestamp_sub_millisecond_returns_error_not_rounding() {
    let t = odt(2026, Month::September, 21, 12, 0, 0, 0) + Duration::nanoseconds(1);
    let err = format_timestamp(t).expect_err("sub-millisecond must error");
    assert!(
        matches!(err, CanonError::SubMillisecondTimestamp),
        "expected SubMillisecondTimestamp, got {err:?}"
    );
}

#[test]
fn normalize_floors_nanosecond_clock_reading() {
    // Real clocks expose sub-ms; floor, never round.
    let raw = odt(2026, Month::September, 29, 14, 40, 0, 123) + Duration::nanoseconds(456_789);
    assert_eq!(raw.nanosecond(), 123_456_789);
    let n = normalize_to_utc_millis(raw);
    assert_eq!(n.offset(), UtcOffset::UTC);
    assert_eq!(n.nanosecond(), 123_000_000);
    assert_eq!(format_timestamp(n).unwrap(), "2026-09-29T14:40:00.123Z");
    // Rounding up would have produced .124 — prove we floored.
    assert_ne!(n.nanosecond(), 124_000_000);
}

#[test]
fn normalize_whole_second_string_gains_dot_000() {
    let raw = OffsetDateTime::parse(
        "2026-09-29T14:40:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("second-precision RFC 3339");
    assert_eq!(raw.nanosecond(), 0);
    let n = normalize_to_utc_millis(raw);
    assert_eq!(format_timestamp(n).unwrap(), "2026-09-29T14:40:00.000Z");
}

#[test]
fn normalize_non_utc_offset_converts_then_floors() {
    let offset = UtcOffset::from_hms(-4, 0, 0).unwrap();
    let local = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, Month::September, 29).unwrap(),
        Time::from_hms_nano(10, 40, 0, 999_999_999).unwrap(),
    )
    .assume_offset(offset);
    let n = normalize_to_utc_millis(local);
    assert_eq!(n.offset(), UtcOffset::UTC);
    // 10:40:00.999999999-04:00 → 14:40:00.999Z (floor, not round to 15:00)
    assert_eq!(format_timestamp(n).unwrap(), "2026-09-29T14:40:00.999Z");
}

#[test]
fn normalize_exact_millisecond_boundary_unchanged() {
    let exact = odt(2026, Month::September, 29, 14, 40, 0, 500);
    assert_eq!(exact.nanosecond(), 500_000_000);
    let n = normalize_to_utc_millis(exact);
    assert_eq!(n, exact);
    assert_eq!(format_timestamp(n).unwrap(), "2026-09-29T14:40:00.500Z");
}

#[test]
fn normalize_is_idempotent_on_messy_and_tidy_inputs() {
    let messy = odt(2026, Month::September, 29, 14, 40, 0, 1) + Duration::nanoseconds(1);
    let once = normalize_to_utc_millis(messy);
    let twice = normalize_to_utc_millis(once);
    assert_eq!(once, twice);
    let tidy = odt(2026, Month::September, 29, 14, 40, 0, 0);
    assert_eq!(normalize_to_utc_millis(tidy), tidy);
}

#[test]
fn normalize_then_format_accepts_live_system_clock() {
    let raw = OffsetDateTime::now_utc();
    let n = normalize_to_utc_millis(raw);
    assert_eq!(n.nanosecond() % 1_000_000, 0);
    format_timestamp(n).expect("normalised system clock must format");
}

#[test]
fn string_escaping_exact_bytes() {
    let s = "\"\\\n\t\u{0000}\u{001F}\u{1F600}";
    let bytes = canonicalise(&CanonicalValue::string(s)).expect("canonicalise");

    // Shortest escapes; \u hex lowercase; U+007F is not in this string.
    // Expected: "\"\\\n\t\u0000\u001f😀"
    let expected = "\"\\\"\\\\\\n\\t\\u0000\\u001f\u{1F600}\"";
    assert_eq!(
        bytes,
        expected.as_bytes(),
        "got {}",
        String::from_utf8_lossy(&bytes)
    );

    // U+007F (DEL) must NOT be escaped.
    let del = canonicalise(&CanonicalValue::string("\u{007F}")).expect("del");
    assert_eq!(del, b"\"\x7f\"");
    assert!(
        !String::from_utf8_lossy(&del).contains("\\u"),
        "U+007F must not be \\u-escaped"
    );
}

#[test]
fn byte_for_byte_determinism_100_times() {
    let value = CanonicalValue::object([
        (
            "tags",
            CanonicalValue::array([
                CanonicalValue::string("b"),
                CanonicalValue::string("a"),
                CanonicalValue::object([
                    ("z", CanonicalValue::i64(-1)),
                    ("a", CanonicalValue::u64(u64::MAX)),
                ]),
            ]),
        ),
        ("note", CanonicalValue::string("\"\\\n\t")),
        ("seq", CanonicalValue::i64(42)),
    ]);

    let first = canonicalise(&value).expect("first");
    for i in 0..100 {
        let again = canonicalise(&value).expect("repeat");
        assert_eq!(again, first, "divergence at iteration {i}");
    }
}

fn arb_entry() -> impl Strategy<Value = Entry> {
    use refledger_log::entry::{Event, HashRef};
    use time::{Month, PrimitiveDateTime, Time};

    let recorded = {
        let time = Time::from_hms_milli(0, 0, 0, 0).unwrap();
        let date = time::Date::from_calendar_date(2026, Month::September, 21).unwrap();
        PrimitiveDateTime::new(date, time).assume_utc()
    };

    (
        any::<u32>().prop_map(|n| u64::from(n) + 1),
        any::<u64>(),
        proptest::string::string_regex("[a-z0-9 _]{0,32}").unwrap(),
        any::<bool>(),
    )
        .prop_map(move |(seq, corrects_seq, reason, with_note)| {
            let mut b = Entry::builder()
                .seq(seq)
                .prev_hash(HashRef::GENESIS)
                .recorded_at(recorded)
                .event(Event::Correction)
                .corrects_seq(corrects_seq)
                .reason(reason);
            if with_note {
                b = b.detection_latency_note("note");
            }
            b.build().expect("arb correction")
        })
}

proptest! {
    #[test]
    fn prop_entry_canonicalise_stable_and_round_trips(entry in arb_entry()) {
        let v = CanonicalValue::from_entry(&entry);
        let a = canonicalise(&v).expect("canonicalise a");
        let b = canonicalise(&v).expect("canonicalise b");
        prop_assert_eq!(&a, &b);

        let parsed = parse_canonical(&a).expect("parse");
        prop_assert_eq!(&parsed, &v);

        let entry_back = Entry::from_canonical(&parsed).expect("entry from canonical");
        prop_assert_eq!(entry_back, entry);
    }
}
