//! The activity store against the real access log on this machine.
//!
//! The unit tests in `activity.rs` prove the parser and the cursor against
//! lines written for them. What they cannot prove is that the format is still
//! the one the parser was measured against: every guard there fails to
//! **unread**, so a format that has drifted does not fail a unit test, it
//! quietly empties the dashboard. This file is what notices.
//!
//! **Read-only as far as AdGuard is concerned.** It reads the log and writes a
//! throwaway database under the temp directory, never the real one in
//! `~/.local/state`. It prints counts and nothing else: the log is a browsing
//! record, and a test's output ends up in terminals and CI logs.

use std::time::Instant;

use adguard_core::activity::{Span, Store};
use adguard_core::access;

#[test]
fn every_line_of_the_real_log_is_understood() {
    let Some(live) = access::path() else {
        eprintln!("skipping: no data directory to look in");
        return;
    };
    if !live.is_file() {
        eprintln!("skipping: {} is not there", live.display());
        return;
    }

    let dir = std::env::temp_dir().join(format!("adguard-ui-activity-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut store = Store::open(&dir.join("activity.sqlite")).expect("open a scratch store");

    let started = Instant::now();
    let first = store.ingest(&live).expect("ingest the real log");
    let took = started.elapsed();
    let size = std::fs::metadata(dir.join("activity.sqlite")).map_or(0, |meta| meta.len());
    eprintln!(
        "first ingest: {} requests, {} unread, in {took:?}; database {size} B",
        first.lines, first.unread,
    );

    let started = Instant::now();
    let again = store.ingest(&live).expect("ingest again");
    eprintln!("second ingest: {} requests in {:?}", again.lines, started.elapsed());

    let month = store.summary(Span::Month).expect("summarise");
    eprintln!(
        "month: {} requests, {} blocked, {} top hosts, {} top rules",
        month.totals.total(),
        month.totals.blocked,
        month.hosts.len(),
        month.rules.len(),
    );
    std::fs::remove_dir_all(&dir).ok();

    assert!(first.lines > 0, "nothing in {} was read", live.display());
    assert_eq!(
        first.unread, 0,
        "{} lines of the real log were not understood — the format has moved",
        first.unread,
    );
    // Only what AdGuard wrote in the moments between the two reads.
    assert!(again.lines < first.lines / 10, "the second ingest re-read the log");
}
