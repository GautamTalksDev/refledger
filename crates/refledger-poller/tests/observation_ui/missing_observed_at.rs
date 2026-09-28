//! compile_fail: ObservationBuilder cannot build without observed_at.

use refledger_poller::observation::{Method, Observation, Outcome};

fn main() {
    let _ = Observation::builder()
        .repo("acme/widgets")
        .unwrap()
        .method(Method::Rest)
        .outcome(Outcome::Skipped {
            reason: refledger_poller::observation::SkipReason::BudgetExhausted,
        })
        .build();
}
