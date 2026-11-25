//! What this application remembers about each userscript, beside AdGuard's own
//! files: the name the user gave it, and the copy AdGuard delivered.
//!
//! **Nothing here is AdGuard's, and nothing here is written into AdGuard's
//! directory except by [`Copies::revert`], which puts AdGuard's own bytes
//! back.** AdGuard has no rename and no notion of a script being edited: its
//! update rewrites the `.meta.json` and the `.user.js` wholesale (contract
//! §15). So a rename stored in the metadata would be undone by the next
//! update, and an edit is only visible at all against a copy of what was there
//! before it.
//!
//! Two stores, in the two XDG directories the basedir spec gives them:
//!
//! - [`Aliases`] in `$XDG_STATE_HOME/adguard-ui/userscripts.yaml` — small,
//!   rewritten whole, and the same kind of thing `window.state` beside it is;
//! - [`Copies`] in `$XDG_DATA_HOME/adguard-ui/userscripts/<id>/` — whole
//!   scripts, up to half a megabyte each, and the only record of what a script
//!   looked like before the user changed it. That is data in the spec's sense
//!   (losing it loses something the user cannot get back), not state.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::model::Userscript;
use crate::window_state::{state_home, SUBDIR};

/// The aliases file, under our own state directory.
const ALIASES_FILE: &str = "userscripts.yaml";

/// The key the aliases sit under, so the file can grow a second section
/// without the first one moving.
const ALIASES_KEY: &str = "aliases";

/// The most of the aliases file we will read — far past any list of names a
/// person would give, and short of anything that is not that list. The same
/// guard `window_state` applies, for its reason.
const ALIASES_LIMIT: u64 = 256 * 1024;

/// The pristine copy: the `.user.js` as AdGuard delivered it.
const BASE: &str = "base.user.js";
/// The version [`BASE`] belongs to.
const BASE_VERSION: &str = "base.version";
/// The user's last edit seen, kept so that an update which overwrites it does
/// not take it out of existence.
const MINE: &str = "mine.user.js";
/// The version [`MINE`] was made on top of.
const MINE_VERSION: &str = "mine.version";
/// The pristine copy [`MINE`] was made from, set aside when an update moves
/// [`BASE`] on. With [`MINE`] and the newly delivered script it is the third
/// side of a merge — which this release does not attempt, but which is
/// impossible to attempt later from data that was not kept.
const MINE_ANCESTOR: &str = "mine.base.user.js";
/// A merge that conflicted, with git's markers in it, written for the user to
/// read beside the updated script ([`Copies::write_merge`]). Never installed:
/// a script with markers in it is not JavaScript.
const MERGE: &str = "merge.user.js";

/// Every file a script's directory may hold, for [`Copies::forget`] — named
/// rather than removed with the directory, so a file this module did not
/// write is never deleted by it.
const FILES: [&str; 6] = [BASE, BASE_VERSION, MINE, MINE_VERSION, MINE_ANCESTOR, MERGE];

/// The files that make up a kept edit, dropped together whenever the edit is.
const EDIT_FILES: [&str; 4] = [MINE, MINE_VERSION, MINE_ANCESTOR, MERGE];

/// A kept edit that is no longer in the installed source.
///
/// Both are AdGuard's doing, never the user's: an update replaced the file
/// over the edit (contract §15, *What `check-update` does to a userscript*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The same version was delivered again over it. `check-update` does this
    /// on every run to an enabled script with no `updateURL`. Bringing the edit
    /// back is exact: nothing upstream moved.
    Overwritten,
    /// A new release replaced the version the edit was made on, and
    /// [`MINE_ANCESTOR`] holds that version. Bringing the edit back is a
    /// three-way merge, which can conflict.
    SetAside,
}

/// What carrying a kept edit into the installed version produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carried {
    /// The edit, applied to the installed version. Ready to install.
    Clean(Vec<u8>),
    /// The edit and the release changed the same lines; the whole script with
    /// git's markers, for a person to read.
    Conflict { marked: String, conflicts: usize },
    /// One of the three copies is not UTF-8, so it cannot be merged as text.
    Unmergeable,
}

/// The user's own names for scripts, by id.
///
/// A path and nothing else, as [`crate::WindowState`] is: what is on disk is
/// the answer.
pub struct Aliases {
    path: PathBuf,
}

impl Aliases {
    /// The file under this machine's `$XDG_STATE_HOME`. `None` only with no
    /// `$HOME` either, and then nothing can be renamed — which is what the
    /// page offers in that case.
    pub fn locate() -> Option<Self> {
        Some(Self::in_state_home(&state_home()?))
    }

    /// The file under an explicitly given state directory — the tests'
    /// sandbox, for the reason [`crate::WindowState::in_state_home`] gives.
    pub fn in_state_home(state_home: &Path) -> Self {
        Self {
            path: state_home.join(SUBDIR).join(ALIASES_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every alias on file, or none.
    ///
    /// **Total, like [`crate::WindowState::load`]**: a missing file, a file
    /// that is not YAML and an entry that is not a pair of strings all mean the
    /// same thing — those scripts show their own names — and there is nothing
    /// else a caller could do about any of them. One bad entry costs itself
    /// alone; the rest are still read.
    pub fn load(&self) -> BTreeMap<String, String> {
        let mut aliases = BTreeMap::new();
        match fs::metadata(&self.path) {
            Ok(meta) if meta.is_file() && meta.len() <= ALIASES_LIMIT => {}
            _ => return aliases,
        }
        let Some(document) = fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| YamlLoader::load_from_str(&text).ok())
            .and_then(|documents| documents.into_iter().next())
        else {
            return aliases;
        };
        if let Some(entries) = document[ALIASES_KEY].as_hash() {
            for (id, name) in entries {
                let (Some(id), Some(name)) = (id.as_str(), name.as_str()) else {
                    continue;
                };
                let name = name.trim();
                if !id.is_empty() && !name.is_empty() {
                    aliases.insert(id.to_owned(), name.to_owned());
                }
            }
        }
        aliases
    }

    /// Name `id` as `alias`, or — with `None`, or a blank — go back to the
    /// name AdGuard gives it.
    ///
    /// Read, changed, written back whole: the file is a few lines, and a
    /// second writer is a second window of this same application, which
    /// renames one script at a time.
    pub fn set(&self, id: &str, alias: Option<&str>) -> io::Result<()> {
        let mut aliases = self.load();
        match alias.map(str::trim).filter(|alias| !alias.is_empty()) {
            Some(alias) => aliases.insert(id.to_owned(), alias.to_owned()),
            None => aliases.remove(id),
        };
        self.save(&aliases)
    }

    /// Lay the aliases on file over freshly read rows, and re-sort them by
    /// the names that now show.
    pub fn apply(&self, scripts: &mut [Userscript]) {
        let aliases = self.load();
        for script in scripts.iter_mut() {
            // An alias equal to the name AdGuard gives is no alias, and saying
            // the script was renamed would be inventing a difference.
            script.alias = aliases
                .get(&script.id)
                .filter(|alias| alias.as_str() != script.original_name())
                .cloned();
        }
        crate::userscripts::sort(scripts);
    }

    fn save(&self, aliases: &BTreeMap<String, String>) -> io::Result<()> {
        let mut entries = Hash::new();
        for (id, alias) in aliases {
            entries.insert(Yaml::String(id.clone()), Yaml::String(alias.clone()));
        }
        let mut root = Hash::new();
        root.insert(Yaml::String(ALIASES_KEY.to_owned()), Yaml::Hash(entries));

        // The emitter, not a format string: an alias is whatever the user
        // typed, and quoting a colon, a leading `-` or a `#` by hand is the
        // kind of thing that works until somebody names a script "Note: …".
        let mut text = String::from(
            "# The names you gave userscripts on AdGuard UI's Extensions page, by id.\n\
             # Only this application reads them: adguard-cli still uses the original\n\
             # names. Delete this file to see the original names again.\n",
        );
        YamlEmitter::new(&mut text)
            .dump(&Yaml::Hash(root))
            .map_err(|err| io::Error::other(err.to_string()))?;
        text.push('\n');
        write_atomically(&self.path, text.as_bytes())
    }
}

/// One change to the copies at a time, in this process.
///
/// Every change is a read, a decision and several writes, and the page makes
/// them from more than one worker thread at once — a reload and a
/// directory-watch re-check overlap exactly when an update lands. Two
/// observers of the same update interleaved can read the old version beside
/// the new copy and file the new release as the edit's ancestor, losing the
/// real one. Each public operation holds this for its whole transaction. One
/// lock for every script rather than one per script, because each holds it for
/// a few file operations and a map of locks would be machinery for nothing.
fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The copies of each script that tell an edit from the script as AdGuard
/// delivered it.
pub struct Copies {
    root: PathBuf,
}

impl Copies {
    /// Under this machine's `$XDG_DATA_HOME`, or `~/.local/share`.
    pub fn locate() -> Option<Self> {
        Some(Self::in_data_home(&data_home()?))
    }

    /// Under an explicitly given data directory.
    pub fn in_data_home(data_home: &Path) -> Self {
        Self {
            root: data_home.join(SUBDIR).join("userscripts"),
        }
    }

    /// Record the script as it is now, and say whether it has been edited.
    ///
    /// `current` is the installed `.user.js`; `version` the metadata's. The
    /// rule, from the three files this keeps:
    ///
    /// - **No pristine copy yet**: this is the first time the script has been
    ///   seen, and what is there now becomes the copy. A script edited before
    ///   this release ever looked at it is therefore taken as delivered —
    ///   there is no earlier copy to know better from.
    /// - **A copy of another version**: AdGuard has updated the script — its
    ///   update rewrites the metadata and the source together — so what is
    ///   there now is the new pristine copy. An edit made on top of the old
    ///   version is kept, with the copy it was made from beside it, for the
    ///   merge a later release will offer.
    /// - **A copy of this version**: edited exactly when the bytes differ, and
    ///   the edit is kept as [`MINE`]. Bytes that match again do **not** drop
    ///   the kept edit. The user may have undone it, but AdGuard may also have
    ///   overwritten it: `check-update` re-downloads an enabled script with no
    ///   `updateURL` on every run, at the same version (contract §15). After
    ///   that, [`MINE`] is the only copy of the edit left anywhere. Only an
    ///   explicit [`Self::revert`] or [`Self::rebase`] — the user's own choice
    ///   — discards it.
    ///
    /// A script with no `@version` is compared as version `""`, so a new
    /// release of it that also carries none looks like an edit until it is
    /// reinstalled from here ([`Self::rebase`]). Nothing better is available:
    /// the version is the only thing AdGuard records that moves with a release.
    ///
    /// `Err` when the copies cannot be read or written; the caller shows the
    /// row as unedited, the one answer that cannot be wrong in a harmful way.
    pub fn observe(&self, id: &str, version: Option<&str>, current: &[u8]) -> io::Result<bool> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        let version = version.unwrap_or("");

        let recorded = read_text(&dir.join(BASE_VERSION));
        let base = fs::read(dir.join(BASE)).ok();
        let (Some(recorded), Some(base)) = (recorded, base) else {
            write_base(&dir, version, current)?;
            return Ok(false);
        };

        if recorded != version {
            new_release(&dir, &recorded, &base, version, current)?;
            return Ok(false);
        }

        if base == current {
            // Not edited now, and any kept edit stays: this may be AdGuard
            // having overwritten it, not the user taking it back.
            return Ok(false);
        }

        // Edited. Kept only when it changed, so a page reload does not rewrite
        // half a megabyte to say what is already on disk.
        if fs::read(dir.join(MINE)).ok().as_deref() != Some(current) {
            if read_text(&dir.join(MINE_VERSION)).as_deref() != Some(version) {
                // A new edit on a new version: the ancestor kept for the old
                // one no longer belongs to anything, and nor does a merge of
                // it the user may have been reading.
                remove_if_present(&dir.join(MINE_ANCESTOR))?;
                remove_if_present(&dir.join(MERGE))?;
            }
            write_atomically(&dir.join(MINE), current)?;
            write_atomically(&dir.join(MINE_VERSION), version.as_bytes())?;
        }
        Ok(true)
    }

    /// [`Self::observe`] every script in `dir`, setting [`Userscript::edited`].
    ///
    /// A source that cannot be read, or copies that cannot be kept, leave the
    /// row unedited: the marker is a hint about the user's own change, and
    /// one that cannot be backed by a comparison is not shown.
    pub fn observe_all(&self, dir: &Path, scripts: &mut [Userscript]) {
        for script in scripts.iter_mut() {
            let path = crate::userscripts::source_path(dir, &script.id);
            script.edited = fs::read(&path)
                .ok()
                .and_then(|current| {
                    self.observe(&script.id, script.version.as_deref(), &current).ok()
                })
                .unwrap_or(false);
        }
    }

    /// Record `current` as the script AdGuard delivered, whatever was kept as
    /// that before.
    ///
    /// For the moment this application has just had AdGuard fetch the
    /// script — an install, a reinstall — when the bytes on disk are known to
    /// be AdGuard's even if the version did not move. A kept edit is left
    /// where it is: AdGuard overwriting the file is not the user giving the
    /// edit up. [`Self::discard_edit`] is for when they have.
    pub fn rebase(&self, id: &str, version: Option<&str>, current: &[u8]) -> io::Result<()> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        let version = version.unwrap_or("");
        // A fetch that moved the version is a release like any other, and
        // the kept edit's ancestor has to be set aside the same way — or the
        // next update would find an edit with no ancestor and drop it.
        let recorded = read_text(&dir.join(BASE_VERSION));
        let base = fs::read(dir.join(BASE)).ok();
        match (recorded, base) {
            (Some(recorded), Some(base)) if recorded != version => {
                new_release(&dir, &recorded, &base, version, current)
            }
            _ => write_base(&dir, version, current),
        }
    }

    /// Forget the kept edit, on the user's say-so — a reinstall whose dialog
    /// told them their changes would be replaced.
    pub fn discard_edit(&self, id: &str) -> io::Result<()> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        for file in EDIT_FILES {
            remove_if_present(&dir.join(file))?;
        }
        Ok(())
    }

    /// Put the pristine copy back over the installed `.user.js` at `target`,
    /// and forget the edit.
    ///
    /// Staged beside the target and renamed over it, so AdGuard never reads a
    /// half-written script; the target's permissions are carried over, since
    /// a fresh file would take this process's umask instead of whatever
    /// AdGuard gave it.
    pub fn revert(&self, id: &str, target: &Path) -> io::Result<()> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        let base = fs::read(dir.join(BASE))?;
        let permissions = fs::metadata(target).ok().map(|meta| meta.permissions());
        write_atomically_with(target, &base, permissions)?;
        for file in EDIT_FILES {
            remove_if_present(&dir.join(file))?;
        }
        Ok(())
    }

    /// Whether a kept edit is missing from the installed source, and why.
    ///
    /// Read only, and meaningful after [`Self::observe`] has seen `current` at
    /// `version` — which is what every read of the page does first. `None` when
    /// there is no kept edit, when it is what is installed, or when the user
    /// has since made a new one: the installed source differing from the
    /// pristine copy *is* the current edit, and nothing is pending behind it.
    pub fn pending(&self, id: &str, version: Option<&str>, current: &[u8]) -> io::Result<Option<Pending>> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        Ok(pending_in(&dir, version.unwrap_or(""), current))
    }

    /// Carry the kept edit into what is installed: exactly, when AdGuard only
    /// re-delivered the same version over it, and by a three-way merge when a
    /// release replaced it ([`crate::script_merge`]). Nothing is written;
    /// [`Self::put_source`] installs a clean result.
    ///
    /// `label` names the installed version in a conflict's markers. `None`
    /// when nothing is pending.
    pub fn carry_over(
        &self,
        id: &str,
        version: Option<&str>,
        current: &[u8],
        label: &str,
    ) -> io::Result<Option<Carried>> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        let Some(pending) = pending_in(&dir, version.unwrap_or(""), current) else {
            return Ok(None);
        };
        let mine = fs::read(dir.join(MINE))?;
        let ancestor = match pending {
            // Upstream did not move: the copy the edit was made on is the one
            // installed, and the merge is the edit itself.
            Pending::Overwritten => current.to_vec(),
            Pending::SetAside => fs::read(dir.join(MINE_ANCESTOR))?,
        };
        let (Ok(ancestor), Ok(mine), Ok(upstream)) = (
            std::str::from_utf8(&ancestor),
            std::str::from_utf8(&mine),
            std::str::from_utf8(current),
        ) else {
            return Ok(Some(Carried::Unmergeable));
        };
        Ok(Some(match crate::script_merge::merge(ancestor, mine, upstream, label) {
            crate::script_merge::Merged::Clean(text) => Carried::Clean(text.into_bytes()),
            crate::script_merge::Merged::Conflict { marked, conflicts } => {
                Carried::Conflict { marked, conflicts }
            }
        }))
    }

    /// Install `bytes` as the script's source at `target` — a merge, or the
    /// kept edit itself — **only if the source is still `expected`**, the bytes
    /// the merge was made against. `Ok(false)` when it moved in between: an
    /// editor's save or another update got there first, and writing over it
    /// would lose that instead.
    ///
    /// Staged and renamed with the target's permissions, as [`Self::revert`]
    /// does. The copies are left to [`Self::observe`], which files what is
    /// now installed as the edit on top of the current version — dropping the
    /// ancestor, which no longer belongs to anything.
    pub fn put_source(&self, id: &str, target: &Path, expected: &[u8], bytes: &[u8]) -> io::Result<bool> {
        let _held = exclusive();
        // For the id's check alone: the write is to AdGuard's file.
        self.dir(id)?;
        if fs::read(target)? != expected {
            return Ok(false);
        }
        let permissions = fs::metadata(target).ok().map(|meta| meta.permissions());
        write_atomically_with(target, bytes, permissions)?;
        Ok(true)
    }

    /// The kept edit, for *Keep my version*: [`Self::put_source`] it to put it
    /// back as it was, the release's changes left out.
    pub fn kept_edit(&self, id: &str) -> io::Result<Vec<u8>> {
        let _held = exclusive();
        fs::read(self.dir(id)?.join(MINE))
    }

    /// Keep a conflicted merge where the user can open it, and say where.
    ///
    /// In this application's directory, never AdGuard's: AdGuard would take a
    /// `.user.js` there for a script, and one with markers in it does not run.
    /// Dropped with the edit ([`EDIT_FILES`]).
    pub fn write_merge(&self, id: &str, marked: &str) -> io::Result<PathBuf> {
        let _held = exclusive();
        let path = self.dir(id)?.join(MERGE);
        write_atomically(&path, marked.as_bytes())?;
        Ok(path)
    }

    /// Delete everything kept for a script that has been removed.
    pub fn forget(&self, id: &str) -> io::Result<()> {
        let _held = exclusive();
        let dir = self.dir(id)?;
        for file in FILES {
            remove_if_present(&dir.join(file))?;
        }
        // Only if empty, which is the point of naming the files: anything
        // else in there is not ours to delete.
        let _ = fs::remove_dir(&dir);
        Ok(())
    }

    /// The directory for one script, named after its id.
    ///
    /// The id is a filename stem from AdGuard's directory, so it already fits
    /// in a name — AdGuard's own `<id>.meta.json` is ten bytes longer — and
    /// contains no `/`. What it can still be is `.` or `..` (a file called
    /// `..meta.json`), which as a directory name would put the copies one level
    /// up; that, and an empty id, are refused rather than mapped.
    fn dir(&self, id: &str) -> io::Result<PathBuf> {
        let usable = !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\0']);
        if !usable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{id:?} cannot name a directory"),
            ));
        }
        Ok(self.root.join(id))
    }
}

/// [`Copies::pending`], for a directory already chosen and locked.
fn pending_in(dir: &Path, version: &str, current: &[u8]) -> Option<Pending> {
    // Not yet observed at this version: nothing here describes `current`.
    if read_text(&dir.join(BASE_VERSION)).as_deref() != Some(version) {
        return None;
    }
    let base = fs::read(dir.join(BASE)).ok()?;
    if base != current {
        // An edit is installed — the kept one, or a newer one. Either way it
        // is what runs, and nothing is missing.
        return None;
    }
    let mine = fs::read(dir.join(MINE)).ok()?;
    if mine == base {
        return None;
    }
    if read_text(&dir.join(MINE_VERSION)).as_deref() == Some(version) {
        Some(Pending::Overwritten)
    } else if dir.join(MINE_ANCESTOR).is_file() {
        Some(Pending::SetAside)
    } else {
        None
    }
}

/// AdGuard delivered `current` at `version`, replacing `base` at `recorded`:
/// set the kept edit aside with the copy it was made on, then record the new
/// pristine copy.
fn new_release(
    dir: &Path,
    recorded: &str,
    base: &[u8],
    version: &str,
    current: &[u8],
) -> io::Result<()> {
    let mine_version = read_text(&dir.join(MINE_VERSION));
    let has_mine = dir.join(MINE).is_file();
    if has_mine && mine_version.as_deref() == Some(recorded) {
        // The edit was made on the copy being replaced: keep that copy as its
        // ancestor before the new one takes its place.
        write_atomically(&dir.join(MINE_ANCESTOR), base)?;
    } else if has_mine && dir.join(MINE_ANCESTOR).is_file() {
        // An edit from an older release, already set aside with its ancestor
        // by an earlier update. Another release arriving does not make that
        // pair any less mergeable, and it is the only record of the edit: it
        // stays until the user replaces it with a new one or reverts.
    } else {
        // No edit, or one without the copy it was made on — nothing a merge
        // could use.
        for file in EDIT_FILES {
            remove_if_present(&dir.join(file))?;
        }
    }
    write_base(dir, version, current)
}

fn write_base(dir: &Path, version: &str, current: &[u8]) -> io::Result<()> {
    // The source first and its version second: a write interrupted between
    // the two leaves an old version beside a new copy, which the next
    // observation reads as an update and re-records — never the other way
    // round, which would hold the new version over the old copy and call the
    // delivered script an edit.
    write_atomically(&dir.join(BASE), current)?;
    write_atomically(&dir.join(BASE_VERSION), version.as_bytes())
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomically_with(path, bytes, None)
}

/// Stage beside `path` and rename over it, so a reader sees the old file or
/// the new one and never a prefix of either.
fn write_atomically_with(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the file has no directory")
    })?;
    fs::create_dir_all(dir)?;
    // Unique per write, not just per file: the page compares on more than one
    // worker thread at a time — a reload and a directory-watch re-check can
    // overlap — and two writers sharing one staging name would rename each
    // other's file away mid-write.
    //
    // A name of its own rather than the target's with a suffix: an id may be
    // up to 245 bytes (AdGuard's `<id>.meta.json` has to fit in 255), so the
    // target's name plus anything is past the limit, and *Revert* on the
    // longest Greasy Fork names would fail with `ENAMETOOLONG`. A leading dot
    // and no `.user.js` or `.meta.json` keep AdGuard and the directory watch
    // from taking it for a script.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let staging = dir.join(format!(
        ".adguard-ui-{}-{}.part",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::write(&staging, bytes)
        .and_then(|()| match permissions {
            Some(permissions) => fs::set_permissions(&staging, permissions),
            None => Ok(()),
        })
        .and_then(|()| fs::rename(&staging, path));
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn data_home() -> Option<PathBuf> {
    data_home_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// The rule apart from the environment, as `window_state::state_home_from`
/// is, and ignoring a relative value for its reason.
fn data_home_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg);
        }
    }
    Some(PathBuf::from(home?).join(".local/share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of our own under `/tmp`, the sandbox `window_state.rs`
    /// uses, for its reason.
    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("adguard-ui-userscript-state-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create the sandbox");
            Self { dir }
        }

        fn aliases(&self) -> Aliases {
            Aliases::in_state_home(&self.dir)
        }

        fn copies(&self) -> Copies {
            Copies::in_data_home(&self.dir)
        }

        fn kept(&self, id: &str, file: &str) -> Option<Vec<u8>> {
            fs::read(self.dir.join(SUBDIR).join("userscripts").join(id).join(file)).ok()
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn script(id: &str, name: &str) -> Userscript {
        Userscript {
            id: id.to_owned(),
            name: name.to_owned(),
            description: String::new(),
            version: None,
            homepage: None,
            download_url: None,
            update_url: None,
            enabled: false,
            ambiguous: false,
            alias: None,
            edited: false,
        }
    }

    #[test]
    fn no_file_means_no_aliases() {
        let sandbox = Sandbox::new("no-aliases");
        assert!(sandbox.aliases().load().is_empty());
    }

    /// What a user types goes into YAML, and a colon, a `#`, quotes, a leading
    /// dash and an emoji are all things somebody will type.
    #[test]
    fn an_alias_survives_the_round_trip_whatever_it_says() {
        let sandbox = Sandbox::new("round-trip");
        let aliases = sandbox.aliases();
        let awkward = "- Note: \"video\" # downloader 📥 'x'";
        aliases.set("%F0%9F%93%A5%20Free", Some(awkward)).expect("save");
        aliases.set("assistant", Some("Assistant")).expect("save");

        let loaded = aliases.load();
        assert_eq!(loaded.get("%F0%9F%93%A5%20Free").map(String::as_str), Some(awkward));
        assert_eq!(loaded.get("assistant").map(String::as_str), Some("Assistant"));
    }

    #[test]
    fn a_blank_alias_resets_to_the_original_name() {
        let sandbox = Sandbox::new("reset");
        let aliases = sandbox.aliases();
        aliases.set("x", Some("Mine")).expect("save");
        aliases.set("x", Some("   ")).expect("save");
        assert!(aliases.load().is_empty());
        aliases.set("x", Some("Mine")).expect("save");
        aliases.set("x", None).expect("save");
        assert!(aliases.load().is_empty());
    }

    /// One entry that is not a pair of strings costs itself, not the file.
    #[test]
    fn a_damaged_entry_does_not_lose_the_others() {
        let sandbox = Sandbox::new("damaged");
        let aliases = sandbox.aliases();
        fs::create_dir_all(aliases.path().parent().unwrap()).unwrap();
        fs::write(aliases.path(), "aliases:\n  good: Fine\n  bad: [1, 2]\n  3: Number\n").unwrap();
        let loaded = aliases.load();
        assert_eq!(loaded.get("good").map(String::as_str), Some("Fine"));
        assert!(!loaded.contains_key("bad"));
        fs::write(aliases.path(), "{{{ not yaml").unwrap();
        assert!(aliases.load().is_empty());
    }

    /// The alias shows and sorts; one equal to the original name is ignored.
    #[test]
    fn applying_renames_and_resorts() {
        let sandbox = Sandbox::new("apply");
        let aliases = sandbox.aliases();
        aliases.set("zebra", Some("Aardvark")).expect("save");
        aliases.set("same", Some("Same")).expect("save");
        let mut scripts = vec![script("same", "Same"), script("zebra", "Zebra")];
        aliases.apply(&mut scripts);
        assert_eq!(scripts[0].display_name(), "Aardvark");
        assert_eq!(scripts[0].original_name(), "Zebra");
        assert_eq!(scripts[1].alias, None, "an alias equal to the name is no alias");
    }

    #[test]
    fn the_first_sight_is_the_pristine_copy() {
        let sandbox = Sandbox::new("first-sight");
        let copies = sandbox.copies();
        assert!(!copies.observe("x", Some("1.0"), b"original").unwrap());
        assert_eq!(sandbox.kept("x", BASE).as_deref(), Some(&b"original"[..]));
        assert_eq!(sandbox.kept("x", BASE_VERSION).as_deref(), Some(&b"1.0"[..]));
        assert!(!copies.observe("x", Some("1.0"), b"original").unwrap());
    }

    #[test]
    fn different_bytes_at_the_same_version_are_an_edit_and_are_kept() {
        let sandbox = Sandbox::new("edit");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        assert!(copies.observe("x", Some("1.0"), b"edited").unwrap());
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"edited"[..]));
        assert_eq!(sandbox.kept("x", MINE_VERSION).as_deref(), Some(&b"1.0"[..]));

        // The original back on disk — undone in an editor, or overwritten by
        // `check-update` re-fetching a script with no `updateURL`. Not edited
        // now, but the kept edit survives: in the second case it is the only
        // copy there is.
        assert!(!copies.observe("x", Some("1.0"), b"original").unwrap());
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"edited"[..]));
    }

    /// An update overwrites the edit on disk; the edit and the copy it was
    /// made from both survive here, which is what a merge would need.
    #[test]
    fn an_update_keeps_the_edit_and_its_ancestor() {
        let sandbox = Sandbox::new("update");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        copies.observe("x", Some("1.0"), b"edited").unwrap();

        assert!(!copies.observe("x", Some("1.1"), b"new release").unwrap());
        assert_eq!(sandbox.kept("x", BASE).as_deref(), Some(&b"new release"[..]));
        assert_eq!(sandbox.kept("x", BASE_VERSION).as_deref(), Some(&b"1.1"[..]));
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"edited"[..]));
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR).as_deref(), Some(&b"original"[..]));

        // A fresh edit on the new release replaces the old one, and the old
        // ancestor goes with it.
        assert!(copies.observe("x", Some("1.1"), b"edited again").unwrap());
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"edited again"[..]));
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR), None);
    }

    /// A second release does not cost the edit set aside at the first: the
    /// edit and the copy it was made on are still a mergeable pair.
    #[test]
    fn a_second_update_keeps_the_edit_set_aside_at_the_first() {
        let sandbox = Sandbox::new("second-update");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        copies.observe("x", Some("1.0"), b"edited").unwrap();
        copies.observe("x", Some("1.1"), b"release two").unwrap();
        copies.observe("x", Some("1.2"), b"release three").unwrap();
        assert_eq!(sandbox.kept("x", BASE).as_deref(), Some(&b"release three"[..]));
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"edited"[..]));
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR).as_deref(), Some(&b"original"[..]));
        assert_eq!(sandbox.kept("x", MINE_VERSION).as_deref(), Some(&b"1.0"[..]));
    }

    /// A reload and a directory-watch re-check observe the same update at
    /// once. Interleaved, one could read the old version beside the new copy
    /// and file the new release as the edit's ancestor; serialised, the
    /// ancestor stays the copy the edit was made on.
    #[test]
    fn concurrent_observers_of_an_update_keep_the_real_ancestor() {
        let sandbox = Sandbox::new("concurrent");
        for round in 0..20 {
            let id = format!("x{round}");
            let copies = sandbox.copies();
            copies.observe(&id, Some("1.0"), b"original").unwrap();
            copies.observe(&id, Some("1.0"), b"edited").unwrap();
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let copies = sandbox.copies();
                    let id = id.clone();
                    std::thread::spawn(move || copies.observe(&id, Some("1.1"), b"new release"))
                })
                .collect();
            for thread in threads {
                thread.join().unwrap().unwrap();
            }
            assert_eq!(sandbox.kept(&id, MINE_ANCESTOR).as_deref(), Some(&b"original"[..]));
            assert_eq!(sandbox.kept(&id, MINE).as_deref(), Some(&b"edited"[..]));
        }
    }

    #[test]
    fn an_update_without_an_edit_keeps_nothing_extra() {
        let sandbox = Sandbox::new("plain-update");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        copies.observe("x", Some("1.1"), b"new").unwrap();
        assert_eq!(sandbox.kept("x", MINE), None);
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR), None);
    }

    #[test]
    fn reverting_puts_the_pristine_copy_back() {
        let sandbox = Sandbox::new("revert");
        let copies = sandbox.copies();
        let target = sandbox.dir.join("x.user.js");
        fs::write(&target, b"original").unwrap();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        fs::write(&target, b"edited").unwrap();
        assert!(copies.observe("x", Some("1.0"), b"edited").unwrap());

        copies.revert("x", &target).expect("revert");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(sandbox.kept("x", MINE), None);
        assert!(!copies.observe("x", Some("1.0"), &fs::read(&target).unwrap()).unwrap());
    }

    /// A reinstall from here at the same version: whatever AdGuard fetched is
    /// the new pristine copy, even if the author changed it without a bump.
    /// The longest id AdGuard's own files allow still reverts: the staging
    /// file must not be named after the target.
    #[test]
    fn reverting_a_script_with_the_longest_id_works() {
        let sandbox = Sandbox::new("long-id");
        let copies = sandbox.copies();
        let id = "x".repeat(255 - ".meta.json".len());
        let target = sandbox.dir.join(format!("{id}.user.js"));
        fs::write(&target, b"original").unwrap();
        copies.observe(&id, Some("1.0"), b"original").unwrap();
        fs::write(&target, b"edited").unwrap();
        copies.revert(&id, &target).expect("revert a 245-byte id");
        assert_eq!(fs::read(&target).unwrap(), b"original");
    }

    /// A fetch of ours that lands a new version sets the edit aside exactly
    /// as an update seen by `observe` does, so a later release keeps it.
    #[test]
    fn rebasing_onto_a_new_version_keeps_the_edit_mergeable() {
        let sandbox = Sandbox::new("rebase-new-version");
        let copies = sandbox.copies();
        copies.observe("x", Some("1"), b"original").unwrap();
        copies.observe("x", Some("1"), b"my edit").unwrap();
        copies.rebase("x", Some("2"), b"release 2").unwrap();
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR).as_deref(), Some(&b"original"[..]));
        copies.observe("x", Some("3"), b"release 3").unwrap();
        assert_eq!(sandbox.kept("x", MINE).as_deref(), Some(&b"my edit"[..]));
        assert_eq!(sandbox.kept("x", MINE_ANCESTOR).as_deref(), Some(&b"original"[..]));
    }

    #[test]
    fn rebasing_takes_the_bytes_as_delivered() {
        let sandbox = Sandbox::new("rebase");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        copies.observe("x", Some("1.0"), b"edited").unwrap();
        copies.rebase("x", Some("1.0"), b"silently changed upstream").unwrap();
        assert!(!copies.observe("x", Some("1.0"), b"silently changed upstream").unwrap());
        assert_eq!(
            sandbox.kept("x", MINE).as_deref(),
            Some(&b"edited"[..]),
            "AdGuard replacing the file is not the user giving the edit up"
        );
        copies.discard_edit("x").unwrap();
        assert_eq!(sandbox.kept("x", MINE), None);
    }

    #[test]
    fn forgetting_removes_only_our_files() {
        let sandbox = Sandbox::new("forget");
        let copies = sandbox.copies();
        copies.observe("x", Some("1.0"), b"original").unwrap();
        copies.observe("x", Some("1.0"), b"edited").unwrap();
        copies.forget("x").unwrap();
        assert!(!sandbox.dir.join(SUBDIR).join("userscripts").join("x").exists());
    }

    /// `..meta.json` is a file AdGuard's directory can hold; its id must not
    /// become a path one level up.
    #[test]
    fn an_id_that_is_a_path_component_is_refused() {
        let sandbox = Sandbox::new("dotdot");
        let copies = sandbox.copies();
        for id in ["", ".", "..", "a/b"] {
            assert!(copies.observe(id, Some("1"), b"x").is_err(), "{id:?}");
        }
        assert!(!sandbox.dir.join(SUBDIR).join(BASE).exists());
    }

    #[test]
    fn a_relative_data_home_is_ignored() {
        assert_eq!(
            data_home_from(Some("relative".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(
            data_home_from(Some("/x".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x"))
        );
    }

    /// The installed `.user.js` of a test script, in a directory of its own.
    fn installed(sandbox: &Sandbox, id: &str, bytes: &[u8]) -> PathBuf {
        let dir = sandbox.dir.join("adguard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.user.js"));
        fs::write(&path, bytes).unwrap();
        path
    }

    const V1: &str = "// ==UserScript==\n// @version 1.0\n// ==/UserScript==\nconst a = 1;\nconst b = 2;\nconst c = 3;\n";

    /// `check-update` re-delivering the same version over an edit — the
    /// empty-`updateURL` case — leaves the edit pending, and carrying it over
    /// gives back exactly the edit.
    #[test]
    fn an_edit_overwritten_at_the_same_version_comes_back_exactly() {
        let sandbox = Sandbox::new("overwritten");
        let copies = sandbox.copies();
        let edited = V1.replace("c = 3", "c = 30");
        copies.observe("s", Some("1.0"), V1.as_bytes()).unwrap();
        assert!(copies.observe("s", Some("1.0"), edited.as_bytes()).unwrap());
        assert_eq!(copies.pending("s", Some("1.0"), edited.as_bytes()).unwrap(), None, "the edit is installed");

        // AdGuard puts its copy back.
        assert!(!copies.observe("s", Some("1.0"), V1.as_bytes()).unwrap());
        assert_eq!(copies.pending("s", Some("1.0"), V1.as_bytes()).unwrap(), Some(Pending::Overwritten));
        assert_eq!(
            copies.carry_over("s", Some("1.0"), V1.as_bytes(), "1.0").unwrap(),
            Some(Carried::Clean(edited.clone().into_bytes()))
        );

        let target = installed(&sandbox, "s", V1.as_bytes());
        assert!(copies.put_source("s", &target, V1.as_bytes(), edited.as_bytes()).unwrap());
        assert_eq!(fs::read(&target).unwrap(), edited.as_bytes());
        assert!(copies.observe("s", Some("1.0"), edited.as_bytes()).unwrap(), "edited again");
        assert_eq!(copies.pending("s", Some("1.0"), edited.as_bytes()).unwrap(), None);
    }

    /// A release over an edit: the edit is set aside with its ancestor, and
    /// carried into the release by a merge that keeps both sides' changes.
    #[test]
    fn an_edit_set_aside_by_a_release_is_merged_into_it() {
        let sandbox = Sandbox::new("set-aside");
        let copies = sandbox.copies();
        let edited = V1.replace("c = 3", "c = 30");
        let v2 = V1.replace("@version 1.0", "@version 2.0").replace("a = 1", "a = 10");
        copies.observe("s", Some("1.0"), V1.as_bytes()).unwrap();
        copies.observe("s", Some("1.0"), edited.as_bytes()).unwrap();
        copies.observe("s", Some("2.0"), v2.as_bytes()).unwrap();

        assert_eq!(copies.pending("s", Some("2.0"), v2.as_bytes()).unwrap(), Some(Pending::SetAside));
        let Some(Carried::Clean(merged)) = copies.carry_over("s", Some("2.0"), v2.as_bytes(), "2.0").unwrap() else {
            panic!("different lines, so a clean merge");
        };
        let merged = String::from_utf8(merged).unwrap();
        assert!(merged.contains("@version 2.0") && merged.contains("a = 10") && merged.contains("c = 30"));

        let target = installed(&sandbox, "s", v2.as_bytes());
        assert!(copies.put_source("s", &target, v2.as_bytes(), merged.as_bytes()).unwrap());
        assert!(copies.observe("s", Some("2.0"), merged.as_bytes()).unwrap());
        assert_eq!(sandbox.kept("s", MINE_ANCESTOR), None, "the old ancestor goes once the edit sits on 2.0");
        assert_eq!(copies.pending("s", Some("2.0"), merged.as_bytes()).unwrap(), None);
    }

    #[test]
    fn a_clash_is_a_conflict_and_its_merge_file_goes_with_the_edit() {
        let sandbox = Sandbox::new("conflict");
        let copies = sandbox.copies();
        let edited = V1.replace("b = 2", "b = 'mine'");
        let v2 = V1.replace("@version 1.0", "@version 2.0").replace("b = 2", "b = 'theirs'");
        copies.observe("s", Some("1.0"), V1.as_bytes()).unwrap();
        copies.observe("s", Some("1.0"), edited.as_bytes()).unwrap();
        copies.observe("s", Some("2.0"), v2.as_bytes()).unwrap();

        let Some(Carried::Conflict { marked, conflicts }) =
            copies.carry_over("s", Some("2.0"), v2.as_bytes(), "2.0").unwrap()
        else {
            panic!("both changed line b");
        };
        assert_eq!(conflicts, 1);
        let path = copies.write_merge("s", &marked).unwrap();
        assert!(path.is_file());
        assert_eq!(copies.kept_edit("s").unwrap(), edited.as_bytes());

        copies.discard_edit("s").unwrap();
        assert!(!path.exists(), "dropped with the edit");
        assert_eq!(copies.pending("s", Some("2.0"), v2.as_bytes()).unwrap(), None);
    }

    /// A source that moved since the merge was made is not written over.
    #[test]
    fn put_source_refuses_a_source_that_moved() {
        let sandbox = Sandbox::new("moved");
        let copies = sandbox.copies();
        let target = installed(&sandbox, "s", b"saved by an editor meanwhile");
        assert!(!copies.put_source("s", &target, V1.as_bytes(), b"merged").unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"saved by an editor meanwhile");
    }

    /// No kept edit, or the kept edit installed, is nothing pending.
    #[test]
    fn nothing_is_pending_without_a_missing_edit() {
        let sandbox = Sandbox::new("nothing");
        let copies = sandbox.copies();
        copies.observe("s", Some("1.0"), V1.as_bytes()).unwrap();
        assert_eq!(copies.pending("s", Some("1.0"), V1.as_bytes()).unwrap(), None);
        assert_eq!(copies.carry_over("s", Some("1.0"), V1.as_bytes(), "1.0").unwrap(), None);
        assert_eq!(copies.pending("never-seen", Some("1.0"), V1.as_bytes()).unwrap(), None);
    }

}
