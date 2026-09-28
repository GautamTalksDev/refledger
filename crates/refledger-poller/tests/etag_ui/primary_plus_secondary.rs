use refledger_poller::github::ratelimit::{PrimaryPoints, SecondaryPoints};

fn main() {
    let primary = PrimaryPoints::new(1);
    let secondary = SecondaryPoints::new(1);
    let _sum = primary + secondary;
}
