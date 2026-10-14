//! Whether browsers are refusing the certificates AdGuard signs, read from
//! AdGuard's own proxy log.
//!
//! [`crate::trust`] and [`crate::nss`] ask whether the stores hold AdGuard's
//! CA. That is **cause-specific**, and on 14 October 2026 it was answering
//! about the wrong certificate (see [`crate::trust::InUse`]). This check is
//! the cause-independent one, in the same relation to them as
//! [`crate::access`] is to [`crate::helper`]: it observes the failure itself,
//! whatever produced it.
//!
//! # The signal
//!
//! When a client aborts the handshake on a certificate AdGuard signed, AdGuard
//! logs one line and stops filtering that site for the client:
//!
//! ```text
//! 14.10.2026 09:51:42.668406 INFO  [16539] AGSslDomainFilter checkDomain: [id=1000020] TLS connection may not be filtered due to TLS error, adding 'chrome#142.250.130.101:443#clients4.google.com' to exceptions
//! ```
//!
//! That is the whole symptom the user sees: a certificate warning on a site's
//! first visit, and the site loading after a refresh — unfiltered, because the
//! refresh went round AdGuard.
//!
//! # The threshold, measured
//!
//! Every `proxy.log` generation on the reference machine, 25 August to 14
//! October 2026:
//!
//! ```text
//! healthy days          0 to 2 lines a day, 13 in fifty days
//! 3 October, 08h        66 lines in an hour   (the CA regenerated, measured)
//! 14 October, 09h–20h   30 to 280 an hour     (the CA in use replaced)
//! ```
//!
//! So [`THRESHOLD`] lines inside [`WINDOW`] — ten in a day, three times the
//! worst healthy day and below the quietest hour of either failure. The
//! occasional healthy line is an application that pins its certificates,
//! which is what the mechanism is for.
//!
//! # Scoped to the run, and to the day
//!
//! A restart is when a fixed certificate takes effect, so lines from before the
//! run began are not counted against it — the rule [`crate::access`] follows.
//! The day bound keeps a run of several weeks from adding up its odd healthy
//! line into an alarm.
//!
//! # What leaves this module
//!
//! A count, two times, and the names AdGuard gives the refusing programs —
//! `chrome`, `firefox`. **Never a host**: the report is pasted into public bug
//! reports (see [`crate::diagnostics`]), and the hosts are what the user
//! browsed.
//!
//! Nothing here escalates, spawns anything, or writes. It reads the tail of one
//! file, and of the generation before it when a rotation has just emptied it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The words AdGuard logs a refused handshake with, up to the quote that opens
/// `<app>#<address>#<host>`.
///
/// If AdGuard rewords it, nothing matches and the count stays at zero — the
/// direction this is meant to fail in.
const MARKER: &str = "TLS connection may not be filtered due to TLS error, adding '";

/// How much of the log to read. The same as [`crate::access`]'s, and on the
/// reference machine it covered the whole of 14 October's live file.
const TAIL: u64 = 4 * 1024 * 1024;

/// The generation before the live one, read when a rotation has left the live
/// file shorter than [`TAIL`].
const PREVIOUS: &str = "proxy.log.1";

/// Refusals inside [`WINDOW`] before the count is worth acting on. See the
/// module header for the measurement.
pub const THRESHOLD: usize = 10;

/// How far back a refusal still counts, at most.
pub const WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// What the proxy log says about refused certificates in one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejections {
    /// Handshakes refused since the run began, and inside [`WINDOW`].
    pub count: usize,
    /// The programs that refused them, as AdGuard names them, sorted and
    /// without repeats.
    pub apps: Vec<String>,
    /// The first and the latest of them, in epoch seconds.
    pub first: Option<i64>,
    pub last: Option<i64>,
}

impl Rejections {
    /// Enough refusals to say that browsers are not accepting AdGuard's
    /// certificates — not merely that one application pins its own.
    pub fn alarming(&self) -> bool {
        self.count >= THRESHOLD
    }
}

/// What this machine's proxy log says about the run that began at
/// `run_started`. `None` when AdGuard's data directory cannot be located; a
/// log that is missing or unreadable is an empty count, as no evidence.
pub fn rejections(run_started: SystemTime) -> Option<Rejections> {
    Some(read(&path()?, run_started, SystemTime::now()))
}

/// AdGuard's proxy log, wherever this machine's data directory is.
pub fn path() -> Option<PathBuf> {
    crate::paths::proxy_log()
}

/// [`rejections`], against a log at a path of the caller's choosing and at a
/// given moment, so a fixture can be read as though it were today.
pub fn read(path: &Path, run_started: SystemTime, now: SystemTime) -> Rejections {
    let seconds = |at: SystemTime| at.duration_since(UNIX_EPOCH).ok().map(|since| since.as_secs() as i64);
    let (Some(started), Some(now)) = (seconds(run_started), seconds(now)) else {
        return Rejections::default();
    };
    let since = started.max(now - WINDOW.as_secs() as i64);
    count(&crate::access::window_of(path, PREVIOUS, TAIL), since)
}

/// Count the refusals in a log tail at or after `since`, in epoch seconds.
fn count(tail: &str, since: i64) -> Rejections {
    let mut found = Rejections::default();
    for (at, app) in tail.lines().filter_map(refusal) {
        if at < since {
            continue;
        }
        found.count += 1;
        found.first.get_or_insert(at);
        found.last = Some(at);
        if !found.apps.iter().any(|known| known == app) {
            found.apps.push(app.to_owned());
        }
    }
    found.apps.sort();
    found
}

/// One refused handshake, as a time and the program AdGuard names — or `None`
/// for every other line, and for one this cannot read in full.
fn refusal(line: &str) -> Option<(i64, &str)> {
    let (_, rest) = line.split_once(MARKER)?;
    let (app, _) = rest.split_once('#')?;
    if app.is_empty() || app.contains(|c: char| c.is_whitespace() || c == '\'') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let at = crate::access::epoch(fields.next()?, fields.next()?)?;
    Some((at, app))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(clock: &str, app: &str) -> String {
        format!(
            "14.10.2026 {clock}.668406 INFO  [16539] AGSslDomainFilter checkDomain: [id=1000020] TLS \
             connection may not be filtered due to TLS error, adding '{app}#142.250.130.101:443#clients4.google.com' \
             to exceptions"
        )
    }

    fn at(clock: &str) -> i64 {
        let mut parts = clock.split(' ');
        crate::access::epoch(parts.next().unwrap(), parts.next().unwrap()).unwrap()
    }

    /// The line exactly as AdGuard wrote it on 14 October, read to a time and
    /// the program — and nothing of the host.
    #[test]
    fn a_real_line_reads_as_its_time_and_program() {
        let real = line("09:51:42", "chrome");
        let (when, app) = refusal(&real).expect("a refusal");
        assert_eq!(app, "chrome");
        assert_eq!(when, at("14.10.2026 09:51:42"));
    }

    #[test]
    fn other_lines_are_not_refusals() {
        for other in [
            "14.10.2026 20:42:51.442517 INFO  [89466] RootHelperClient disconnect: Finished",
            "14.10.2026 09:51:42.668406 INFO  [16539] AGSslDomainFilter checkDomain: adding 'chrome#x' to exceptions",
            "garbage TLS connection may not be filtered due to TLS error, adding 'chrome#1.2.3.4:443#a' to exceptions",
            "",
        ] {
            assert_eq!(refusal(other), None, "{other}");
        }
    }

    /// The run's edge: a refusal before the daemon started belongs to a run a
    /// restart has already ended.
    #[test]
    fn only_this_runs_refusals_count() {
        let log = [
            line("08:00:00", "firefox"),
            line("09:51:42", "chrome"),
            line("09:52:00", "chrome"),
            line("10:00:00", "firefox"),
        ]
        .join("\n");
        let found = count(&log, at("14.10.2026 09:51:00"));
        assert_eq!(found.count, 3);
        assert_eq!(found.apps, ["chrome", "firefox"]);
        assert_eq!(found.first, Some(at("14.10.2026 09:51:42")));
        assert_eq!(found.last, Some(at("14.10.2026 10:00:00")));
    }

    /// The healthy worst — a couple of pinning applications in a day — stays
    /// quiet, and the quietest failing hour does not.
    #[test]
    fn the_threshold_sits_between_the_measured_days() {
        let healthy = Rejections {
            count: 3,
            ..Rejections::default()
        };
        let failing = Rejections {
            count: 30,
            ..Rejections::default()
        };
        assert!(!healthy.alarming());
        assert!(failing.alarming());
    }

    /// The day bound, through the public reader: a refusal more than a day
    /// before `now` does not count, even within a run that old.
    #[test]
    fn a_long_run_counts_only_its_last_day() {
        let dir = std::env::temp_dir().join("adguard-ui-rejected-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("proxy.log");
        let mut text = String::from("13.10.2026 09:00:00.000000 INFO  [1] earlier\n");
        text.push_str(&line("09:51:42", "chrome").replace("14.10.2026", "12.10.2026"));
        text.push('\n');
        text.push_str(&line("09:51:42", "chrome"));
        text.push('\n');
        std::fs::write(&log, text).unwrap();

        let seconds = |stamp: &str| UNIX_EPOCH + Duration::from_secs(at(stamp) as u64);
        let found = read(&log, seconds("12.10.2026 00:00:00"), seconds("14.10.2026 12:00:00"));
        assert_eq!(found.count, 1);
    }
}
