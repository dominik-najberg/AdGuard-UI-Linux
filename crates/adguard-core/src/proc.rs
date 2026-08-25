//! Reading one process's line in `/proc`.
//!
//! Two modules ask `/proc/<pid>/stat` different questions about the same
//! install — [`crate::orphan`] whether a daemon it found earlier is still that
//! same live process, [`crate::helper`] whether AdGuard's root helper is still
//! running under one — and both turn on fields this file is the only place to
//! parse.
//!
//! Nothing here escalates or writes. It reads one file per process and the
//! kernel decides what a process may see, which for this file is everything.

use std::fs;

/// The state character of a process that has exited and has not been reaped.
///
/// Not a curiosity in this crate: it is the shape both of the wedged daemon
/// [`crate::orphan`] clears and of the dead root helper [`crate::helper`]
/// reports, and in both the process is *present* in `/proc` while being no
/// use to anyone.
pub(crate) const ZOMBIE: char = 'Z';

/// The fields of `/proc/<pid>/stat` this crate reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stat {
    /// Field 2 — the executable's name, with its parentheses removed.
    ///
    /// **The kernel truncates this to fifteen characters**, so AdGuard's
    /// `adguard_root_helper` is `adguard_root_he` here, in `ps`, and anywhere
    /// else this field surfaces. Anything comparing against it has to be
    /// written against the truncated spelling; see [`crate::helper`].
    pub comm: String,
    /// Field 3 — the process state. [`ZOMBIE`] is the one value this crate
    /// makes a decision on.
    pub state: char,
    /// Field 4 — the parent's pid. What makes a process findable as *some
    /// particular* daemon's child rather than merely as a process of the right
    /// name.
    pub ppid: i32,
    /// Field 22 — the start time in clock ticks since boot. Constant for the
    /// life of a process and effectively unique per pid, so it is what tells a
    /// process from a later one handed the same number.
    pub started: u64,
}

/// The moment the machine booted, in seconds since the epoch.
///
/// `btime` in `/proc/stat` — a whole number of seconds, and the only thing that
/// turns [`Stat::started`] into a wall-clock time. The pair is what
/// [`crate::orphan::Daemon::started_at`] needs so a log written in wall clock
/// can be scoped to one proxy run.
///
/// `None` for a `/proc/stat` that cannot be read or does not carry the field,
/// which means the same as everything else here: nothing is known.
pub(crate) fn boot_time() -> Option<i64> {
    fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// Clock ticks per second — the unit [`Stat::started`] counts in.
///
/// 100 on every Linux this application targets, but read rather than assumed:
/// it is a property of the kernel the binary is running on, not of the binary.
/// A non-positive answer means `sysconf` could not tell us, and the caller
/// treats that as unknown rather than dividing by it.
pub(crate) fn ticks_per_second() -> i64 {
    // SAFETY: `sysconf` reads a constant of the C library, takes one argument
    // and touches nothing of ours. It is `unsafe` only because it is extern.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) }
}

/// Read `/proc/<pid>/stat`.
///
/// `None` for a pid that has gone, a `/proc` that cannot be read, and a line
/// that does not parse — three different things that all mean the same to every
/// caller here: nothing is known about this process, so nothing is decided from
/// it.
pub(crate) fn stat(pid: i32) -> Option<Stat> {
    parse(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Split one `stat` line.
///
/// Field 2 is the executable name **in parentheses and unescaped**, so it may
/// hold whitespace and parentheses of its own — `[adguard_root_he]` is tame,
/// but a process is free to be called `(a b) c`. The file therefore cannot be
/// split on whitespace from the left. Everything after the **last** `)` is
/// field 3 onwards, which is the standard way through it, and the name is what
/// lies between the **first** `(` and that same `)`.
///
/// Kept separate from [`stat`] so the parse can be tested against lines this
/// machine does not happen to produce.
fn parse(line: &str) -> Option<Stat> {
    let open = line.find('(')?;
    let (through_comm, rest) = line.rsplit_once(')')?;
    let comm = through_comm.get(open + 1..)?.to_owned();

    // Field 3 onwards, in order: state, ppid, then eighteen fields nothing here
    // reads before the start time at field 22.
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;

    Some(Stat { comm, state, ppid, started })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line, from the healthy root helper measured on 2026-08-25.
    /// Trimmed after field 22; nothing beyond it is read.
    const HELPER: &str = "1208388 (adguard_root_he) S 1208374 1208374 0 0 -1 4194560 \
                          1155 0 0 0 41 96 0 0 20 0 5 0 11974417";

    /// The same helper an hour later, in the state this crate reports on.
    const DEFUNCT: &str = "13666 (adguard_root_he) Z 13482 13482 0 0 -1 4227136 \
                           0 0 0 0 0 0 0 0 20 0 1 0 1077829";

    #[test]
    fn reads_the_healthy_helper() {
        let stat = parse(HELPER).expect("should parse");
        assert_eq!(stat.comm, "adguard_root_he");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.ppid, 1208374);
        assert_eq!(stat.started, 11974417);
    }

    #[test]
    fn reads_the_defunct_helper() {
        let stat = parse(DEFUNCT).expect("should parse");
        assert_eq!(stat.state, ZOMBIE);
        assert_eq!(stat.ppid, 13482);
        // The name is identical in both states, which is exactly why the state
        // and not the name is what any verdict turns on.
        assert_eq!(stat.comm, "adguard_root_he");
    }

    /// The reason this is not a `split_whitespace` from the left.
    ///
    /// A process may be named anything, and both the space and the `)` here
    /// would move every field after it if the name were not cut out first.
    #[test]
    fn survives_a_name_holding_spaces_and_a_bracket() {
        let stat = parse("42 (we (are) legion) R 7 7 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 99")
            .expect("should parse");
        assert_eq!(stat.comm, "we (are) legion");
        assert_eq!(stat.state, 'R');
        assert_eq!(stat.ppid, 7);
        assert_eq!(stat.started, 99);
    }

    /// Truncation is the kernel's, not ours, and it is load-bearing: the helper
    /// is `adguard_root_helper` on disk and never reads back that way.
    #[test]
    fn the_name_is_never_longer_than_the_kernel_keeps() {
        assert_eq!(parse(HELPER).expect("should parse").comm.len(), 15);
    }

    /// The reader must answer for a real, live process — this one — and the
    /// answer must be stable across reads. Guards the field-22 arithmetic,
    /// which is silent when wrong: an off-by-one lands on a neighbouring
    /// counter that is also a plausible-looking integer.
    #[test]
    fn reads_a_start_time_that_does_not_change() {
        let me = std::process::id() as i32;
        let mine = stat(me).expect("this process has a stat");
        assert_ne!(mine.state, ZOMBIE, "the test process is running");
        assert_eq!(stat(me).map(|again| again.started), Some(mine.started));
        assert!(mine.started > 0, "a start time of 0 means the wrong field");
    }

    /// And the parent it names must be the one the kernel agrees on, which is
    /// the field `helper` matches its whole verdict against.
    #[test]
    fn reads_the_parent_this_process_actually_has() {
        let me = std::process::id() as i32;
        let mine = stat(me).expect("stat");
        assert_eq!(mine.ppid, std::os::unix::process::parent_id() as i32);
    }

    #[test]
    fn no_stat_for_a_pid_that_cannot_exist() {
        assert_eq!(stat(-1), None);
    }

    /// Boot time and the tick rate, which together are the only way a start
    /// time in ticks becomes a moment a log can be compared against.
    ///
    /// Stated against this machine rather than against a fixture: both come
    /// from the kernel, and a wrong answer for either would silently mis-scope
    /// every access-log reading by hours.
    #[test]
    fn the_clock_this_machine_actually_keeps() {
        let hz = ticks_per_second();
        assert!(hz > 0, "sysconf reported {hz} ticks per second");

        let boot = boot_time().expect("/proc/stat carries btime");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past 1970")
            .as_secs() as i64;
        assert!(boot > 0 && boot <= now, "booted at {boot}, now {now}");

        // This process started after the machine did, and not after now. A
        // wide bound on purpose — it is the field-22 arithmetic being checked,
        // not the scheduler.
        let me = stat(std::process::id() as i32).expect("this process has a stat");
        let started = boot + me.started as i64 / hz;
        assert!(
            (boot..=now + 1).contains(&started),
            "this process started at {started}, outside boot {boot}..={now}",
        );
    }

    #[test]
    fn rejects_a_line_that_is_not_one() {
        for line in ["", "no parentheses here", "12 (unterminated S 1 1", "12 (x)"] {
            assert!(parse(line).is_none(), "{line:?} should not parse");
        }
    }
}
