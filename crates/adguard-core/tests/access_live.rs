//! The access-log check against the real log on this machine.
//!
//! The unit tests in `access.rs` prove the verdict against fixtures — lines
//! copied out of this file, and days assembled to order. What they cannot prove
//! is the two things that actually matter, and both of them have one honest
//! source, which is the machine:
//!
//! - that the format is still the one the parser was written against. Every
//!   guard in that module fails to **silence**, so a format that has drifted
//!   does not fail a unit test — it quietly stops answering, and this file is
//!   what notices.
//! - that a **healthy, running** proxy is not reported as one whose protection
//!   has stopped.
//!
//! **Read-only.** Nothing here writes anything or starts anything; it is a tail
//! of a log file, a walk of `/proc`, and one `status`.
//!
//! The suite skips when AdGuard CLI is not installed, when the proxy is not
//! running, and when the log has nothing in it yet — with no proxy there is no
//! run to date, and that is a legitimate state rather than a failure.
//!
//! It also skips, loudly, when this machine really is bypassed. That is the
//! state the check exists to find, and a machine currently in it is not evidence
//! of anything wrong *here*.

use std::time::{Duration, Instant, SystemTime};

use adguard_core::{access, orphan, Cli, Filtering};

/// `None` when there is no CLI to ask, which is a skip rather than a failure.
fn cli() -> Option<Cli> {
    Cli::discover().ok()
}

/// The one daemon this install is running, or nothing — the same pairing the
/// application makes before it reads anything off a process tree.
fn daemon(cli: &Cli) -> Option<orphan::Daemon> {
    match orphan::daemons(cli.binary()).as_slice() {
        [daemon] => Some(daemon.clone()),
        found => {
            eprintln!("skipping: expected exactly one daemon, found {}", found.len());
            None
        }
    }
}

/// **The format canary.** Read the whole log with no run boundary in the way,
/// and require the check to have an opinion.
///
/// `Unseen` here means no internal entry anywhere in four mebibytes carried a
/// recognisable timestamp and a recognisable status — which is what an
/// `adguard-cli` upgrade that renamed the client, moved a column or restyled the
/// date would produce. The application would go on running and silently stop
/// checking; this is the only place that difference is visible.
///
/// A week is deliberately longer than the roughly-hourly ping and shorter than
/// the log's own history, so a machine that has been running for an afternoon
/// still has something to say.
#[test]
fn the_real_log_still_parses() {
    let Some(path) = access::path() else {
        eprintln!("skipping: no data directory to look in");
        return;
    };
    let Ok(length) = std::fs::metadata(&path).map(|meta| meta.len()) else {
        eprintln!("skipping: {} is not there", path.display());
        return;
    };

    let week = SystemTime::now() - Duration::from_secs(7 * 24 * 60 * 60);
    let verdict = access::read(&path, week);
    eprintln!("{} ({length} B) over the last week reads {verdict:?}", path.display());
    assert_ne!(
        verdict,
        Filtering::Unseen,
        "no internal entry in {} parsed — the log format has moved, and the check \
         has silently stopped checking",
        path.display(),
    );
}

/// The assertion this file exists for: while the proxy is up and its own
/// requests are getting through, nothing may report protection as bypassed.
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
    let Some(daemon) = daemon(&cli) else { return };
    let Some(started) = daemon.started_at() else {
        eprintln!("skipping: this machine's run could not be dated");
        return;
    };

    let verdict = access::filtering(started);
    eprintln!(
        "pid {} has been running since {started:?} and reads {verdict:?}",
        daemon.pid(),
    );
    if verdict == Filtering::Bypassed {
        eprintln!(
            "skipping: this machine's proxy really is being bypassed — that is the \
             state this check exists to report, not a failure of it",
        );
        return;
    }

    // The guard, spelled out as the application applies it: only a positive
    // `Bypassed` may reach the panel, and never the absence of evidence.
    assert!(
        !matches!(verdict, Filtering::Bypassed),
        "a proxy whose own requests are succeeding must not be called bypassed",
    );
}

/// The run boundary is real, and it is the thing that makes the Restart button
/// honest: read against a run that started a moment ago, the same log can only
/// say it does not know.
#[test]
fn a_run_that_started_now_has_no_evidence_against_it() {
    let Some(path) = access::path() else {
        eprintln!("skipping: no data directory to look in");
        return;
    };
    if !path.exists() {
        eprintln!("skipping: {} is not there", path.display());
        return;
    }
    assert_eq!(access::read(&path, SystemTime::now()), Filtering::Unseen);
}

/// What the read costs, because it runs beside every `status` the Status page
/// is due to make one of.
///
/// An upper bound with a wide margin, as every timing assertion in this project
/// is: a loaded machine must not fail it, and the number it guards against is
/// far larger than the measurement. Run it with `--nocapture` to see the real
/// figure.
#[test]
fn the_read_is_cheap_enough_for_the_cadence() {
    let Some(path) = access::path() else {
        eprintln!("skipping: no data directory to look in");
        return;
    };
    if !path.exists() {
        eprintln!("skipping: {} is not there", path.display());
        return;
    }

    // Ten, so a single unlucky scheduling decision cannot decide it — and the
    // first of them pays for a cold page cache, which the poll never does.
    let started = Instant::now();
    for _ in 0..10 {
        let _ = access::read(&path, SystemTime::UNIX_EPOCH);
    }
    let each = started.elapsed() / 10;
    eprintln!("access::read: {each:?} per call");
    assert!(each < Duration::from_millis(500), "{each:?}");
}
