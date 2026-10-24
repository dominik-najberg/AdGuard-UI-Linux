//! Matching addresses against the filter lists switched on on this machine.
//!
//! Read-only. The addresses are fixed ones, not anything from the user's
//! browsing, so the rules that decide them can be printed.

use std::time::Instant;

use adguard_core::rules::{Address, Lists, Verdict};

#[test]
fn the_real_lists_decide_well_known_addresses() {
    let started = Instant::now();
    let lists = match Lists::read() {
        Ok(lists) => lists,
        Err(err) => {
            eprintln!("skipping: the lists could not be read: {err}");
            return;
        }
    };
    eprintln!("read in {:?}", started.elapsed());

    for typed in [
        "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
        "ad.doubleclick.net",
        "example.com",
        "www.wikipedia.org",
    ] {
        let started = Instant::now();
        let check = lists.check(&Address::parse(typed).unwrap());
        let verdict = match &check.verdict {
            Verdict::Blocked(block) => format!("blocked by {} (list {})", block.rule, block.filter),
            Verdict::Allowed { block, exception } => {
                format!("allowed by {} over {}", exception.rule, block.rule)
            }
            Verdict::Clear => "clear".to_owned(),
        };
        eprintln!(
            "{typed}: {verdict}; {} other match(es), {} more; {} rules, {} regex; {:?}",
            check.matches.len(),
            check.more,
            check.rules,
            check.regex,
            started.elapsed()
        );
        for found in &check.matches {
            eprintln!("    {} (list {}) {:?}", found.rule, found.filter, found.when());
        }
        assert!(check.rules > 0, "no rules were read");
    }
}
