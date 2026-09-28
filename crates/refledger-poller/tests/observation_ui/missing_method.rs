//! compile_fail: ObservationBuilder cannot build without method.
//!
//! poller_version is stamped from CARGO_PKG_VERSION automatically — there is
//! no setter. The compile-time gate that used to require an explicit version
//! is now the required `method` field instead.

use refledger_poller::observation::{Observation, Outcome};
use time::{Month, PrimitiveDateTime, Time};

fn main() {
    let time = Time::from_hms_milli(0, 0, 0, 0).unwrap();
    let date = time::Date::from_calendar_date(2026, Month::January, 1).unwrap();
    let t = PrimitiveDateTime::new(date, time).assume_utc();

    let _ = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .observed_at(t)
        .unwrap()
        .outcome(Outcome::Skipped {
            reason: refledger_poller::observation::SkipReason::BudgetExhausted,
        })
        .build();
}
