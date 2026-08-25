//! Whether traffic is *reaching* the proxy, read from AdGuard's own access log.
//!
//! [`crate::helper`] asks whether the root helper is still alive. That check is
//! **cause-specific**: it catches a dead `adguard_root_helper` and nothing else.
//! This one is cause-independent — it observes whether filtering is happening,
//! rather than whether the machinery that should cause it still looks intact.
//!
//! Both exist because `adguard-cli status` reports **intent**. It says what is
//! configured and what is listening, never whether packets reach it. Every
//! failure of this shape looks identical from the Status page: "Protection is
//! on", and ads on the screen.
//!
//! # The signal
//!
//! AdGuard's own client issues roughly hourly requests through the proxy. They
//! appear in `<data>/logs/access.log` as `"internal_proxy_client"`:
//!
//! ```text
//! 25.08.2026 19:56:50.042068 "internal_proxy_client" HTTP1 CONNECT - - 502 any NONE 0 - - 171846b 202ms --
//! ```
//!
//! They go through AdGuard's own HTTP proxy, so they fail exactly when socket
//! protection does — and because they are AdGuard's rather than the user's,
//! their absence of success does not confound with an idle machine. An
//! unattended laptop still produces them.
//!
//! # The rule, and why it is not the one the issue proposed
//!
//! [Issue #14] states it as *within a single proxy run, if at least one internal
//! entry exists and none of them returned 200, traffic is not reaching the
//! proxy*. **Measured against the twelve days it was drawn from, that rule fires
//! on none of the five bypassed days.** Every one of those bypasses began
//! *mid-run*: the daemon started healthy, filtered for hours, and then stopped,
//! so each run carries plenty of 200s alongside the failure that followed them.
//! The run that produced the 2026-08-25 event — the one the whole check exists
//! for — holds 190 successes and then twenty-eight consecutive failures.
//!
//! What separates the two states is therefore not *no success in this run* but
//! **no success since**: the trailing entries, the ones after the last success.
//! Every internal entry within one run, over those twelve days, grouped into
//! maximal spans of failure bounded by a success or by the run's edge (contract
//! §9):
//!
//! ```text
//! filtering normally   longest span of failures:  2 entries over  60 minutes
//! bypassed             shortest span of failures: 19 entries over 18 hours
//! ```
//!
//! Five such long spans, and they land on exactly the five days that were spent
//! unprotected. So the discriminator survives, in the form the issue gives it —
//! *the absence of successes, not the presence of failures* — with the window
//! narrowed from the run to the tail of it. The issue's rule is the special case
//! where the run has had no success at all, and this one still covers it.
//!
//! The thresholds sit in the gap: [`FAILURES`] and [`SPAN`], three entries over
//! two hours, against a healthy maximum of two over one hour and a bypassed
//! minimum of nineteen over eighteen. Latency is one to two ping intervals,
//! which is why this **corroborates** the liveness check rather than racing it:
//! a dead helper is reported the moment `/proc` shows the corpse, and this
//! answers hours later for the bypasses that have no corpse to show.
//!
//! # The failures alone are not enough, and the log carries the rest
//!
//! A 502 means the request did not get through. It does not say *where* it
//! stopped, and a healthy proxy answers exactly the same way when
//! `filters.adtidy.org` — or the network — is unreachable. Reported as a bypass,
//! that would tell a user their pages had been loading unfiltered when they had
//! not been loading at all.
//!
//! So the window has to be quiet in the other sense too: **nobody else's traffic
//! may be reaching the proxy either**. A bypass takes traffic away from the
//! proxy, so its log empties; a dead upstream leaves the traffic arriving and
//! failing, so its log keeps filling. [`OTHER_TRAFFIC_PER_MINUTE`] has the
//! measurement and the one case this still cannot separate.
//!
//! **Scoping to the run is still load-bearing.** A restart is the measured cure,
//! and failures logged before one must not count against the run that followed
//! it, or the panel would go on reporting a bypass the user had just fixed.
//!
//! # Failing to silence
//!
//! `access.log`'s format is not versioned and is not part of any contract, so a
//! parser that stops recognising the line must fail to **silence**, never to a
//! false alarm: reporting that protection is off when it is on teaches the user
//! to disregard the indicator. Three things enforce that direction here, and all
//! of them turn an unrecognised line into no evidence rather than into evidence
//! of failure:
//!
//! - a line is read only when it has exactly [`FIELDS`] fields, which is what
//!   every one of the 2,496 internal entries across ten rotations has;
//! - its status must parse as an HTTP status code. Every other column on the
//!   line — `any`, `NONE`, `0`, `58b`, `111ms`, `--`, a hostname — fails that
//!   test, so a column that shifts silences the check instead of moving the
//!   verdict;
//! - anything in the 2xx range counts as a success, which is the half of the
//!   test that can only ever quieten the verdict.
//!
//! Nothing here escalates, spawns anything, or writes. It reads the tail of one
//! file, and of the generation before it when a rotation has just emptied that
//! one — see [`PREVIOUS`].
//!
//! [Issue #14]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/14

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The client name AdGuard gives its own requests, quoted as the log quotes it.
///
/// If AdGuard ever renames it, nothing matches, [`Filtering::Unseen`] comes back
/// and this application says nothing — the direction this is meant to fail in.
const INTERNAL_CLIENT: &str = "\"internal_proxy_client\"";

/// Fields in one access-log line (contract §9).
///
/// Measured constant at sixteen across every internal entry in ten rotations —
/// 2,496 lines, no other count. Requiring it exactly is the cheapest guard
/// there is against a format that gains or loses a column: the line stops
/// parsing, and a line that does not parse is not evidence of anything.
const FIELDS: usize = 16;

/// Zero-based index of the quoted client field.
const CLIENT: usize = 2;

/// Zero-based index of the status code — `200`, `502`, or `-` on the TLS lines
/// that carry no status of their own.
const STATUS: usize = 7;

/// How much of the log to read back through.
///
/// **Sized against a measured generation, not guessed.** AdGuard rotates this
/// file itself at ~10 MiB (contract §9); one full generation on the reference
/// machine spans 24.08.2026 22:20 to 25.08.2026 23:12 — twenty-five hours, so
/// roughly 400 KiB an hour, and the busiest generation measured runs at about
/// 1 MiB an hour. Four mebibytes is therefore four hours of the worst measured
/// traffic and ten of the ordinary kind, against a [`SPAN`] of two.
///
/// It is more window than the state being detected needs. A bypassed proxy logs
/// almost nothing — traffic that is not reaching the proxy is not in the proxy's
/// log either — so what this size actually buys is the awkward middle case:
/// `manual` mode, where a dead helper breaks the HTTP proxy while the SOCKS5
/// proxy beside it goes on serving, and the log stays busy while the internal
/// requests fail.
///
/// A truncated window can only lose failures, never invent them, so the cost of
/// getting this wrong is a missed detection.
const TAIL: u64 = 4 * 1024 * 1024;

/// The rotated generation immediately before the live one.
///
/// **Read because rotation is a real hole and this is the cheap half of
/// closing it.** A roll leaves `access.log` a few kilobytes long, and until it
/// refills there is no window to read — measured while writing this module,
/// where the file rolled and the next reading covered three minutes.
///
/// The seam is continuous, measured (contract §9): one generation ends and the
/// next begins ~1 ms later under the same pid, so the two really are one
/// stream and joining them invents no gap.
///
/// The hole this leaves closed is not the dangerous one either way. Rotation is
/// driven by traffic volume and a bypass produces no traffic, so a bypass in
/// `auto` mode cannot roll the log out from under its own evidence.
const PREVIOUS: &str = "access.log.1";

/// How many trailing failures before the verdict is worth acting on.
///
/// Three, against a measured maximum of two in a run that was filtering
/// normally and a minimum of nineteen in one that was not.
const FAILURES: usize = 3;

/// How much of everybody else's traffic may appear in the failure window before
/// it stops being evidence of a bypass.
///
/// **The failures alone do not distinguish a bypass from a dead upstream.**
/// AdGuard's internal request answers 502 when nothing is reaching the proxy,
/// and it answers 502 when the proxy is fine and `filters.adtidy.org` — or the
/// network — is not. Every 502 measured here carries `-` where a successful one
/// carries an upstream address, so the line itself cannot tell the two apart.
///
/// What can is the rest of the log. The two states differ in what the *user's*
/// traffic is doing:
///
/// - **Bypassed.** Traffic is not being redirected into the proxy, so it never
///   appears in the proxy's log. Measured across the five bypassed spans on the
///   reference machine: **0.1 to 5.1 entries an hour**, against **1,036 an
///   hour** everywhere else — four orders of magnitude, and the same
///   coincidence contract §8's day table reports from the other end.
/// - **Upstream or network down.** Traffic *is* still being redirected into the
///   proxy; it arrives and fails there, so the log keeps filling.
///
/// So a window busy with other clients is a window in which traffic is reaching
/// the proxy, whatever AdGuard's own request made of it. Two entries a minute
/// is 2.8× the worst rate measured inside a real bypass — 43 an hour, over the
/// two hours of 25.08 that carried the most — and 8.6× below the ordinary one.
///
/// **It does not close the gap entirely, and the wording of the state says so.**
/// A machine that is powered on and has been off the network for hours logs
/// neither its own traffic nor a successful check, and reads from here exactly
/// as a bypass does. Nothing in this file can separate those two, so the panel
/// names the observation and offers that reading rather than asserting a
/// bypass.
const OTHER_TRAFFIC_PER_MINUTE: i64 = 2;

/// How long those failures must span.
///
/// Two hours — two ping intervals — against a measured maximum of one hour of
/// failures in a healthy run and a minimum of eighteen in a bypassed one. The
/// count alone would already separate the two; the span is what keeps a burst
/// of failures inside one second, of the kind a restart produces, from reading
/// as hours of them.
const SPAN: Duration = Duration::from_secs(2 * 60 * 60);

/// What AdGuard's own requests say about the proxy they went through.
///
/// The three states are distinguishable, which is what makes the signal usable
/// rather than merely suggestive:
///
/// | State | Internal entries | Successes |
/// | --- | --- | --- |
/// | Machine off, or the log is unreadable | none at all | — |
/// | Running, filtering | present | some |
/// | Running, bypassed | present | none, for hours |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filtering {
    /// A request through the proxy has succeeded since this run began.
    Reaching,
    /// Requests have been attempted since the last success and every one of them
    /// failed, for long enough that the next one was due several times over.
    ///
    /// **The only value here that is evidence of anything**, and the one a
    /// caller may act on.
    Bypassed,
    /// Nothing to go on: no internal entries in this run, a log that could not
    /// be read, or failures that have not yet met [`FAILURES`] and [`SPAN`].
    ///
    /// Deliberately not merged with [`Self::Reaching`]. "Traffic is getting
    /// through" and "we cannot tell" are different facts, and only the first of
    /// them is a reason to be reassured.
    Unseen,
}

/// What the access log says about the proxy run that began at `run_started`.
///
/// [`Filtering::Unseen`] when AdGuard is not installed, when the log is absent
/// or unreadable, and when the run is too young to have produced evidence —
/// three different things that mean the same to every caller here: nothing is
/// known, so nothing is claimed.
pub fn filtering(run_started: SystemTime) -> Filtering {
    match crate::paths::access_log() {
        Some(path) => read(&path, run_started),
        None => Filtering::Unseen,
    }
}

/// [`filtering`], against a log at a path of the caller's choosing.
///
/// The path is a parameter for the reason [`crate::helper::RootHelper::inspect`]
/// documents: the state this check exists to report is one the reference machine
/// is not in most of the time, and a constant buried in the function would leave
/// the verdict that matters unprovable except by breaking a working install.
/// Here a fixture is a text file, so both ends are covered against lines this
/// machine really wrote.
pub fn read(path: &Path, run_started: SystemTime) -> Filtering {
    let Ok(since) = run_started.duration_since(UNIX_EPOCH) else {
        // A run that began before 1970 is a clock this cannot reason about.
        return Filtering::Unseen;
    };
    verdict(&window(path), since.as_secs() as i64)
}

/// AdGuard's access log, wherever this machine's data directory is.
pub fn path() -> Option<PathBuf> {
    crate::paths::access_log()
}

/// Decide from the text of a log tail, given the run's start in epoch seconds.
///
/// Kept separate from [`read`] so the verdict can be tested against days this
/// machine is not currently having.
fn verdict(tail: &str, since: i64) -> Filtering {
    let mut reached = false;
    // The trailing failures: those after the last success, which is the window
    // the module header measures. A success empties it, so what survives to the
    // end of the loop is exactly the tail of the run.
    let mut failures: Vec<i64> = Vec::new();
    // Everybody else's requests inside that same window — see
    // [`OTHER_TRAFFIC_PER_MINUTE`]. Counted by position rather than by time,
    // which is exact because the log is chronological and costs no parse: the
    // window opens at the first trailing failure, so a line that is neither an
    // internal entry nor blank counts from that point on.
    let mut others: i64 = 0;

    for line in tail.lines() {
        let Some((at, status)) = internal_entry(line) else {
            // Permissive on purpose, and the opposite way round from
            // `internal_entry`: a line this cannot read still counts as
            // somebody's traffic, so a format that has drifted quietens the
            // verdict here too rather than sharpening it.
            if !failures.is_empty() && !line.trim().is_empty() {
                others += 1;
            }
            continue;
        };
        // Before the daemon started, so it belongs to a run a restart has
        // already ended. Neither half of it counts.
        if at < since {
            continue;
        }
        if (200..300).contains(&status) {
            reached = true;
            failures.clear();
            others = 0;
        } else {
            if failures.is_empty() {
                others = 0;
            }
            failures.push(at);
        }
    }

    // `max(0)` rather than an assumption of order: the timestamps are local
    // wall clock, and an hour of it repeats once a year.
    let span = match (failures.first(), failures.last()) {
        (Some(first), Some(last)) => (last - first).max(0),
        _ => 0,
    };
    let quiet = others <= span / 60 * OTHER_TRAFFIC_PER_MINUTE;
    if failures.len() >= FAILURES && span >= SPAN.as_secs() as i64 && quiet {
        Filtering::Bypassed
    } else if reached {
        Filtering::Reaching
    } else {
        // Includes the vetoed case: other traffic is reaching the proxy, so the
        // failures are not evidence of a bypass — and nothing here has seen a
        // success either, so they are not evidence of health.
        Filtering::Unseen
    }
}

/// One of AdGuard's own requests, as a time and a status — or `None` for every
/// other line, and for anything this parser does not positively recognise.
fn internal_entry(line: &str) -> Option<(i64, u16)> {
    // The overwhelming majority of the file is somebody else's traffic, and a
    // substring scan rejects those without splitting or allocating anything.
    // Not the test — a URL is free to contain that name, and the field check
    // below is what decides — only a way of not doing the work 98% of the time.
    // Worth the two lines: it took a full-window read on the reference machine
    // from 85 ms to 15 ms.
    if !line.contains(INTERNAL_CLIENT) {
        return None;
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != FIELDS || fields[CLIENT] != INTERNAL_CLIENT {
        return None;
    }
    // A status, and demonstrably a status. The TLS lines carry `-` here and drop
    // out, which is right: they are the payload inside a `CONNECT` whose own
    // line is counted already.
    let status: u16 = fields[STATUS].parse().ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    Some((epoch(fields[0], fields[1])?, status))
}

/// `25.08.2026` + `22:40:19.394002` as seconds since the epoch.
///
/// The timestamps are **local wall clock** with no offset written down, so the
/// conversion has to go through the machine's own timezone rules — which is
/// what [`local`] is for. The microseconds are dropped: whole seconds are four
/// orders of magnitude finer than the two-hour window they are compared in.
fn epoch(date: &str, clock: &str) -> Option<i64> {
    let mut parts = date.split('.');
    let day = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let year = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    let mut parts = clock.split(':');
    let hour = parts.next()?.parse().ok()?;
    let minute = parts.next()?.parse().ok()?;
    let second = parts.next()?.split('.').next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    // Range-checked before `mktime` sees them, because `mktime` *normalises*:
    // it would turn a garbage month 47 into a date three years out rather than
    // refusing it, and a garbage line must produce no reading at all.
    let ranges = [
        (month, 1..=12),
        (day, 1..=31),
        (hour, 0..=23),
        (minute, 0..=59),
        // Leap seconds are spelled 60 and the kernel does write them.
        (second, 0..=60),
    ];
    if !ranges.iter().all(|(value, range)| range.contains(value)) {
        return None;
    }
    local(year, month, day, hour, minute, second)
}

/// A local-time civil date as seconds since the epoch.
///
/// `tm_isdst = -1` asks the C library to work out for itself whether summer time
/// was in force, which is the only correct answer for a timestamp written
/// without an offset. It is also the honest one: at the hour that repeats each
/// autumn the question genuinely has two answers, and picking either shifts a
/// reading by an hour inside a two-hour window. That can delay a verdict or
/// bring it forward; it cannot invent one, because the entries it is measuring
/// all shift together.
fn local(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32) -> Option<i64> {
    // SAFETY: a zeroed `tm` is a valid one — every field is an `int` — and
    // `mktime` reads and normalises the struct we hand it and nothing else.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = second;
    tm.tm_isdst = -1;

    // SAFETY: `tm` is a live, fully initialised local, and `mktime` writes only
    // through the pointer it is given.
    let seconds = unsafe { libc::mktime(&mut tm) };
    (seconds != -1).then_some(seconds as i64)
}

/// The last [`TAIL`] bytes of the log, reaching back into the previous
/// generation when a rotation has left the live one shorter than that.
///
/// Never an error: an unreadable file, a missing one and an empty one all come
/// back as no text, which [`verdict`] reads as no evidence.
fn window(path: &Path) -> String {
    let live = tail(path, TAIL);
    let shortfall = TAIL.saturating_sub(live.len() as u64);
    if shortfall == 0 {
        return live;
    }

    let Some(previous) = path.parent().map(|dir| dir.join(PREVIOUS)) else {
        return live;
    };
    let mut earlier = tail(&previous, shortfall);
    if earlier.is_empty() {
        return live;
    }
    // The generations are separate files, so nothing guarantees the earlier one
    // ended on a line boundary the way a single stream would.
    if !earlier.ends_with('\n') {
        earlier.push('\n');
    }
    earlier.push_str(&live);
    earlier
}

/// The last `want` bytes of a file, as text — empty when there is nothing to
/// read or the file cannot be read at all.
///
/// The first line is dropped whenever the read began part-way in, because a line
/// cut through the middle can parse into anything — and the one thing this
/// module may not do is read a fragment as a failure.
fn tail(path: &Path, want: u64) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    let from = length.saturating_sub(want);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }

    let mut bytes = Vec::with_capacity(length.min(want) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    // Lossy rather than strict: one mangled byte anywhere in four mebibytes of
    // somebody else's log must not cost the whole reading. What it mangles stops
    // parsing, which is the safe direction.
    let mut text = String::from_utf8_lossy(&bytes).into_owned();

    if from > 0 {
        match text.find('\n') {
            Some(newline) => {
                text.drain(..=newline);
            }
            // Four mebibytes without a line ending is not a log.
            None => return String::new(),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line, from a run that was filtering.
    const SUCCESS: &str = "25.08.2026 22:40:19.394002 \"internal_proxy_client\" HTTP1 CONNECT - - 200 any NONE 0 - 130.117.76.67:443 58b 111ms -- ";

    /// A real line, from the seventeen hours of 2026-08-25 that were not.
    const FAILURE: &str = "25.08.2026 19:56:50.042068 \"internal_proxy_client\" HTTP1 CONNECT - - 502 any NONE 0 - - 171846b 202ms -- ";

    /// The other shape an internal entry takes: the TLS payload inside the
    /// `CONNECT` above, carrying `-` where the status would be.
    const TUNNEL: &str = "25.08.2026 22:40:19.467201 \"internal_proxy_client\" TLS - filters.adtidy.org - - any NONE 0 - - 17406b 185ms -- ";

    /// Somebody else's request. Every line that is not AdGuard's own is one of
    /// these, and they are the overwhelming majority of the file.
    const REAL: &str = "25.08.2026 23:03:32.864735 \"chrome\" HTTP2 POST https://chat.google.com/u/0/_/x https://chat.google.com/ 200 xhr NONE 0 - 192.178.213.138:443 2649b 162ms -- ";

    fn at(clock: &str) -> i64 {
        epoch("25.08.2026", clock).expect("a timestamp this machine wrote")
    }

    /// One internal entry, spelled as the log spells it.
    fn entry(clock: &str, status: &str) -> String {
        format!(
            "25.08.2026 {clock}.000000 \"internal_proxy_client\" HTTP1 CONNECT - - {status} any NONE 0 - - 171846b 202ms --"
        )
    }

    /// The run began at midnight, which is before every fixture below.
    fn midnight() -> i64 {
        at("00:00:00")
    }

    /// The same moment as [`read`] wants it.
    fn run_at(epoch: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(epoch as u64)
    }

    #[test]
    fn reads_a_success_and_a_failure() {
        assert_eq!(internal_entry(SUCCESS), Some((at("22:40:19"), 200)));
        assert_eq!(internal_entry(FAILURE), Some((at("19:56:50"), 502)));
    }

    /// The TLS half of a tunnel has no status and must not become one. It is not
    /// a failure — it is the inside of a `CONNECT` that was already counted.
    #[test]
    fn a_tunnel_line_carries_no_verdict() {
        assert_eq!(internal_entry(TUNNEL), None);
    }

    /// The user's own traffic says nothing about whether it was filtered, so it
    /// is never read as evidence either way.
    #[test]
    fn somebody_elses_request_is_not_an_internal_entry() {
        assert_eq!(internal_entry(REAL), None);
    }

    /// The measured bypass: hourly failures, nothing succeeding.
    #[test]
    fn hours_of_failure_with_no_success_is_a_bypass() {
        let log = (0..4)
            .map(|hour| entry(&format!("{:02}:56:42", hour + 2), "502"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(verdict(&log, midnight()), Filtering::Bypassed);
    }

    /// The shape the issue's own rule was written for: a run that has never
    /// succeeded. Still covered, because "since the last success" falls back to
    /// "since the run began" when there has not been one.
    #[test]
    fn a_run_that_never_succeeded_is_a_bypass() {
        let log = format!(
            "{}\n{}\n{}",
            entry("08:21:25", "502"),
            entry("09:21:26", "502"),
            entry("10:21:26", "502"),
        );
        assert_eq!(verdict(&log, midnight()), Filtering::Bypassed);
    }

    /// **The case the issue's rule misses, and the reason this one is worded
    /// differently.** Every measured bypass began mid-run, so the run carries
    /// successes and then stops carrying them.
    #[test]
    fn a_run_that_succeeded_and_then_stopped_is_a_bypass() {
        let mut log = vec![entry("00:56:42", "200"), entry("01:56:41", "200")];
        for hour in 2..6 {
            log.push(entry(&format!("{hour:02}:56:42"), "502"));
        }
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Bypassed);
    }

    /// Two failures an hour apart, between successes — measured on 18.08, a day
    /// that filtered 14,587 requests. The longest such span in twelve days of
    /// healthy running, and it must not raise anything.
    #[test]
    fn the_worst_measured_healthy_run_is_not_a_bypass() {
        let log = format!(
            "{}\n{}\n{}\n{}",
            entry("06:51:28", "200"),
            entry("07:51:28", "502"),
            entry("08:51:33", "502"),
            entry("09:51:33", "200"),
        );
        assert_eq!(verdict(&log, midnight()), Filtering::Reaching);
    }

    /// Failures still count when the successes that surrounded them have scrolled
    /// out of the window — but two of them are two of them either way.
    #[test]
    fn two_trailing_failures_are_not_enough() {
        let log = format!("{}\n{}", entry("07:51:28", "502"), entry("08:51:33", "502"));
        assert_eq!(verdict(&log, midnight()), Filtering::Unseen);
    }

    /// Three failures inside a second is a restart, not seventeen hours of
    /// browsing unprotected. [`SPAN`] is what tells them apart.
    #[test]
    fn a_burst_of_failures_is_not_a_bypass() {
        let log = format!(
            "{}\n{}\n{}",
            entry("06:51:28", "502"),
            entry("06:51:28", "502"),
            entry("06:51:29", "502"),
        );
        assert_eq!(verdict(&log, midnight()), Filtering::Unseen);
    }

    /// A success ends the streak, whatever came before it. This is the whole of
    /// the recovery path: the panel clears itself on the next reading rather
    /// than on a timer of ours.
    #[test]
    fn one_success_clears_a_days_worth_of_failures() {
        let mut log: Vec<String> = (2..20)
            .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
            .collect();
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Bypassed);
        log.push(entry("20:40:20", "200"));
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Reaching);
    }

    /// **Scoping to the run, which is what makes the restart button honest.**
    /// The same log, read against a daemon that started after the failures,
    /// says nothing at all.
    #[test]
    fn failures_from_before_the_run_do_not_count() {
        let log: Vec<String> = (2..20)
            .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
            .collect();
        let log = log.join("\n");
        assert_eq!(verdict(&log, midnight()), Filtering::Bypassed);
        assert_eq!(verdict(&log, at("20:40:45")), Filtering::Unseen);
    }

    /// An empty log, and a log of somebody else's traffic. Neither is evidence.
    #[test]
    fn nothing_to_go_on_is_never_a_bypass() {
        assert_eq!(verdict("", midnight()), Filtering::Unseen);
        assert_eq!(verdict(REAL, midnight()), Filtering::Unseen);
    }

    /// **The fail-to-silence guard, stated as the risk it covers.** A format
    /// that gains or loses a column, or moves the status somewhere else, must
    /// take the check off rather than turn it on.
    ///
    /// Each of these is the bypassed log above with one thing changed, and every
    /// one of them has to come back [`Filtering::Unseen`] — not
    /// [`Filtering::Bypassed`], which is what a parser that read the wrong
    /// column as a failing status would produce.
    #[test]
    fn a_format_that_stops_parsing_stops_the_check() {
        let bypassed: Vec<String> = (2..8)
            .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
            .collect();
        let mutate = |change: &dyn Fn(&str) -> String| {
            let log: Vec<String> = bypassed.iter().map(|line| change(line)).collect();
            verdict(&log.join("\n"), midnight())
        };

        // Sanity: unchanged, these lines really are a bypass.
        assert_eq!(mutate(&|line: &str| line.to_owned()), Filtering::Bypassed);

        // A seventeenth column.
        assert_eq!(mutate(&|line: &str| format!("{line} extra")), Filtering::Unseen);
        // A column removed, which also shifts the status onto `CONNECT`.
        assert_eq!(
            mutate(&|line: &str| line.replacen("HTTP1 ", "", 1)),
            Filtering::Unseen
        );
        // The client renamed.
        assert_eq!(
            mutate(&|line: &str| line.replace("internal_proxy_client", "internal_client")),
            Filtering::Unseen
        );
        // The status spelled some other way.
        assert_eq!(
            mutate(&|line: &str| line.replace(" 502 ", " BAD_GATEWAY ")),
            Filtering::Unseen
        );
        // The date in some other order. ISO would parse as day 2026 of month 8.
        assert_eq!(
            mutate(&|line: &str| line.replace("25.08.2026", "2026-08-25")),
            Filtering::Unseen
        );
    }

    /// Every column that is not the status, offered to the status parser. None
    /// of them may read as one, which is what makes the column guard above worth
    /// having rather than merely reassuring.
    #[test]
    fn no_other_column_could_be_mistaken_for_a_status() {
        let columns: Vec<&str> = SUCCESS.split_whitespace().collect();
        for (index, column) in columns.iter().enumerate() {
            if index == STATUS {
                continue;
            }
            let reads_as_status = column
                .parse::<u16>()
                .is_ok_and(|code| (100..=599).contains(&code));
            assert!(!reads_as_status, "column {index} ({column:?}) parses as a status");
        }
    }

    /// A 2xx that is not 200 is still a success. Being generous here can only
    /// quieten the verdict, which is the direction this module fails in.
    #[test]
    fn any_two_hundred_is_a_success() {
        let log = format!(
            "{}\n{}\n{}\n{}",
            entry("02:56:42", "502"),
            entry("03:56:42", "502"),
            entry("04:56:42", "502"),
            entry("05:56:42", "204"),
        );
        assert_eq!(verdict(&log, midnight()), Filtering::Reaching);
    }

    /// The timestamp parser, against the shapes a corrupted or rotated file can
    /// hand it. A half-line is the ordinary case — [`tail`] cuts one every time
    /// the file is longer than the window.
    #[test]
    fn rejects_a_timestamp_that_is_not_one() {
        for (date, clock) in [
            ("", ""),
            ("25.08", "22:40:19"),
            ("25.08.2026.1", "22:40:19"),
            ("47.08.2026", "22:40:19"),
            ("25.13.2026", "22:40:19"),
            ("25.08.2026", "25:40:19"),
            ("25.08.2026", "22:61:19"),
            ("25.08.2026", "22:40"),
            ("25.08.2026", "22:40:19:00"),
            ("25.08.2026", "-1:40:19"),
        ] {
            assert!(epoch(date, clock).is_none(), "{date:?} {clock:?} should not parse");
        }
    }

    /// Two timestamps an hour apart really are 3,600 seconds apart, which is the
    /// arithmetic [`SPAN`] rests on.
    #[test]
    fn an_hour_of_log_time_is_an_hour() {
        assert_eq!(at("08:51:33") - at("07:51:33"), 3_600);
    }

    /// The tail is the *end* of the file, and it never begins with a fragment.
    #[test]
    fn the_tail_drops_the_line_it_cut() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("access.log");

        // Longer than the window, and every line individually identifiable.
        let filler = "x".repeat(1_023);
        let mut log = String::with_capacity(TAIL as usize * 2);
        while log.len() < TAIL as usize * 2 {
            log.push_str(&filler);
            log.push('\n');
        }
        log.push_str(SUCCESS);
        log.push('\n');
        std::fs::write(&path, &log).expect("write the log");

        let read = tail(&path, TAIL);
        assert!(read.len() <= TAIL as usize, "{} bytes", read.len());
        assert!(read.ends_with(&format!("{SUCCESS}\n")));
        // Whatever the window cut, it is not in what came back.
        for line in read.lines() {
            assert!(
                line.len() == filler.len() || line == SUCCESS,
                "a fragment survived: {} bytes",
                line.len()
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **A rotation must not blind the check.** Measured while writing this
    /// module: `access.log` rolled and what was left of it covered three
    /// minutes, which is no window at all against a [`SPAN`] of two hours.
    ///
    /// The bypass here is written across the seam, most of it in the generation
    /// that has just been rotated away, and it still has to be seen.
    #[test]
    fn a_bypass_survives_the_log_rolling_over() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-rolled-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let live = dir.join("access.log");
        let rolled = dir.join(PREVIOUS);

        let hourly = |hours: std::ops::Range<u32>| {
            hours
                .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        // Everything but the last ping is in the generation that rolled away.
        std::fs::write(&rolled, hourly(2..8)).expect("write the rolled log");
        std::fs::write(&live, hourly(8..9)).expect("write the live log");
        assert_eq!(read(&live, run_at(midnight())), Filtering::Bypassed);

        // And a live log long enough to fill the window on its own must not
        // reach back for one it does not need: a success there is the answer.
        let mut filled = String::new();
        while filled.len() < TAIL as usize {
            filled.push_str(&entry("09:00:00", "200"));
            filled.push('\n');
        }
        std::fs::write(&live, &filled).expect("write the live log");
        assert_eq!(read(&live, run_at(midnight())), Filtering::Reaching);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The guard against a dead upstream.** The same failures, in a window
    /// busy with somebody else's requests, are not evidence of a bypass — a
    /// bypass takes traffic *away* from the proxy, so a proxy still being asked
    /// for things is a proxy traffic is still reaching.
    #[test]
    fn failures_beside_other_traffic_are_not_a_bypass() {
        let mut log: Vec<String> = (2..8)
            .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
            .collect();
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Bypassed);

        // A browser going about its business through the same proxy, at a rate
        // this machine reaches inside a minute of ordinary use.
        for _ in 0..2_000 {
            log.push(REAL.to_owned());
        }
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Unseen);
    }

    /// The trickle that survives a real bypass must not veto it.
    ///
    /// Measured on the reference machine: 87 entries — `chronyd`, a little
    /// `chrome`, `slack` — across the busiest two hours of the 25.08 bypass,
    /// which is 43 an hour against an ordinary rate of 1,036. The threshold sits
    /// between them, and this is the side of it that has to keep working.
    #[test]
    fn the_trickle_that_survives_a_bypass_does_not_veto_it() {
        let mut log: Vec<String> = Vec::new();
        for hour in 2..8 {
            log.push(entry(&format!("{hour:02}:56:42"), "502"));
            // 43 an hour, spread through the window rather than heaped at
            // either end, because the count is what is being tested.
            for _ in 0..43 {
                log.push(REAL.to_owned());
            }
        }
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Bypassed);
    }

    /// A line the parser cannot read counts as somebody's traffic, not as
    /// nothing. The permissive direction here is the quiet one: an unreadable
    /// log vetoes the verdict rather than sharpening it.
    #[test]
    fn unreadable_lines_count_towards_the_veto() {
        let mut log: Vec<String> = (2..8)
            .map(|hour| entry(&format!("{hour:02}:56:42"), "502"))
            .collect();
        for _ in 0..2_000 {
            log.push("something this parser has never seen".to_owned());
        }
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Unseen);
    }

    /// Traffic from *before* the window does not veto what happens after it.
    /// The window opens at the first trailing failure, and a busy morning
    /// followed by a silent afternoon is the shape a bypass beginning at noon
    /// actually has.
    #[test]
    fn traffic_before_the_window_does_not_veto_it() {
        let mut log = vec![entry("01:56:42", "200")];
        for _ in 0..5_000 {
            log.push(REAL.to_owned());
        }
        for hour in 2..8 {
            log.push(entry(&format!("{hour:02}:56:42"), "502"));
        }
        assert_eq!(verdict(&log.join("\n"), midnight()), Filtering::Bypassed);
    }

    /// A log that is not there is not a bypass — the state a machine that has
    /// never run the proxy is in.
    #[test]
    fn an_absent_log_says_nothing() {
        let path = std::env::temp_dir().join("adguard-ui-access-test/definitely-absent.log");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read(&path, SystemTime::now()), Filtering::Unseen);
    }
}
