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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    /// A file owned by this user, executable, no suid — the shape AdGuard's
    /// helper actually ships in, reproduced without naming it.
    #[test]
    fn a_user_owned_executable_is_missing_the_two_that_matter() {
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
