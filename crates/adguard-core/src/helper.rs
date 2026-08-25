//! Whether AdGuard's own root helper has been set up.
//!
//! The helper needs root and **none of that root is ours** (`architecture.md`
//! §6). AdGuard ships it, names the fix itself, and gates on three properties
//! of it. Measured from the binary's strings (contract §8):
//!
//! ```text
//! Root helper check: owned_by_root={}, has_suid={}, is_executable={}
//! Automatic mode requires root helper to have suid bit set
//! Please run `sudo {} -s` to set it
//! ```
//!
//! So this module does exactly what `adguard-cli` does — `stat` the helper for
//! those three properties — and reports **the check**, not a verdict. Three
//! separate facts, so a helper that is root-owned but not suid says so rather
//! than collapsing into "not set up".
//!
//! **Those strings name automatic mode, and they undersell what the check is
//! worth.** An earlier revision of this module took them at their word and said
//! auto mode was the one thing here that needs root. It is not. With the helper
//! in its shipped state, `manual` mode — the default, and the mode every
//! endpoint on the Status page advertises — answers **every** request through
//! its HTTP proxy with 502 and never opens an upstream connection at all, while
//! the SOCKS5 proxy beside it works normally. Contract §8 has the measurement
//! and the before-and-after. Nothing in the CLI's own output connects the two:
//! the daemon logs `prepareFd: Failed to protect socket` and the user sees a
//! browser that cannot load anything.
//!
//! That is why the check is not filed under auto mode anywhere it is shown.
//!
//! **The check is advisory, and the contract measurement says why.** `config
//! set proxy_mode auto` succeeds with every one of the three unmet: exit 0,
//! `Config has been updated`, and `proxy.yaml` really holds `auto` afterwards.
//! The CLI does not consult the helper at write time. That makes this check
//! load-bearing — nothing else would stop a user selecting a mode that cannot
//! work — and it means the unmet state has to be *renderable* as well as
//! preventable, because a terminal or a text editor can reach it either way.
//!
//! Nothing here escalates, spawns anything, or writes. It reads file metadata.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The three properties `adguard-cli` checks, and the path they were read from.
///
/// Deliberately not a bool. A user who has run `sudo … -s` and still cannot
/// switch modes needs to know *which* property is missing, and "not set up" is
/// the one answer that cannot tell them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootHelper {
    /// The path inspected, as given. Kept so the setup command can name it and
    /// so a report can say what it looked at.
    pub path: PathBuf,
    pub owned_by_root: bool,
    pub has_suid: bool,
    pub is_executable: bool,
}

impl RootHelper {
    /// Read the three properties of the file at `path`.
    ///
    /// **The path is a parameter on purpose, and which branch that buys has
    /// changed.** It was written when the helper here was shipped `-rwxr-xr-x
    /// potworny potworny`: the unmet branch was the real state and rendered for
    /// free, and the met branch was only reachable by pointing the check
    /// somewhere else. This machine has since run AdGuard's own `sudo … -s`, so
    /// the helper is `-rwsr-xr-x root root` and the two have swapped — the
    /// unmet rendering is now the one nothing local reaches.
    ///
    /// The parameter is what makes that a non-event: `$ADGUARD_ROOT_HELPER`
    /// points the check at any file, and the tests below cover both ends
    /// against binaries the system already ships. A constant buried in the
    /// function would have left half the feature unprovable on whichever side
    /// of that line the machine happened to sit — and the only way back would
    /// be setting a suid bit on something, which is exactly the act this design
    /// exists to avoid.
    ///
    /// Symlinks are followed. `fs::metadata` does; `fs::symlink_metadata` would
    /// report the *link's* `lrwxrwxrwx` and its uid, so a helper reached through
    /// one would read as not-root-owned whatever it pointed at — and the
    /// `adguard-cli` entry on `$PATH` is itself a symlink here (contract §8).
    pub fn inspect(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let meta = fs::metadata(path)?;
        let (owned_by_root, has_suid, is_executable) =
            properties(meta.uid(), meta.permissions().mode());
        Ok(Self {
            path: path.to_path_buf(),
            owned_by_root,
            has_suid,
            is_executable,
        })
    }

    /// Read AdGuard's own helper, wherever this machine's CLI is installed.
    ///
    /// `None` when the CLI cannot be located at all; `Some(Err(_))` when it can
    /// but the helper beside it cannot be read. The two are different facts —
    /// "AdGuard is not installed" and "AdGuard is installed and something is
    /// wrong with its helper" — and the second is the one worth showing a user.
    pub fn detect() -> Option<io::Result<Self>> {
        crate::paths::root_helper().map(Self::inspect)
    }

    /// All three, which is what `adguard-cli` gates automatic mode on — and,
    /// measured, what its HTTP proxy needs before it will connect to anything
    /// in any mode (contract §8).
    pub fn is_set_up(&self) -> bool {
        self.owned_by_root && self.has_suid && self.is_executable
    }

    /// AdGuard's own setup command, with this machine's path in it.
    ///
    /// Shown for the user to run themselves, never executed here. The helper
    /// lives in a user-writable directory, so suid-root on it makes anyone who
    /// can write that file root — AdGuard's design, accepted by installing
    /// AdGuard, and deliberateness at a prompt is the only safeguard the
    /// arrangement has (`architecture.md` §6).
    pub fn setup_command(&self) -> String {
        format!("sudo {} -s", self.path.display())
    }

    /// The properties that are missing, in the order the CLI names them.
    /// Empty when [`Self::is_set_up`].
    ///
    /// **Noun phrases, so any subset of them reads as a sentence.** They were
    /// participles once — "owned by root", "the setuid bit set" — which suited
    /// the one caller there was then ("Automatic mode needs it owned by root")
    /// and fell apart on the subset that caller was least likely to see:
    /// a root-owned helper missing only the suid bit rendered "needs it the
    /// setuid bit set". Callers now say "missing …", which holds for all seven
    /// combinations.
    pub fn unmet(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.owned_by_root {
            missing.push("root ownership");
        }
        if !self.has_suid {
            missing.push("the setuid bit");
        }
        if !self.is_executable {
            missing.push("the executable bit");
        }
        missing
    }
}

/// The three properties, from a uid and a mode.
///
/// Split out from [`RootHelper::inspect`] so all eight combinations can be
/// covered without a filesystem — and specifically so "has the suid bit but is
/// not root-owned" is testable at all. That one cell cannot be built as a file
/// here: it would mean setting a suid bit on something, which this project does
/// not do even on a throwaway file.
///
/// `is_executable` is the owner-execute bit, matching what a `-rwxr-xr-x` /
/// `-rw-r--r--` reading tells a user. Root runs the helper, so the group and
/// other bits are not the interesting ones.
fn properties(uid: u32, mode: u32) -> (bool, bool, bool) {
    let owned_by_root = uid == 0;
    let has_suid = mode & 0o4000 != 0;
    let is_executable = mode & 0o100 != 0;
    (owned_by_root, has_suid, is_executable)
}

/// AdGuard's root helper, beside the **resolved** `adguard-cli` binary.
///
/// See [`crate::paths::root_helper`]; this is re-exported here because the
/// helper is this module's subject and `paths` is only where it is found.
pub fn path() -> Option<PathBuf> {
    crate::paths::root_helper()
}

// ---- the helper as a running process ----

/// The root helper's name as `/proc` spells it.
///
/// `adguard_root_helper` is nineteen characters and the kernel keeps fifteen,
/// so this truncated form is what appears in `/proc/<pid>/stat` and in `ps`
/// alike. Measured on v1.4.13 in **both** states — a working helper and a dead
/// one read identically here, which is why [`process`] decides on the state and
/// never on the name.
///
/// If AdGuard ever renames it, nothing matches, [`HelperProcess::Unseen`] comes
/// back, and this application says nothing. That is the direction this is meant
/// to fail in; see [`process`].
const HELPER_COMM: &str = "adguard_root_he";

/// What `/proc` says about the root helper running under one proxy daemon.
///
/// [`RootHelper`] above asks whether the helper was ever **set up**. This asks
/// whether it is **still running**, and the two are not the same event: a
/// helper can be perfectly installed, start with the daemon, and die hours
/// later. Measured on 2026-08-25, that is exactly what happens.
///
/// # The state this exists for
///
/// `adguard-cli` reports a proxy that is running and filtering while its helper
/// is a corpse and nothing is being filtered at all:
///
/// ```text
/// $ ps -eo pid,ppid,user,stat,cmd
///   13482    8245 potworny  Sl  …/adguard-cli start --no-fork --log-to-file
///   13666   13482 root      Z   [adguard_root_he] <defunct>
///
/// $ adguard-cli status
/// The AdGuard proxy server is running
/// System-wide automatic filtering is enabled
/// ```
///
/// The daemon's log names the moment it happened and then repeats the
/// consequence for as long as the daemon lives:
///
/// ```text
/// ERROR RootHelperClient on_packets_received: Failed to parse response
/// INFO  RootHelperClient disconnect: Finished
/// ERROR RootHelperClient send_command: Sequencer is not initialized
/// WARN  AGStandaloneServerSocketFactory prepareFd: Failed to protect socket
/// ```
///
/// `prepareFd: Failed to protect socket` is the same line this module's header
/// records for a helper that was never set up — the same consequence reached by
/// a different route, which is why both live here.
///
/// Nothing recovers on its own. A `restart` does, every time.
///
/// # This is upstream's bug, and upstream's own diagnostic
///
/// [`AdguardTeam/AdGuardCLI#136`] is the same failure on v1.4.11, closed
/// *Resolution: Done* on 2026-08-01 with a fix bound for the nightly channel.
/// Asked to narrow it down, AdGuard's engineer requested exactly one thing from
/// the reporter: the "presence/absence of running `adguard_root_helper`
/// process". This is that check. It is worth carrying until the fix reaches the
/// release channel, which v1.4.13 — published three months earlier — predates.
///
/// [`AdguardTeam/AdGuardCLI#136`]: https://github.com/AdguardTeam/AdGuardCLI/issues/136
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperProcess {
    /// A live helper, parented to the daemon asked about.
    Running,
    /// The helper exited and has not been reaped. **The only value here that is
    /// evidence of anything**, and the shape every measured failure took.
    Defunct,
    /// No helper process is parented to that daemon at all.
    ///
    /// Deliberately not merged with [`Self::Defunct`]. An absence has too many
    /// innocent readings — a daemon that has not spawned it yet, a `/proc` that
    /// could not be walked, a future AdGuard that parents or names it
    /// differently — and a caller must not turn any of them into a claim that
    /// protection has stopped.
    Unseen,
}

/// Find the root helper belonging to `daemon`, by pid.
///
/// # Why the answer is a state and not a `bool`
///
/// A false alarm here is worse than a missed detection. Telling a user their
/// protection has stopped when it has not teaches them to disregard the one
/// indicator that will eventually be telling the truth, and this check runs
/// against a process tree AdGuard is free to rearrange in any release. So the
/// verdict a caller may act on is [`HelperProcess::Defunct`] — a corpse we can
/// positively see — and never the absence of a helper, which is
/// [`HelperProcess::Unseen`] and means only that nothing is known.
///
/// That the corpse is reliably there is a property of the bug rather than
/// luck: the daemon never reaps it, which is why it is a zombie and not simply
/// gone.
///
/// # Why the name, and not the executable
///
/// `/proc/<pid>/exe` is the identification [`crate::orphan`] uses and it cannot
/// be used here. The helper runs as **root**, so reading its link from this
/// application's uid fails with `EACCES`, and a zombie has no such link at all
/// — the two cases this function most needs to tell apart are precisely the two
/// that route would refuse to read. Field 2 of `/proc/<pid>/stat` is readable
/// for any process in either state, and pairing it with the parent's pid is
/// what makes the match *this* daemon's helper rather than any process of that
/// name.
///
/// # Cost
///
/// One `read_dir` of `/proc` and one small file per entry, no network and no
/// privilege — the same walk [`crate::orphan::daemons`] already performs, and
/// cheap enough for the Status page's two-second poll.
pub fn process(daemon: i32) -> HelperProcess {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HelperProcess::Unseen;
    };

    let mut found = HelperProcess::Unseen;
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse().ok()) else {
            continue;
        };
        let Some(stat) = crate::proc::stat(pid) else {
            continue;
        };
        if stat.ppid != daemon || stat.comm != HELPER_COMM {
            continue;
        }
        // One live helper settles it, whatever else is lying around. The daemon
        // reaps nothing it spawns, so a helper that died and was replaced
        // within a single run leaves its corpse beside the working one, and
        // reading that corpse as the verdict would report a healthy install
        // broken.
        if stat.state != crate::proc::ZOMBIE {
            return HelperProcess::Running;
        }
        found = HelperProcess::Defunct;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use std::time::{Duration, Instant};

    /// How long a spawned stand-in is given to become the program it was asked
    /// to be, or to finish dying. Measured in microseconds either way; this is
    /// the point past which the machine has a different problem.
    const WITHIN: Duration = Duration::from_secs(5);

    /// Poll `/proc` until this process matches, or give up.
    fn settle(pid: i32, matches: impl Fn(&crate::proc::Stat) -> bool) -> bool {
        let deadline = Instant::now() + WITHIN;
        while Instant::now() < deadline {
            if crate::proc::stat(pid).as_ref().is_some_and(&matches) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Both verdicts, against a real process, found by the same walk the
    /// application uses.
    ///
    /// A copy of `/bin/sh` named `adguard_root_he` stands in for the helper:
    /// what [`process`] matches on is field 2 of `/proc/<pid>/stat` plus the
    /// parent's pid, and a renamed shell supplies exactly that shape without
    /// needing root or anything AdGuard owns. The name is the truncated
    /// spelling deliberately — a file called `adguard_root_helper` would read
    /// back cut to fifteen characters anyway, and writing it out in full here
    /// would hide the very truncation [`HELPER_COMM`] exists for.
    ///
    /// The script is a **compound** command for the reason `orphan`'s
    /// equivalent documents: given a single simple one a shell execs it in
    /// place, taking the name we went to the trouble of choosing with it.
    #[test]
    fn finds_a_live_helper_and_then_its_corpse() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-helper-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let stand_in = dir.join(HELPER_COMM);
        fs::copy("/bin/sh", &stand_in).expect("copy /bin/sh");

        let mut child = std::process::Command::new(&stand_in)
            .args(["-c", "while :; do sleep 1; done"])
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        let us = std::process::id() as i32;

        assert!(
            settle(pid, |stat| stat.comm == HELPER_COMM),
            "pid {pid} never became {HELPER_COMM}",
        );
        assert_eq!(process(us), HelperProcess::Running);

        // SAFETY: a positive pid signals exactly that process. It is this
        // process's own child, spawned above and not yet reaped, so the pid
        // cannot have been reused by anything else.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Deliberately not reaped yet: a zombie is what the bug leaves behind,
        // and it is the state this whole check turns on.
        assert!(
            settle(pid, |stat| stat.state == crate::proc::ZOMBIE),
            "pid {pid} never became a zombie",
        );
        assert_eq!(process(us), HelperProcess::Defunct);

        child.wait().expect("reap");
        // And once reaped there is nothing to see, which must not read as the
        // corpse it no longer is.
        assert_eq!(process(us), HelperProcess::Unseen);

        fs::remove_dir_all(&dir).ok();
    }

    /// What the walk costs, because it runs beside every `status` on the Status
    /// page's two-second poll.
    ///
    /// An upper bound with a wide margin, as every timing assertion in this
    /// project is: a loaded machine must not fail it, and the number it guards
    /// against is far larger than the measurement. Run it with `--nocapture` to
    /// see the real figure.
    ///
    /// Pid 1 rather than a real daemon, deliberately — the cost is the walk of
    /// `/proc` and one small read per entry, which is paid in full whatever the
    /// verdict turns out to be.
    #[test]
    fn the_walk_is_cheap_enough_for_the_poll() {
        // Ten, so a single unlucky scheduling decision cannot decide it.
        let started = Instant::now();
        for _ in 0..10 {
            let _ = process(1);
        }
        let each = started.elapsed() / 10;
        eprintln!("helper::process: {each:?} per call");
        assert!(each < Duration::from_millis(100), "{each:?}");
    }

    /// A daemon with no helper under it is never reported as broken.
    ///
    /// pid 1 has plenty of children on any running system and none of them is
    /// AdGuard's helper, so this is the false-alarm guard stated against a
    /// process tree that certainly exists.
    #[test]
    fn an_unrelated_parent_yields_no_verdict() {
        assert_eq!(process(1), HelperProcess::Unseen);
    }

    /// All eight combinations, without touching a filesystem. The suid-without-
    /// root cell exists only here, for the reason `properties` documents.
    #[test]
    fn every_combination_of_the_three_properties() {
        let cases = [
            //  uid, mode,    root,  suid,  exec
            (0u32, 0o4755u32, true, true, true),
            (0, 0o755, true, false, true),
            (0, 0o4644, true, true, false),
            (0, 0o644, true, false, false),
            (1000, 0o4755, false, true, true),
            (1000, 0o755, false, false, true),
            (1000, 0o4644, false, true, false),
            (1000, 0o644, false, false, false),
        ];
        for (uid, mode, root, suid, exec) in cases {
            assert_eq!(
                properties(uid, mode),
                (root, suid, exec),
                "uid {uid}, mode {mode:o}"
            );
        }
    }

    /// The met branch, against a file this machine already ships setuid-root.
    /// Nothing is chmod-ed to produce it — `/usr/bin/passwd` is `-rwsr-xr-x
    /// root root` as installed, which is precisely the shape `adguard-cli`
    /// wants its helper to have.
    #[test]
    fn a_setuid_root_binary_reads_as_set_up() {
        let path = PathBuf::from("/usr/bin/passwd");
        if !path.exists() {
            eprintln!("skipping: no /usr/bin/passwd on this machine");
            return;
        }
        let helper = RootHelper::inspect(&path).expect("passwd is readable");
        assert!(helper.owned_by_root, "{helper:?}");
        assert!(helper.has_suid, "{helper:?}");
        assert!(helper.is_executable, "{helper:?}");
        assert!(helper.is_set_up());
        assert!(helper.unmet().is_empty());
    }

    /// Root-owned and executable but *not* suid — the case that must not be
    /// flattened into "not set up" without saying which property is missing.
    #[test]
    fn root_owned_without_the_suid_bit_names_only_that() {
        let path = PathBuf::from("/bin/ls");
        if !path.exists() {
            eprintln!("skipping: no /bin/ls on this machine");
            return;
        }
        let helper = RootHelper::inspect(&path).expect("ls is readable");
        assert!(helper.owned_by_root, "{helper:?}");
        assert!(!helper.has_suid, "{helper:?}");
        assert!(helper.is_executable, "{helper:?}");
        assert!(!helper.is_set_up());
        assert_eq!(helper.unmet(), vec!["the setuid bit"]);
    }

    /// Root-owned, no suid, not executable.
    #[test]
    fn a_plain_root_owned_file_is_missing_two_of_the_three() {
        let path = PathBuf::from("/etc/hostname");
        if !path.exists() {
            eprintln!("skipping: no /etc/hostname on this machine");
            return;
        }
        let helper = RootHelper::inspect(&path).expect("hostname is readable");
        assert_eq!(
            (
                helper.owned_by_root,
                helper.has_suid,
                helper.is_executable
            ),
            (true, false, false),
            "{helper:?}"
        );
        assert_eq!(
            helper.unmet(),
            vec!["the setuid bit", "the executable bit"]
        );
    }

    /// The two tests below write a file and assert it is **not** root-owned,
    /// which is false when the suite itself runs as root: the file is then
    /// owned by root because the process that created it is. That is not a
    /// hypothetical — a container runs as root by default, and CI is a
    /// container (`.github/workflows/ci.yml`), where these two were the only
    /// failures on an otherwise green run.
    ///
    /// Skipping is the same answer the cases above give when `/bin/ls` or
    /// `/etc/hostname` is absent: the property under test cannot be reproduced
    /// here, and an assertion made anyway would be describing the runner
    /// rather than `inspect`. The met branch is unaffected — it reads
    /// `/usr/bin/passwd`, whose ownership belongs to nobody's test process.
    fn running_as_root() -> bool {
        // SAFETY: `geteuid` reads a process attribute, takes no arguments and
        // cannot fail. It is `unsafe` only because it is an extern fn.
        unsafe { libc::geteuid() == 0 }
    }

    /// A file owned by this user, executable, no suid — the shape AdGuard's
    /// helper actually ships in, reproduced without naming it.
    #[test]
    fn a_user_owned_executable_is_missing_the_two_that_matter() {
        if running_as_root() {
            eprintln!("skipping: running as root, so a file this test writes is root-owned");
            return;
        }
        let dir = std::env::temp_dir().join("adguard-ui-helper-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("stand-in");
        fs::write(&path, b"#!/bin/sh\n").expect("write the stand-in");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod 755");

        let helper = RootHelper::inspect(&path).expect("the stand-in is readable");
        assert_eq!(
            (
                helper.owned_by_root,
                helper.has_suid,
                helper.is_executable
            ),
            (false, false, true),
            "{helper:?}"
        );
        assert!(!helper.is_set_up());
        assert_eq!(helper.unmet(), vec!["root ownership", "the setuid bit"]);

        let _ = fs::remove_file(&path);
    }

    /// Not executable either — every property missing.
    #[test]
    fn a_plain_user_owned_file_is_missing_all_three() {
        if running_as_root() {
            eprintln!("skipping: running as root, so a file this test writes is root-owned");
            return;
        }
        let dir = std::env::temp_dir().join("adguard-ui-helper-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("not-a-program");
        fs::write(&path, b"text\n").expect("write the file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod 644");

        let helper = RootHelper::inspect(&path).expect("the file is readable");
        assert!(!helper.is_set_up());
        assert_eq!(
            helper.unmet(),
            vec!["root ownership", "the setuid bit", "the executable bit"]
        );

        let _ = fs::remove_file(&path);
    }

    /// An absent helper is an error, not a `RootHelper` reading false three
    /// times. "It is not set up" and "it is not there" call for different
    /// wording, exactly as this app distinguishes off from unknown everywhere.
    #[test]
    fn a_missing_path_is_an_error_not_three_falses() {
        let path = std::env::temp_dir().join("adguard-ui-helper-test/definitely-absent");
        let _ = fs::remove_file(&path);
        assert!(RootHelper::inspect(&path).is_err());
    }

    /// The command is AdGuard's own, with the inspected path in it — and it is
    /// only ever a string. Nothing in this crate runs it.
    #[test]
    fn the_setup_command_is_adguards_own_wording() {
        let helper = RootHelper {
            path: PathBuf::from("/home/someone/.local/opt/adguard-cli/adguard_root_helper"),
            owned_by_root: false,
            has_suid: false,
            is_executable: true,
        };
        assert_eq!(
            helper.setup_command(),
            "sudo /home/someone/.local/opt/adguard-cli/adguard_root_helper -s"
        );
    }

    /// The reading this machine actually gives, so an `adguard-cli` upgrade
    /// that ships the helper differently is noticed here rather than in the UI.
    /// Skips when AdGuard is not installed, like the other `_live` checks.
    #[test]
    fn the_real_helper_reads_as_shipped() {
        let Some(result) = RootHelper::detect() else {
            eprintln!("skipping: adguard-cli is not installed");
            return;
        };
        let helper = result.expect("the helper is beside the resolved binary");
        assert!(
            helper.path.ends_with("adguard_root_helper"),
            "{:?}",
            helper.path
        );
        assert!(
            helper.is_executable,
            "the helper ships executable: {helper:?}"
        );
    }
}
