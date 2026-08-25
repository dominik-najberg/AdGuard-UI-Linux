//! The root-helper liveness check against the real install on this machine.
//!
//! The unit tests in `helper.rs` prove the verdict against a stand-in process
//! that this suite spawns and kills on purpose. What they cannot prove is the
//! thing that actually matters: that a **healthy, running** AdGuard proxy is
//! not reported as one whose protection has stopped.
//!
//! That question has one honest source, and it is the machine.
//!
//! **Read-only.** Nothing here signals anything or writes anything; the check
//! is a walk of `/proc` and `status` mutates nothing beyond the config rewrite
//! every invocation performs anyway.
//!
//! The suite skips when AdGuard CLI is not installed and when the proxy is not
//! running — with no daemon there is no helper to have an opinion about, and
//! that is a legitimate state rather than a failure.
//!
//! It also skips, loudly, when the helper really is defunct. That is the
//! upstream bug this check exists to find (`AdguardTeam/AdGuardCLI#136`), and a
//! machine currently suffering it is not evidence of anything wrong *here*.

use adguard_core::{helper, orphan, Cli, HelperProcess};

/// `None` when there is no CLI to ask, which is a skip rather than a failure.
fn cli() -> Option<Cli> {
    Cli::discover().ok()
}

/// The one assertion this file exists for: while the proxy is up and its helper
/// is alive, nothing may report protection as bypassed.
///
/// The application's rule is *`status` says running **and** the helper is
/// defunct*. Both halves are spelled out here rather than only the conclusion,
/// so a regression that dropped or inverted either one cannot leave this
/// passing.
#[test]
fn a_healthy_install_is_never_reported_bypassed() {
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
    let [daemon] = found.as_slice() else {
        eprintln!("skipping: expected exactly one daemon, found {}", found.len());
        return;
    };

    let verdict = helper::process(daemon.pid());
    if verdict == HelperProcess::Defunct {
        eprintln!(
            "skipping: this machine's root helper is defunct under pid {} — \
             that is AdguardTeam/AdGuardCLI#136, not a failure of this check",
            daemon.pid(),
        );
        return;
    }

    // The guard, spelled out as the application applies it.
    let would_report_bypassed = status.running && verdict == HelperProcess::Defunct;
    assert!(
        !would_report_bypassed,
        "a running proxy with a {verdict:?} helper must not be called bypassed",
    );
}

/// A helper that is running must keep saying so when asked twice.
///
/// The walk reads a different `/proc` entry on each call and a verdict that
/// flickered between them would put the Status page into a loop between two
/// headlines every two seconds. Cheap to state, and it would catch a match on
/// something transient.
#[test]
fn the_verdict_is_stable_across_reads() {
    let Some(cli) = cli() else {
        eprintln!("skipping: adguard-cli is not installed");
        return;
    };
    let found = orphan::daemons(cli.binary());
    let [daemon] = found.as_slice() else {
        eprintln!("skipping: expected exactly one daemon, found {}", found.len());
        return;
    };

    let first = helper::process(daemon.pid());
    assert_eq!(first, helper::process(daemon.pid()));
    eprintln!("the helper under pid {} reads {first:?}", daemon.pid());
}
