//! The request search against the real access log on this machine.
//!
//! Read-only, and it prints counts and timings only: a hit is a line of the
//! user's browsing, and a test's output ends up in terminals and CI logs.

use std::time::Instant;

use adguard_core::access;
use adguard_core::activity::Action;
use adguard_core::requests::{quic, search, seen, Query};

#[test]
fn the_real_log_can_be_searched() {
    let Some(live) = access::path() else {
        eprintln!("skipping: no data directory to look in");
        return;
    };
    if !live.is_file() {
        eprintln!("skipping: {} is not there", live.display());
        return;
    }

    let started = Instant::now();
    let newest = search(&live, &Query::default());
    eprintln!("newest page: {} hits, {} lines read, in {:?}", newest.hits.len(), newest.scanned, started.elapsed());
    assert!(!newest.hits.is_empty(), "nothing in {} was read", live.display());
    assert!(newest.hits.windows(2).all(|pair| pair[0].at >= pair[1].at), "not newest first");

    // A word that is on no line reads every line: the worst case a keystroke
    // can cost.
    let started = Instant::now();
    let nothing = search(&live, &Query { text: "zz-no-such-host-zz".to_owned(), ..Query::default() });
    eprintln!("miss: {} lines read, in {:?}", nothing.scanned, started.elapsed());
    assert!(nothing.hits.is_empty());
    assert!(!nothing.more);

    let started = Instant::now();
    let blocked = search(&live, &Query { action: Some(Action::Blocked), ..Query::default() });
    eprintln!("blocked page: {} hits, in {:?}", blocked.hits.len(), started.elapsed());
    assert!(blocked.hits.iter().all(|hit| hit.action == Action::Blocked));

    // The two whole passes the Diagnostics page and the website check make.
    let started = Instant::now();
    let share = quic(&live);
    eprintln!(
        "quic: {} of {} lines, {} with no action, in {:?}",
        share.quic, share.lines, share.uninspected, started.elapsed()
    );
    assert!(share.quic <= share.lines);

    let started = Instant::now();
    let site = seen(&live, "example.com");
    eprintln!("seen: {} requests, in {:?}", site.total, started.elapsed());
}
