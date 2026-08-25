//! Finding a proxy process the CLI itself has lost track of.
//!
//! # The state this exists for, measured on 2026-08-01
//!
//! An install can reach a state where `adguard-cli` reports the proxy stopped
//! while the previous proxy process is still alive and still holding the ports.
//! Captured whole, on v1.4.13:
//!
//! ```text
//! $ ps -eo pid,ppid,etime,stat,cmd
//!    6925    2968  01:11:56 Sl  …/adguard-cli start --no-fork --log-to-file
//!    6932    6925  01:11:56 Z   [adguard_root_he] <defunct>
//!
//! $ ss -lntp
//! LISTEN 127.0.0.1:3129  users:(("adguard-cli",pid=6925,fd=62))
//! LISTEN 127.0.0.1:1081  users:(("adguard-cli",pid=6925,fd=63))
//!
//! $ adguard-cli status                                    # 0.2 s, exit 0
//! The AdGuard proxy server is not running
//! ```
//!
//! The daemon has been reparented to `systemd --user`, it never reaped its root
//! helper, and it no longer answers on `agcli.socket` — so `status`, which asks
//! over that socket, reports it gone while the kernel says otherwise.
//!
//! # Neither of the CLI's own commands gets out of it
//!
//! `stop` is a no-op — it returns in 0.1 s at **exit 0** and the process is
//! still there afterwards:
//!
//! ```text
//! Failed to stop the AdGuard proxy server
//! Failed to stop proxy server, it is not running
//! ```
//!
//! `start` cannot bind what is already bound, and takes **60.0 s** to say so —
//! its own internal deadline, `CSM response_from_listener: Client wait data from
//! listener timeout` in `logs/app.log` — before printing, again at **exit 0**:
//!
//! ```text
//! Failed to start proxy server: An unknown error has occurred
//! ```
//!
//! For scale, a start against a healthy install takes **1.1 s**. So the CLI has
//! no route out of this and the user is left with a proxy that is down, a UI
//! that agrees it is down, and a Start button that does nothing for a minute.
//!
//! A `SIGTERM` to that one pid ends it in under **0.5 s** and both ports come
//! back; `SIGKILL` was never needed. That is the whole of the cure, and it needs
//! no privilege — the process belongs to the user running this application,
//! which is why it is done here rather than shown as a command in the way §6 of
//! `architecture.md` requires for anything wanting `sudo`.
//!
//! # What identifies the leftover — and what does not
//!
//! **Not the command line.** A perfectly healthy daemon is also
//! `adguard-cli start --no-fork --log-to-file`; that was measured immediately
//! after recovery, on the working process that replaced this one. Killing on
//! that alone would kill a running proxy.
//!
//! What identifies it is the **contradiction**: such a process exists, and
//! `status` says nothing is running. Those cannot both be true of a healthy
//! install, and this module supplies one half — the process — while the caller
//! supplies the other. See [`Daemon::alive`] for the second guard, which is what
//! keeps a start from killing the very daemon it just forked.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proc;

/// How long a signalled daemon is given to go away before we report that it did
/// not. Measured at under 0.5 s; this is the point past which something else is
/// wrong and saying so beats waiting.
const REAPED_WITHIN: Duration = Duration::from_secs(5);

/// How often `/proc` is re-read while waiting for that.
const POLL: Duration = Duration::from_millis(50);

/// The argument pair that marks the process holding the ports.
///
/// `start` alone is not enough: the invocation the user (or this application)
/// runs is itself `adguard-cli start`, and it lives for about a second while it
/// forks the real daemon. Requiring `--no-fork` names the child — the one that
/// stays, and the one that binds — rather than the parent that is about to exit.
///
/// If AdGuard ever stops passing it, nothing here matches and no recovery is
/// attempted. That is the right way for this to fail.
const DAEMON_ARGS: [&str; 2] = ["start", "--no-fork"];

/// One live `adguard-cli` proxy process.
///
/// Carries its start time as well as its pid, because the two reads that matter
/// — finding it, and signalling it — are separated by a `start` that can take a
/// minute, and a pid is only unique among *live* processes. See
/// [`Self::alive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pid: i32,
    /// Field 22 of `/proc/<pid>/stat`: the process's start time, in clock ticks
    /// since boot. Constant for the life of a process and effectively unique
    /// per pid, so it is what tells this process from a later one that happened
    /// to be given the same number.
    started: u64,
}

impl Daemon {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Is this same process still running?
    ///
    /// Three ways to be gone, and all of them have to count:
    ///
    /// - the pid no longer exists;
    /// - it exists but belongs to a *later* process that was given the same
    ///   number, which the start time is what detects;
    /// - it exists, is this very process, and has already exited — a **zombie**,
    ///   waiting for a parent that has not reaped it.
    ///
    /// The last is not a curiosity here. It is what a process becomes the
    /// instant it dies, it keeps a `/proc/<pid>/stat` with an unchanged start
    /// time, and the wedged install this module was written for had one sitting
    /// in it (`[adguard_root_he] <defunct>`). Counting a zombie as alive would
    /// make [`Self::terminate`] wait out its whole deadline and then report
    /// failure over a process that had done exactly what was asked.
    pub fn alive(&self) -> bool {
        matches!(proc::stat(self.pid), Some(stat)
            if stat.state != proc::ZOMBIE && stat.started == self.started)
    }

    /// Ask this process to exit, and wait to see whether it did.
    ///
    /// `SIGTERM` only. Measured, it is sufficient — the leftover went in under
    /// 0.5 s and released both ports — and it is what `killall adguard-cli`
    /// sends, which is the recovery this reproduces without `killall`'s aim:
    /// that command would take a *healthy* proxy with it, and every other
    /// `adguard-cli` the user happened to be running.
    ///
    /// Returns whether the process is gone. A signal that fails because the
    /// process has already exited counts as success — that is the outcome
    /// asked for, and it is a race we are bound to lose sometimes.
    pub fn terminate(&self) -> bool {
        // Already gone, or the pid now belongs to something else. Signalling
        // here would be the one genuinely dangerous thing this module could do.
        if !self.alive() {
            return true;
        }

        // SAFETY: `kill(2)` with a positive pid signals exactly that process and
        // touches nothing in ours. The pid was read from `/proc` and re-checked
        // above; the worst outcome of losing that race is `ESRCH`, which the
        // wait below turns into the success it is.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }

        let deadline = Instant::now() + REAPED_WITHIN;
        while Instant::now() < deadline {
            if !self.alive() {
                return true;
            }
            std::thread::sleep(POLL);
        }
        !self.alive()
    }

    /// Read one `/proc` entry, keeping it only if it is this CLI's daemon.
    ///
    /// `binary` must already be canonicalised: `/proc/<pid>/exe` is a link to
    /// the real file, so comparing it against `~/.local/bin/adguard-cli` — the
    /// symlink `$PATH` finds on the reference machine — would never match.
    ///
    /// Reading that link needs the same uid (or `CAP_SYS_PTRACE`), so another
    /// user's processes fail here with `EACCES` and drop out. That is the
    /// filter we want rather than a limitation: this application may only
    /// signal its own user's processes, and the kernel enforces it for us.
    fn read(pid: i32, binary: &Path) -> Option<Self> {
        if fs::read_link(format!("/proc/{pid}/exe")).ok()? != binary {
            return None;
        }
        if !is_daemon(&fs::read(format!("/proc/{pid}/cmdline")).ok()?) {
            return None;
        }
        let stat = proc::stat(pid)?;
        // A daemon that has already exited is not holding anything, and is not
        // something to go looking for a reason to signal.
        (stat.state != proc::ZOMBIE).then_some(Self { pid, started: stat.started })
    }
}

/// Every live proxy daemon belonging to this CLI install.
///
/// Says nothing about whether any of them is wedged — a healthy install has
/// exactly one and it looks identical. The caller pairs this with a `status`
/// reading; see the module header.
///
/// An unreadable `/proc`, or a binary path that no longer resolves, yields an
/// empty list: nothing found means nothing is done, which is the safe direction
/// for a function whose result is used to choose what to signal.
pub fn daemons(binary: &Path) -> Vec<Daemon> {
    let Ok(binary) = fs::canonicalize(binary) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            Daemon::read(pid, &binary)
        })
        .collect()
}

/// Does this `/proc/<pid>/cmdline` describe the daemon rather than the
/// short-lived invocation that spawns it?
///
/// The file is NUL-separated with a trailing NUL, so the arguments are exactly
/// its non-empty splits. Matched as whole arguments — a *substring* test would
/// find `start` inside a filter URL and a `--no-fork` inside nothing at all, but
/// the first is enough of a reason.
fn is_daemon(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).filter(|arg| !arg.is_empty()).collect();
    DAEMON_ARGS
        .iter()
        .all(|wanted| args.contains(&wanted.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real command line of the leftover daemon, as `/proc` holds it.
    const DAEMON: &[u8] = b"/home/u/.local/opt/adguard-cli/adguard-cli\0start\0--no-fork\0--log-to-file\0";

    /// What the user, or this application, actually runs. It forks the one
    /// above and exits — signalling it would achieve nothing.
    const INVOCATION: &[u8] = b"/home/u/.local/opt/adguard-cli/adguard-cli\0start\0";

    #[test]
    fn recognises_the_daemon() {
        assert!(is_daemon(DAEMON));
    }

    #[test]
    fn ignores_the_invocation_that_forks_it() {
        assert!(!is_daemon(INVOCATION));
    }

    /// Every other subcommand this application runs shares the same binary, so
    /// the argument test is the only thing keeping a `status` poll — which the
    /// Status page fires every second — out of the list of things to kill.
    #[test]
    fn ignores_the_other_subcommands() {
        for args in [
            &b"/opt/adguard-cli\0status\0"[..],
            &b"/opt/adguard-cli\0stop\0"[..],
            &b"/opt/adguard-cli\0license\0"[..],
            // The value is spelled out rather than written after a `\0`, where
            // `\03129` reads as an octal escape to everyone including clippy.
            &[b"/opt/adguard-cli\0config\0set\0--\0listen_ports.http_proxy\0".as_slice(), b"3129\0"]
                .concat()[..],
        ] {
            assert!(!is_daemon(args), "matched {:?}", String::from_utf8_lossy(args));
        }
    }

    /// A filter URL is an ordinary argument and can contain anything. It must
    /// not be read as the `start` that marks a daemon.
    #[test]
    fn a_url_containing_start_is_not_a_daemon() {
        let args = b"/opt/adguard-cli\0filters\0install\0--\0https://e.org/start--no-fork.txt\0";
        assert!(!is_daemon(args));
    }

    #[test]
    fn empty_cmdline_is_not_a_daemon() {
        assert!(!is_daemon(b""));
        assert!(!is_daemon(b"\0\0"));
    }

    /// This process is not an AdGuard daemon, whatever else is true of the
    /// machine the tests run on — so the scan must come back without it.
    #[test]
    fn does_not_find_itself() {
        let me = std::env::current_exe().expect("current exe");
        let found = daemons(&me);
        assert!(
            found.is_empty(),
            "the test binary was mistaken for a daemon: {found:?}"
        );
    }

    /// A daemon standing for a process that is gone must never be signalled,
    /// and `alive` is the check that decides it. Pid 0 is not a process this
    /// call can name, so it stands in for the vanished one.
    #[test]
    fn a_vanished_daemon_is_not_alive() {
        let ghost = Daemon { pid: 0, started: 1 };
        assert!(!ghost.alive());
        // And terminating it is a no-op that reports success rather than
        // signalling whatever pid 0 would mean to `kill`.
        assert!(ghost.terminate());
    }

    /// The same pid with a different start time is a *different* process. This
    /// is the pid-reuse guard, and getting it backwards would mean signalling
    /// an innocent process that inherited the number.
    #[test]
    fn a_recycled_pid_is_not_the_same_daemon() {
        let me = std::process::id() as i32;
        let real = proc::stat(me).expect("stat").started;
        assert!(Daemon { pid: me, started: real }.alive());
        assert!(!Daemon {
            pid: me,
            started: real.wrapping_add(1)
        }
        .alive());
    }

    /// The dangerous half, against a real process — found through `/proc` by
    /// the same scan the application uses, and actually signalled.
    ///
    /// `sh` stands in for `adguard-cli`: what the scan matches on is the
    /// resolved `/proc/<pid>/exe` plus two arguments, and `sh -c <script> start
    /// --no-fork` supplies exactly that shape while touching nothing AdGuard
    /// owns. Nothing else on the machine is `sh` carrying those two arguments,
    /// so the pid found is the one spawned here.
    ///
    /// The script is a **compound** command on purpose. Given a single simple
    /// one, a shell execs it in place rather than forking — `sh -c 'sleep 30'`
    /// becomes `sleep 30`, taking `/proc/<pid>/exe` and both arguments with it.
    /// Measured on the reference machine's dash; a loop is not exec-optimised,
    /// so the shell stays a shell for as long as it is needed.
    #[test]
    fn finds_and_terminates_a_real_process() {
        let shell = fs::canonicalize("/bin/sh").expect("/bin/sh");
        // The arguments after the script become $0 and $1 — a command line of
        // our choosing, on a binary we did not have to write.
        let mut child = std::process::Command::new(&shell)
            .args(["-c", "while :; do sleep 1; done", "start", "--no-fork"])
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        settled(pid);

        let found = daemons(&shell);
        let daemon = found.iter().find(|daemon| daemon.pid() == pid).unwrap_or_else(|| {
            panic!(
                "the scan missed pid {pid}: {found:?}\nexe {:?}\ncmdline {:?}",
                fs::read_link(format!("/proc/{pid}/exe")),
                fs::read(format!("/proc/{pid}/cmdline")).map(|c| String::from_utf8_lossy(&c).into_owned()),
            )
        });
        assert!(daemon.alive());

        assert!(daemon.terminate(), "SIGTERM did not end pid {pid}");
        // It is a zombie at this point rather than absent — this process is its
        // parent and has not reaped it yet. `alive` has to say gone anyway, or
        // `terminate` above would have waited out its whole deadline.
        assert!(!daemon.alive(), "a reaped-pending zombie must read as gone");
        assert!(!daemons(&shell).iter().any(|d| d.pid() == pid));

        child.wait().expect("reap");
    }

    /// How long a just-spawned child is given to finish becoming the program it
    /// was asked to be. Measured in microseconds; this is the point past which
    /// the machine has a different problem.
    const EXEC_WITHIN: Duration = Duration::from_secs(5);

    /// Wait for that.
    ///
    /// `spawn` does not promise it. It returns once `posix_spawn` releases the
    /// parent, and the kernel does that from *inside* the child's `execve` —
    /// early enough that `/proc/<pid>` can still describe the process being
    /// replaced. Both intermediate shapes were measured on 2026-08-03:
    ///
    /// - exe and command line still this test binary's own, state `R`, on the
    ///   CI runner — which is what made this test fail there;
    /// - exe already the shell with the command line not yet filled in, on
    ///   1935 of 2000 spawns on the reference machine.
    ///
    /// Neither shape carries the two arguments, so [`daemons`] is right to pass
    /// over it and the scan comes back empty. There is nothing here for the
    /// application to guard against: it reads that list at idle, or after a CLI
    /// invocation it waited on, never in the microsecond after a fork. The
    /// stopwatch that is too fast belongs to this test, so the wait does too.
    fn settled(pid: i32) {
        let mine = fs::read_link("/proc/self/exe").ok();
        let deadline = Instant::now() + EXEC_WITHIN;
        while Instant::now() < deadline {
            let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if exe != mine && cmdline.iter().any(|byte| *byte != 0) {
                return;
            }
            std::thread::sleep(POLL);
        }
        panic!("pid {pid} never finished exec'ing");
    }
}
