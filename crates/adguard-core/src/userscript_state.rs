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

/// Every file a script's directory may hold, for [`Copies::forget`] — named
/// rather than removed with the directory, so a file this module did not
/// write is never deleted by it.
const FILES: [&str; 5] = [BASE, BASE_VERSION, MINE, MINE_VERSION, MINE_ANCESTOR];

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
        let dir = self.dir(id)?;
        let version = version.unwrap_or("");

        let recorded = read_text(&dir.join(BASE_VERSION));
        let base = fs::read(dir.join(BASE)).ok();
        let (Some(recorded), Some(base)) = (recorded, base) else {
            self.rebase(id, Some(version), current)?;
            return Ok(false);
        };

        if recorded != version {
            let mine_version = read_text(&dir.join(MINE_VERSION));
            let has_mine = dir.join(MINE).is_file();
            if has_mine && mine_version.as_deref() == Some(recorded.as_str()) {
                // The edit was made on the copy being replaced: keep that copy
                // as its ancestor before the new one takes its place.
                write_atomically(&dir.join(MINE_ANCESTOR), &base)?;
            } else if has_mine && dir.join(MINE_ANCESTOR).is_file() {
                // An edit from an older release, already set aside with its
                // ancestor by an earlier update. Another release arriving does
                // not make that pair any less mergeable, and it is the only
                // record of the edit: it stays until the user replaces it with
                // a new one or reverts.
            } else {
                // No edit, or one without the copy it was made on — nothing a
                // merge could use.
                remove_if_present(&dir.join(MINE))?;
                remove_if_present(&dir.join(MINE_VERSION))?;
                remove_if_present(&dir.join(MINE_ANCESTOR))?;
            }
            write_base(&dir, version, current)?;
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
                // one no longer belongs to anything.
                remove_if_present(&dir.join(MINE_ANCESTOR))?;
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
        write_base(&self.dir(id)?, version.unwrap_or(""), current)
    }

    /// Forget the kept edit, on the user's say-so — a reinstall whose dialog
    /// told them their changes would be replaced.
    pub fn discard_edit(&self, id: &str) -> io::Result<()> {
        let dir = self.dir(id)?;
        for file in [MINE, MINE_VERSION, MINE_ANCESTOR] {
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
        let dir = self.dir(id)?;
        let base = fs::read(dir.join(BASE))?;
        let permissions = fs::metadata(target).ok().map(|meta| meta.permissions());
        write_atomically_with(target, &base, permissions)?;
        for file in [MINE, MINE_VERSION, MINE_ANCESTOR] {
            remove_if_present(&dir.join(file))?;
        }
        Ok(())
    }

    /// Delete everything kept for a script that has been removed.
    pub fn forget(&self, id: &str) -> io::Result<()> {
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
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let mut staging = path.as_os_str().to_owned();
    staging.push(format!(
        ".{}-{}.part",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let staging = PathBuf::from(staging);
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
}
