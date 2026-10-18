//! A copy of `adguard.conf` taken while the licence works, and put back when
//! AdGuard CLI has thrown its config away.
//!
//! # Why
//!
//! `adguard.conf` holds the licence, the device identity the licence is bound
//! to, and the CA's private key. AdGuard CLI rewrites it on every invocation,
//! truncate first and write after. When a reader meets it empty it logs
//! `Upgrading config from revision 0 to 1` and seeds a new one. From then on
//! the install is unlicensed, under a new device id. Activating again means
//! replacing a device on the account, and usually a new CA as well. Measured
//! on 1, 4, 9, 10, 11 and 18 October 2026, on the last date with the disk full
//! (`Failed to write settings file: No space left on device`) and the machine
//! freezing.
//!
//! Putting the old file back restores all three, without the user doing
//! anything. Contract §5 measured that the licence travels with the file.
//!
//! # When a copy is taken
//!
//! Only right after `license` said the licence is active, and only if the file
//! read the same, byte for byte, before and after that `license` ran. A copy
//! taken while something else was halfway through rewriting the file would be
//! worse than none. All of it runs inside the CLI slot (`slot.rs`), so none of
//! this application's own invocations can interleave with it.
//!
//! # When it is put back
//!
//! Only while `license` is refused for want of a licence, and only if the file
//! differs from the copy. Then `license` is asked again, and if it still
//! refuses, the file that was there is put back. So a copy can never make
//! things worse, and a licence lost on AdGuard's side, which the copy cannot
//! fix, is left exactly as it was found.
//!
//! The caller must not try while an activation is in progress: the file then
//! holds the device id the user is logging in for.
//!
//! # Where
//!
//! `$XDG_STATE_HOME/adguard-ui/adguard.conf.backup`, mode 0600 in a 0700
//! directory. It is the same secret as the original, readable by the same
//! user. For a `Cli` pointed at another data directory (the test sandboxes),
//! it sits beside that directory instead, so a sandbox can never overwrite the
//! real copy.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::{Cli, Error};

const CONF_FILE: &str = "adguard.conf";
const BACKUP_FILE: &str = "adguard.conf.backup";

/// How long to wait for the CLI slot before giving up on a backup or a
/// restore. Neither is urgent; both are retried on a later reading.
const SLOT_WAIT: Duration = Duration::from_secs(30);

/// What [`ConfBackup::restore`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    /// No copy, or the file already matches it. Nothing was touched.
    NothingToDo,
    /// The copy was put back and `license` now says the licence is active.
    Restored,
    /// The copy was put back, `license` still refused, so the file that was
    /// there before is back in place.
    DidNotHelp,
    /// Something could not be read or written, or the slot was not free.
    Failed(String),
}

/// The pair of files: AdGuard's own, and our copy of it.
#[derive(Debug, Clone)]
pub struct ConfBackup {
    conf: PathBuf,
    backup: PathBuf,
}

impl ConfBackup {
    /// The pair for `cli`'s data directory. `None` when there is no home to
    /// put either in.
    pub fn for_cli(cli: &Cli) -> Option<Self> {
        match cli.xdg_data_home() {
            Some(home) => Some(Self::at(
                crate::paths::data_dir_under(home).join(CONF_FILE),
                home.join(crate::window_state::SUBDIR).join(BACKUP_FILE),
            )),
            None => Some(Self::at(
                crate::paths::data_dir()?.join(CONF_FILE),
                crate::window_state::state_home()?
                    .join(crate::window_state::SUBDIR)
                    .join(BACKUP_FILE),
            )),
        }
    }

    /// The pair at explicit paths.
    pub fn at(conf: PathBuf, backup: PathBuf) -> Self {
        Self { conf, backup }
    }

    pub fn backup_path(&self) -> &Path {
        &self.backup
    }

    /// Whether the file differs from the copy, or there is no copy yet.
    ///
    /// Cheap, and no `adguard-cli` runs. It is what lets a caller try
    /// [`Self::keep`] on every successful reading and pay for the `license`
    /// it runs only when something actually changed.
    pub fn stale(&self) -> bool {
        match (fs::read(&self.conf), fs::read(&self.backup)) {
            (Ok(conf), Ok(backup)) => conf != backup,
            (Ok(conf), Err(_)) => !conf.is_empty(),
            (Err(_), _) => false,
        }
    }

    /// Copy the file if the licence is active and the file is stable.
    ///
    /// Returns whether a copy was written. `Ok(false)` covers every "not
    /// now": a licence that is not active, a file that changed under the
    /// reading, or one that already matches the copy.
    pub fn keep(&self, cli: &Cli) -> Result<bool, String> {
        cli.exclusively(SLOT_WAIT, || {
            let before = fs::read(&self.conf).map_err(|err| err.to_string())?;
            if before.is_empty() {
                return Ok(false);
            }
            match cli.license() {
                Ok(licence) if licence.is_active() => {}
                Ok(_) | Err(_) => return Ok(false),
            }
            let after = fs::read(&self.conf).map_err(|err| err.to_string())?;
            if after != before {
                return Ok(false);
            }
            if fs::read(&self.backup).is_ok_and(|copy| copy == after) {
                return Ok(false);
            }
            write_private(&self.backup, &after).map_err(|err| err.to_string())?;
            Ok(true)
        })
        .unwrap_or_else(|| Err("an earlier AdGuard CLI command is still running".to_owned()))
    }

    /// Put the copy back, if that brings the licence back. See the module
    /// docs for when this may be called.
    pub fn restore(&self, cli: &Cli) -> Restore {
        cli.exclusively(SLOT_WAIT, || self.restore_held(cli))
            .unwrap_or_else(|| {
                Restore::Failed("an earlier AdGuard CLI command is still running".to_owned())
            })
    }

    fn restore_held(&self, cli: &Cli) -> Restore {
        let Ok(copy) = fs::read(&self.backup) else {
            return Restore::NothingToDo;
        };
        if copy.is_empty() {
            return Restore::NothingToDo;
        }
        let current = match fs::read(&self.conf) {
            Ok(current) => current,
            Err(err) => return Restore::Failed(err.to_string()),
        };
        if current == copy {
            return Restore::NothingToDo;
        }
        // Only for a licence that is refused. One that answers, active or
        // not, is not a file that was thrown away.
        if !matches!(cli.license(), Err(Error::Unlicensed { .. })) {
            return Restore::NothingToDo;
        }

        let mode = fs::metadata(&self.conf).map(|meta| meta.permissions().mode() & 0o777).ok();
        if let Err(err) = replace(&self.conf, &copy, mode) {
            return Restore::Failed(err.to_string());
        }
        match cli.license() {
            Ok(licence) if licence.is_active() => Restore::Restored,
            _ => match replace(&self.conf, &current, mode) {
                Ok(()) => Restore::DidNotHelp,
                Err(err) => Restore::Failed(format!(
                    "the backup did not bring the licence back, and the file it replaced could \
                     not be put back: {err}"
                )),
            },
        }
    }
}

/// Write `bytes` to `path` readable by its owner only, through a temporary
/// file and a rename, so no reader ever sees half of it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    replace(path, bytes, Some(0o600))
}

/// Atomically replace `path` with `bytes`, keeping `mode` when given.
fn replace(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode.unwrap_or(0o600))
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Some(mode) = mode {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("adguard-ui-confbak-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_copy_is_private_and_whole() {
        let dir = scratch("private");
        let target = dir.join("state/adguard-ui").join(BACKUP_FILE);
        write_private(&target, b"secret").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"secret");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(target.parent().unwrap()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != BACKUP_FILE)
            .collect();
        assert!(leftovers.is_empty(), "a temporary file was left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_keeps_the_original_mode() {
        let dir = scratch("mode");
        let conf = dir.join(CONF_FILE);
        fs::write(&conf, b"old").unwrap();
        fs::set_permissions(&conf, fs::Permissions::from_mode(0o664)).unwrap();
        replace(&conf, b"new", Some(0o664)).unwrap();
        assert_eq!(fs::read(&conf).unwrap(), b"new");
        assert_eq!(fs::metadata(&conf).unwrap().permissions().mode() & 0o777, 0o664);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staleness_is_about_content() {
        let dir = scratch("stale");
        let pair = ConfBackup::at(dir.join(CONF_FILE), dir.join(BACKUP_FILE));
        assert!(!pair.stale(), "no file at all is nothing to copy");
        fs::write(dir.join(CONF_FILE), b"one").unwrap();
        assert!(pair.stale(), "a file with no copy is stale");
        fs::write(dir.join(BACKUP_FILE), b"one").unwrap();
        assert!(!pair.stale());
        fs::write(dir.join(CONF_FILE), b"two").unwrap();
        assert!(pair.stale());
        fs::write(dir.join(CONF_FILE), b"").unwrap();
        fs::remove_file(dir.join(BACKUP_FILE)).unwrap();
        assert!(!pair.stale(), "an empty file is never worth copying");
        let _ = fs::remove_dir_all(&dir);
    }
}
