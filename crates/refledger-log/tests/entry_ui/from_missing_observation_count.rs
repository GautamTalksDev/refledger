//! compile_fail: Binding builder cannot build without observation_count.
//! Type-state makes the required field a compile error, not an Option.

use refledger_log::entry::Binding;
use time::{Month, PrimitiveDateTime, Time};

fn main() {
    let time = Time::from_hms_milli(0, 0, 0, 0).unwrap();
    let date = time::Date::from_calendar_date(2026, Month::January, 1).unwrap();
    let first = PrimitiveDateTime::new(date, time).assume_utc();
    let last = first;

    // Missing `.observation_count(...)` — `build` must not exist on this type state.
    let _from = Binding::builder()
        .target_sha("a".repeat(40))
        .commit_sha("b".repeat(40))
        .tree_sha("c".repeat(40))
        .first_observed(first)
        .last_observed(last)
        .build();
}
