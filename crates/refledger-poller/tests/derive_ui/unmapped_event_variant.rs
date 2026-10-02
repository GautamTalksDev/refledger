//! compile_fail: ClassifiedEvent match must be exhaustive — a missing arm
//! (as would happen when a new variant is added without updating derive)
//! must not compile. This file deliberately omits PendingMoveDeferred.

use refledger_poller::classify::ClassifiedEvent;

fn map_event(e: ClassifiedEvent) {
    match e {
        ClassifiedEvent::Move { .. } => {}
        ClassifiedEvent::Deletion { .. } => {}
        ClassifiedEvent::Recreation { .. } => {}
        ClassifiedEvent::RepoUnavailable { .. } => {}
        ClassifiedEvent::RepoRedirected { .. } => {}
        // ClassifiedEvent::PendingMoveDeferred omitted on purpose
    }
}

fn main() {
    let _ = map_event;
}
