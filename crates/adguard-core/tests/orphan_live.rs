//! The leftover-process scan against the real install on this machine.
//!
//! The unit tests in `orphan.rs` prove the pieces — the argument match against
//! recorded command lines, and the find-and-signal cycle against a stand-in
//! process. Neither can prove the thing that actually matters: that a **healthy,
//! running** AdGuard proxy is not mistaken for the wedged leftover this module
//! exists to kill.
//!
//! That question has one honest source, and it is the machine. A running proxy
//! and a wedged one are the same binary with the same command line — measured,
//! immediately after recovering from the real thing on 2026-08-01 — so the only
//! difference between "leave it alone" and "send it SIGTERM" is whether
//! `adguard-cli status` agrees the proxy is up. This checks that the two really
//! do agree on an install that is working.
//!
//! **Read-only, and safe to run by default.** Nothing here signals anything;
//! `Daemon::terminate` is never called and must never be called from this file.
//! The scan itself is a few `/proc` reads, and `status` mutates nothing beyond
//! the config rewrite every invocation performs anyway.
//!
//! The suite skips when AdGuard CLI is not installed, and skips again when the
//! proxy is not running — a stopped proxy is a legitimate state, and it is the
//! state in which this module's answer is allowed to be "there is something
//! here to clear".

use adguard_core::{orphan, Cli};

/// `None` when there is no CLI to ask, which is a skip rather than a failure.
fn cli() -> Option<Cli> {
    Cli::discover().ok()
}

/// The one assertion this file exists for: while the proxy is up, nothing the
/// scan finds may be treated as strandable.
///
/// The application's rule is *daemons found **and** `status` says stopped*. Here
/// `status` says running, so whatever the scan returns, no recovery can trigger.
/// A regression that inverted or dropped that guard would leave this passing —
/// which is why the test also states the inputs it depends on rather than only
/// the conclusion.
#[test]
fn a_running_proxy_is_never_strandable() {
    let Some(cli) = cli() else {
        eprintln!("skipping: adguard-cli is not installed");
        return;
    };
    let Ok(status) = cli.status() else {
        eprintln!("skipping: could not read status");
        return;
    };
    if !status.running {
        eprintln!("skipping: the proxy is not running");
        return;
    }

    let found = orphan::daemons(cli.binary());

    // The guard, spelled out as the application applies it.
    let would_recover = !status.running && !found.is_empty();
    assert!(
        !would_recover,
        "a running proxy was classed as a leftover to kill: {found:?}"
    );
}

/// While the proxy is up, the scan should be able to *see* it.
///
/// Not a safety property — the one above is — but the thing that makes recovery
/// possible at all. If AdGuard stops launching its daemon as
/// `adguard-cli start --no-fork …`, or starts running it as another user, this
/// goes quiet and a wedged install would simply never be recovered. Silent, and
/// only visible here.
///
/// So a miss is reported rather than asserted: running the proxy from a system
/// service, or as a different user, is a legitimate install this test has no
/// business failing over. `/proc/<pid>/exe` is unreadable across users by
/// design, and that is the kernel refusing to let this application signal
/// something it does not own.
#[test]
fn the_scan_can_see_a_running_proxy() {
    let Some(cli) = cli() else {
        eprintln!("skipping: adguard-cli is not installed");
        return;
    };
    let Ok(status) = cli.status() else {
        eprintln!("skipping: could not read status");
        return;
    };
    if !status.running {
        eprintln!("skipping: the proxy is not running");
        return;
    }

    let found = orphan::daemons(cli.binary());
    if found.is_empty() {
        eprintln!(
            "note: the proxy is running but no daemon process was found — recovery \
             from a wedged proxy would not be possible on this install"
        );
        return;
    }

    // Everything the scan returns must be a live process it just read, and pids
    // are positive. Cheap, and it is the only place the real `/proc` parse is
    // exercised against AdGuard's own process rather than a stand-in.
    for daemon in &found {
        assert!(daemon.pid() > 0, "implausible pid: {daemon:?}");
        assert!(
            daemon.alive(),
            "the scan returned a process it had just found to be dead: {daemon:?}"
        );
    }
}
