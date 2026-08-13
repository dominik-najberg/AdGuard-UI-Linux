//! The window's remembered size: `~/.local/state/adguard-ui/window.state`.
//!
//! The first file this application writes for itself. Everything else it
//! persists belongs to AdGuard — `proxy.yaml` through `adguard-cli`, the two
//! filter databases through the same — and the one file it already wrote of its
//! own is a login entry the desktop reads, not us ([`crate::autostart`]). This
//! is state in the XDG basedir spec's own sense of the word: `$XDG_STATE_HOME`
//! is for what "should persist between (application) restarts, but that is not
//! important or portable enough to the user that it should be stored in
//! `$XDG_DATA_HOME`", and among the examples it gives for that is the "current
//! state of the application that can be reused on a restart (view, layout, open
//! files, undo history, …)". So `~/.local/state`, not `~/.config`: nobody
//! hand-edits this, nobody syncs it between machines, and losing it costs one
//! resize.
//!
//! **The size is remembered. The position is not, and cannot be.** GTK4 removed
//! `gtk_window_move()` and `gtk_window_get_position()` and put nothing in their
//! place — measured 14 August 2026 against the installed GTK 4.22.4, where
//! `nm -D libgtk-4.so.1` defines neither, and against the `gtk4`/`gdk4` 0.11.4
//! crates, which expose no position getter or setter on `Window`, `Surface` or
//! `Toplevel`. It is not an omission in the bindings and not a Wayland
//! limitation that X11 escapes: `xdg-shell` gives a client no request and no
//! event carrying its own toplevel's coordinates, because placement is the
//! compositor's job there, and the X11 route that still works goes around GDK
//! entirely. So this file holds no `x` and no `y`, and it is worth knowing that
//! it never will rather than wondering which release adds them.
//!
//! **`maximized` is stored because the size alone cannot express it.**
//! Maximizing a window does not move its default size at all — measured 14
//! August 2026: a window set to 1000×640 and then maximized still reported
//! `default_size` as `(1000, 640)`, which is what it unmaximizes back to. That
//! is the right number to keep, and it means a file holding only a size has no
//! way to say *this window was maximized*: the next launch would open it at
//! 1000×640 on a screen the user had filled. Storing the pair restores both
//! halves of what they did — maximized, over a size to come back to.
//!
//! It also settles what GTK does when a restored size is close to the screen's:
//! the window comes back maximized whether it asked to be or not, with the size
//! rewritten to the one that fits. Measured the same day on this machine's
//! 1536×960 display, a window asked for at the full 1536×960 was granted
//! 1469×928 *and* maximized, both written back into the properties this module
//! reads. Because the flag is stored, that is recorded as what it is and the
//! next launch asks for it deliberately.
//!
//! Nothing here knows what a window is. The GUI reads the geometry, hands the
//! sizes of the monitors it can see to [`Geometry::fitted`], and passes the
//! result to the toolkit — so this module stays testable without a display, as
//! `architecture.md` §2 requires of everything in this crate.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The size the window opens at when nothing has been saved, which is where it
/// opened for every release up to this one.
///
/// It lives here rather than in the builder that uses it because it is now two
/// things at once — the shipped default *and* what a corrupt or missing state
/// file falls back to — and those two drifting apart would be invisible: the
/// window would simply open at one size on a fresh machine and another after a
/// bad read, with nothing on screen to say which was which.
pub const DEFAULT_WIDTH: i32 = 880;

/// See [`DEFAULT_WIDTH`].
pub const DEFAULT_HEIGHT: i32 = 720;

/// Below this a saved size is not a small window, it is a broken file.
///
/// Deliberately far under what this window can actually shrink to, and the
/// looseness is the point. **GTK enforces the real minimum itself, and writes
/// the enforced value back**: measured 14 August 2026, a window asked to open
/// at 100×100 over a sidebar that cannot go under 180 came up at 360×200 and
/// reported *that* as its default size. So a size under this floor cannot
/// produce a window too small to use — it can only mean the file is not one we
/// wrote.
///
/// Pinning the floor to this window's own measured minimum would therefore buy
/// nothing and cost something real: that figure moves with the text scale, the
/// icon theme and the widget versions underneath us, so it would be a constant
/// measured on one machine and applied to every other. Someone whose desktop
/// makes this window legitimately smaller than this one does would find it back
/// at 880×720 on every launch, with nothing on screen to say why.
const MIN_WIDTH: i32 = 200;

/// See [`MIN_WIDTH`].
const MIN_HEIGHT: i32 = 150;

/// And above this it is not a window either — no display on any desk is this
/// big, so a file saying so is a file that has been damaged rather than one
/// describing a window someone wants back.
///
/// The number is not derived from anything and does not need to be: the clamp
/// that decides what a window actually opens at is [`Geometry::fitted`],
/// against the monitors that exist. This is only the backstop for a value read
/// before any display is known, and its one job is to keep a corrupted figure
/// from being handed to the toolkit as though it were a size.
const MAX_DIMENSION: i32 = 32_767;

/// The most of this file we will read.
///
/// It is three keys and a comment. Anything past this is not a state file that
/// grew, it is a different file under our name — a log an editor pointed
/// somewhere odd — and reading it into memory to find no `width =` in it would
/// be the whole cost of the mistake.
///
/// **A size is not the whole of the check, because for the worst case it reads
/// as zero.** A fifo, a character device, a `/dev/zero` symlink: `metadata`
/// reports length 0 for all of them, so a size test alone waves through exactly
/// the files that would hang the read or swallow the machine's memory. What
/// excludes those is [`std::fs::Metadata::is_file`], and it has to be asked
/// separately. A symlink to an ordinary file still passes both — `metadata`
/// follows it — so a home directory kept under a dotfile manager is unaffected.
const LIMIT: u64 = 64 * 1024;

/// Our own directory under `$XDG_STATE_HOME`, named after the binary.
///
/// The binary's name and not the application id, which is the split the tree
/// already makes: `paths.rs` looks in `adguard-cli` for AdGuard's data, and the
/// reverse-DNS id appears in [`crate::autostart::ENTRY`] only because a desktop
/// entry's *filename* has to match it or GNOME stops grouping the window with
/// its launcher.
const SUBDIR: &str = "adguard-ui";

/// The file itself. Suffixed, so a second thing worth remembering can sit
/// beside it later without this one needing a rename first.
const FILE: &str = "window.state";

/// The half-written file, staged beside the real one so an interrupted write
/// cannot leave a truncated state file where the next launch would read it.
///
/// The same shape as [`crate::autostart`]'s staging name and for a weaker
/// version of the same reason: nothing but this module ever looks in this
/// directory, so a leftover would be litter rather than a hazard — but it would
/// still be litter with our name on it.
const STAGING: &str = "state.part";

/// A window's size, and whether it was maximized.
///
/// No position; the module header says why. Copy rather than a reference type
/// because it is three fields and is compared far more often than it is
/// written — the saver holds the last value it wrote and skips a save that
/// would not change the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for Geometry {
    /// What the window opened at before it remembered anything.
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            maximized: false,
        }
    }
}

impl Geometry {
    /// Read a state file, taking whatever is legible and defaulting the rest.
    ///
    /// **Per key, not per file**, which is the whole argument for a format this
    /// plain over the YAML this crate already has a parser for: a file
    /// truncated mid-write by a machine losing power keeps the keys that made
    /// it to disk, where a document parser would reject the lot and lose a
    /// height that was sitting there intact. Unknown keys are ignored for the
    /// same reason in the other direction — a file written by a later version
    /// is a file we can still read the width out of.
    ///
    /// Total by construction: every failure is a key falling back, so there is
    /// no error for a caller to decide what to do about.
    fn parse(text: &str) -> Self {
        let mut geometry = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // On the first `=`, so a value carrying one is the value's problem
            // and not a line that silently vanishes.
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "width" => geometry.width = value.parse().unwrap_or(DEFAULT_WIDTH),
                "height" => geometry.height = value.parse().unwrap_or(DEFAULT_HEIGHT),
                // Anything that is not the word we write reads as not
                // maximized, which is the state a user can always get out of.
                "maximized" => geometry.maximized = value.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
        geometry
    }

    /// The file's text — the same bytes for the same geometry, every time, so a
    /// save that would change nothing can be skipped by comparing the geometry
    /// rather than the file.
    ///
    /// It explains itself, the way the login entry does. Whoever finds this
    /// file is finding it because they went looking for where a window size is
    /// kept, and the two things they will then want to know are how to get rid
    /// of it and why there is no position in it.
    fn render(&self) -> String {
        format!(
            "# AdGuard UI's window, as you last left it. Delete this file and the\n\
             # window opens at {DEFAULT_WIDTH}x{DEFAULT_HEIGHT} again; nothing else here depends on it.\n\
             #\n\
             # There is no position, and there will not be one: GTK4 gives an\n\
             # application no way to ask where its own window is, or to put it back.\n\
             width = {}\n\
             height = {}\n\
             maximized = {}\n",
            self.width,
            self.height,
            self.maximized,
        )
    }

    /// The same geometry, cut down to something the monitors in front of the
    /// user can actually show.
    ///
    /// `monitors` is `(width, height)` in the logical pixels the toolkit sizes
    /// windows in — plain numbers rather than a GDK type, because this crate
    /// does not link GTK and this is the whole of what it needs to know about
    /// a display.
    ///
    /// **Against the largest of them, not the one the window was on.** Which
    /// monitor a window opens on is the compositor's decision and not ours, and
    /// clamping to the smallest attached display would shrink a window that
    /// fits perfectly well on the one it is about to appear on. The case this
    /// exists for is the laptop that spent yesterday on a 3440-wide desk and
    /// today has only its own screen: the saved size is not wrong, it is merely
    /// no longer usable, and coming back at the width of the display beats
    /// coming back at a width whose right-hand edge is past it.
    ///
    /// No monitors means no clamp. A display list that could not be read is not
    /// evidence that the saved size is bad.
    pub fn fitted(self, monitors: &[(i32, i32)]) -> Self {
        let widest = monitors.iter().map(|(width, _)| *width).max();
        let tallest = monitors.iter().map(|(_, height)| *height).max();

        Self {
            width: widest.map_or(self.width, |limit| self.width.min(limit)),
            height: tallest.map_or(self.height, |limit| self.height.min(limit)),
            maximized: self.maximized,
        }
        .sane()
    }

    /// Each dimension, or the default in place of one that is not a size.
    ///
    /// Per dimension rather than all-or-nothing: a file with a good width and a
    /// mangled height has told us one true thing, and there is no reading of
    /// "fall back safely" under which throwing it away is safer.
    fn sane(self) -> Self {
        let usable = |value: i32, min: i32, default: i32| {
            if (min..=MAX_DIMENSION).contains(&value) {
                value
            } else {
                default
            }
        };

        Self {
            width: usable(self.width, MIN_WIDTH, DEFAULT_WIDTH),
            height: usable(self.height, MIN_HEIGHT, DEFAULT_HEIGHT),
            maximized: self.maximized,
        }
    }
}

/// The state file, wherever it belongs on this machine.
///
/// Holds a path and nothing else, for the same reason [`crate::Autostart`]
/// does: what is on disk is the answer, and a copy of it kept here would be a
/// second one to keep in step.
pub struct WindowState {
    path: PathBuf,
}

impl WindowState {
    /// The file under this machine's `$XDG_STATE_HOME`, or `~/.local/state`
    /// when the variable is unset or relative — the fallback the basedir spec
    /// names.
    ///
    /// `None` only when there is no `$HOME` to fall back to either. That is a
    /// session with nowhere to write, and the window opens at its default size
    /// and forgets it, which is exactly how it behaved before this file
    /// existed.
    pub fn locate() -> Option<Self> {
        Some(Self::in_state_home(&state_home()?))
    }

    /// The file under an explicitly given state directory.
    ///
    /// What the tests use, and the same split `paths::config_file_under` and
    /// `Autostart::in_config_home` make for the same reason: a function that
    /// reads this process's environment cannot be pointed at a sandbox, and
    /// setting the variable to do it races every other test in the binary.
    pub fn in_state_home(state_home: &Path) -> Self {
        Self {
            path: state_home.join(SUBDIR).join(FILE),
        }
    }

    /// The file this reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved geometry, or the default.
    ///
    /// **No `Result`, deliberately.** Every way this can fail — no file, no
    /// permission, a directory where the file should be, bytes that are not
    /// text, a value that is not a number — has the same right answer, which is
    /// the size the window has always opened at. Handing the caller an error
    /// would offer it a decision it does not have: there is nothing else it
    /// could do, and nothing worth telling the user, because a window that
    /// opens at its default size is not a fault report.
    pub fn load(&self) -> Geometry {
        // Asked before reading, because the point is not to have read it. A
        // metadata call that fails takes the same path as one that refuses,
        // since neither leaves us anything to parse.
        match fs::metadata(&self.path) {
            Ok(meta) if worth_reading(&meta) => {}
            _ => return Geometry::default(),
        }

        match fs::read_to_string(&self.path) {
            Ok(text) => Geometry::parse(&text).sane(),
            Err(_) => Geometry::default(),
        }
    }

    /// Write the geometry, creating the directory if this is the first time.
    ///
    /// Staged and renamed, so the file is either the last geometry or the one
    /// before it and never half of each. `Err` is for the caller to report or
    /// swallow as it sees fit — the GUI swallows it, because a full disk has
    /// already told the user about itself in louder ways than a window size.
    pub fn save(&self, geometry: &Geometry) -> io::Result<()> {
        let dir = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the state file has no directory")
        })?;
        fs::create_dir_all(dir)?;

        let staging = self.path.with_extension(STAGING);
        fs::write(&staging, geometry.render())?;
        if let Err(err) = fs::rename(&staging, &self.path) {
            let _ = fs::remove_file(&staging);
            return Err(err);
        }
        Ok(())
    }
}

/// Whether what is at the state path is something we will open at all.
///
/// A predicate over the metadata rather than a check inlined into [`
/// WindowState::load`], because the half of it that matters is the half that
/// cannot be demonstrated through `load`: a directory is refused here *and*
/// would fail the read anyway, so a test driving `load` with one passes whether
/// this function asks `is_file` or not. Asking the predicate directly is what
/// pins it — and the case it is really for is one no test can drive at all,
/// since a fifo with no writer does not fail the read, it never returns from
/// it.
fn worth_reading(meta: &fs::Metadata) -> bool {
    meta.is_file() && meta.len() <= LIMIT
}

/// `$XDG_STATE_HOME`, or `~/.local/state`.
fn state_home() -> Option<PathBuf> {
    state_home_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
}

/// The rule, apart from the environment it is usually applied to.
///
/// A relative `$XDG_STATE_HOME` is ignored, as the spec says to and as
/// `autostart.rs` already does for `$XDG_CONFIG_HOME`: it would resolve against
/// this process's working directory, which is wherever the user happened to
/// launch us from — so the window would remember its size per directory.
fn state_home_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg);
        }
    }
    Some(PathBuf::from(home?).join(".local/state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of our own under `/tmp`, named after the test so a failure
    /// leaves something identifiable behind. The same sandbox `autostart.rs`
    /// uses, for the same reason: this crate takes no `tempfile` dependency for
    /// what is four lines.
    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("adguard-ui-window-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create the sandbox");
            Self { dir }
        }

        fn state(&self) -> WindowState {
            WindowState::in_state_home(&self.dir)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A machine that has never run this version has no file, and must get the
    /// size every release before this one opened at — not a zero, which GTK
    /// reads as "use the natural size" and which would open the window at
    /// whatever the widgets happened to ask for.
    #[test]
    fn a_first_launch_has_no_file_and_gets_the_shipped_default() {
        let sandbox = Sandbox::new("first-launch");
        let state = sandbox.state();

        assert!(!state.path().exists());
        assert_eq!(state.load(), Geometry::default());
        assert_eq!(state.load().width, DEFAULT_WIDTH);
        assert_eq!(state.load().height, DEFAULT_HEIGHT);
        assert!(!state.load().maximized);
    }

    /// The round trip the whole feature is: what the window reported comes back
    /// on the next launch, to the pixel, with the maximized flag alongside it.
    #[test]
    fn a_saved_geometry_reads_back_exactly() {
        let sandbox = Sandbox::new("round-trip");
        let state = sandbox.state();

        let saved = Geometry {
            width: 1024,
            height: 640,
            maximized: true,
        };
        state.save(&saved).expect("save");
        assert_eq!(state.load(), saved);
    }

    /// The first save creates the directory, because nothing else in this
    /// application ever writes to `~/.local/state` and on most machines it will
    /// not be there.
    #[test]
    fn the_first_save_creates_the_state_directory() {
        let sandbox = Sandbox::new("mkdir");
        let state = sandbox.state();

        assert!(!state.path().parent().expect("a parent").exists());
        state.save(&Geometry::default()).expect("save");
        assert!(state.path().is_file());
    }

    /// Nothing is left beside the file. A `.part` in a directory this
    /// application owns would be a puzzle for whoever opened it next, and the
    /// staged write is only worth having if it tidies up after itself.
    #[test]
    fn the_write_is_staged_and_leaves_no_part_file_behind() {
        let sandbox = Sandbox::new("staging");
        let state = sandbox.state();

        state.save(&Geometry::default()).expect("save");
        state.save(&Geometry { width: 900, height: 700, maximized: false }).expect("save again");

        let dir = state.path().parent().expect("a parent");
        let names: Vec<_> = fs::read_dir(dir)
            .expect("read the directory")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE)], "{names:?}");
    }

    /// The saver decides whether to write by comparing geometries, which is
    /// only sound if an unchanged geometry would have produced an unchanged
    /// file. A timestamp or a version line in [`Geometry::render`] would break
    /// that quietly — the file would be rewritten on every tick of a resize
    /// that had settled.
    #[test]
    fn an_unchanged_geometry_renders_byte_identically() {
        let geometry = Geometry {
            width: 1280,
            height: 800,
            maximized: false,
        };
        assert_eq!(geometry.render(), geometry.render());
        assert_ne!(geometry.render(), Geometry::default().render());
    }

    /// What we write is what we read: the two halves of the format meet nowhere
    /// else, so a comment line that stopped being a comment, or a key spelled
    /// one way in `render` and another in `parse`, would be a window that
    /// forgets its size with every file on disk looking correct.
    #[test]
    fn what_is_written_is_what_is_parsed() {
        for geometry in [
            Geometry::default(),
            Geometry { width: 1920, height: 1080, maximized: true },
            Geometry { width: MIN_WIDTH, height: MIN_HEIGHT, maximized: false },
        ] {
            assert_eq!(Geometry::parse(&geometry.render()), geometry);
        }
    }

    /// The case that rules out a document format. A file cut off mid-write
    /// keeps the keys that reached the disk; a YAML or JSON parser would reject
    /// the whole thing and lose the width sitting there intact.
    #[test]
    fn a_truncated_file_keeps_the_keys_that_survived_it() {
        let full = Geometry { width: 1024, height: 768, maximized: false }.render();
        let cut = &full[..full.find("height").expect("a height line")];

        let parsed = Geometry::parse(cut);
        assert_eq!(parsed.width, 1024);
        assert_eq!(parsed.height, DEFAULT_HEIGHT);
    }

    /// One bad value costs one dimension. A width that is not a number says
    /// nothing about the height beside it, and defaulting both would throw away
    /// the one thing the file still got right.
    #[test]
    fn a_garbage_value_falls_back_for_that_key_alone() {
        let parsed = Geometry::parse("width = eight hundred\nheight = 900\n");
        assert_eq!(parsed.width, DEFAULT_WIDTH);
        assert_eq!(parsed.height, 900);
    }

    /// A file from a later version is still a file we can read a width out of.
    /// This is what makes adding a key later a decision about that key, rather
    /// than one that strands everybody who downgrades.
    #[test]
    fn keys_from_a_later_version_are_ignored_rather_than_fatal() {
        let parsed = Geometry::parse("width = 1000\nmonitor = HDMI-1\nopacity = 0.9\nheight = 800\n");
        assert_eq!(parsed.width, 1000);
        assert_eq!(parsed.height, 800);
    }

    /// The file explains itself in comments and someone will eventually edit it
    /// by hand, so the parser has to survive the shapes an editor leaves.
    #[test]
    fn comments_blank_lines_and_stray_whitespace_are_tolerated() {
        let parsed = Geometry::parse(
            "# a comment\n\
             \n\
             \twidth=1111   \n\
             # height = 1 — commented out, so not a height\n\
                height    =    999\n\
             maximized = TRUE\n\
             a line with no equals sign at all\n",
        );
        assert_eq!(
            parsed,
            Geometry { width: 1111, height: 999, maximized: true }
        );
    }

    /// Last one wins, which is the only rule that makes an appended line a way
    /// to change the file rather than a coin toss.
    #[test]
    fn a_repeated_key_takes_its_last_value() {
        assert_eq!(Geometry::parse("width = 800\nwidth = 1200\n").width, 1200);
    }

    /// Zero is the value that matters here, because GTK does not treat it as a
    /// small window: a default size of 0 means "ask the widgets", so a file
    /// carrying one would open the window at whatever the pages happened to
    /// request. Negative is the same shape of nonsense, and a number too large
    /// for `i32` falls back rather than wrapping to one that looks plausible.
    #[test]
    fn a_size_that_is_not_a_size_falls_back_to_the_default() {
        for text in [
            "width = 0\nheight = 0\n",
            "width = -1200\nheight = -800\n",
            "width = 99999999999999999999\nheight = 99999999999999999999\n",
            "width = 40000\nheight = 40000\n",
        ] {
            let sane = Geometry::parse(text).sane();
            assert_eq!(sane.width, DEFAULT_WIDTH, "{text}");
            assert_eq!(sane.height, DEFAULT_HEIGHT, "{text}");
        }
    }

    /// A file that is not ours is not read into memory to find that out.
    #[test]
    fn an_absurdly_large_file_is_not_read_into_memory() {
        let sandbox = Sandbox::new("too-big");
        let state = sandbox.state();

        state.save(&Geometry::default()).expect("save");
        let padding = "# ".to_string() + &"x".repeat(LIMIT as usize);
        fs::write(state.path(), format!("width = 1234\n{padding}\n")).expect("write a large file");

        assert_eq!(state.load(), Geometry::default());
    }

    /// Anything that is not a plain file is refused **before** it is opened,
    /// and the size is not what refuses it.
    ///
    /// The case this exists for is a fifo or a `/dev/zero` symlink at the state
    /// path, whose read either never returns or never stops — and it is exactly
    /// the case a size check cannot catch, because `metadata` reports both as
    /// length zero and so as comfortably under [`LIMIT`]. Neither can be driven
    /// through `load` in a test: the failure is a hang, not a wrong answer.
    ///
    /// So the predicate is asserted directly, against a directory standing in
    /// as the one non-file every machine can make. Driving `load` with that
    /// directory would prove nothing — the read would fail on its own and
    /// return the default whether `is_file` were asked or not — which is the
    /// whole reason [`worth_reading`] is a function rather than a condition
    /// inside the match.
    #[test]
    fn something_that_is_not_a_file_is_refused_before_it_is_read() {
        let sandbox = Sandbox::new("not-a-file");
        let state = sandbox.state();
        fs::create_dir_all(state.path()).expect("a directory where the file goes");

        let directory = fs::metadata(state.path()).expect("metadata");
        assert!(directory.len() <= LIMIT, "the size check alone would let this through");
        assert!(!worth_reading(&directory));

        // And the file it is standing in for is still read, so the gate has not
        // simply refused everything.
        fs::remove_dir(state.path()).expect("take the directory back out of the way");
        state.save(&Geometry::default()).expect("save");
        assert!(worth_reading(&fs::metadata(state.path()).expect("metadata")));
        assert_eq!(state.load(), Geometry::default());
    }

    /// A flag that is neither `true` nor `false` reads as not maximized, which
    /// is the state a user can always get out of with one click. Guessing the
    /// other way would open a window filling the screen because a byte was
    /// mangled.
    #[test]
    fn an_unreadable_maximized_flag_reads_as_not_maximized() {
        assert!(Geometry::parse("maximized = true\n").maximized);
        assert!(Geometry::parse("maximized = TRUE\n").maximized);
        assert!(!Geometry::parse("maximized = false\n").maximized);
        assert!(!Geometry::parse("maximized = yes\n").maximized);
        assert!(!Geometry::parse("maximized =\n").maximized);
        assert!(!Geometry::parse("width = 900\n").maximized);
    }

    /// The laptop that came home from a wide desk. The size is not corrupt —
    /// it was right yesterday — so it is cut to the display rather than thrown
    /// away for the default.
    #[test]
    fn a_size_larger_than_every_monitor_is_shrunk_to_the_largest_one() {
        let saved = Geometry { width: 3440, height: 1440, maximized: false };
        let fitted = saved.fitted(&[(1536, 960)]);

        assert_eq!(fitted.width, 1536);
        assert_eq!(fitted.height, 960);
    }

    /// Two displays, and the window is clamped to what the *larger* can show:
    /// which monitor it opens on is the compositor's decision, and shrinking to
    /// the smaller would narrow a window that fits the screen it appears on.
    #[test]
    fn the_clamp_is_against_the_largest_monitor_not_the_smallest() {
        let saved = Geometry { width: 2000, height: 1200, maximized: false };
        let fitted = saved.fitted(&[(1536, 960), (3440, 1440)]);

        assert_eq!(fitted, saved);
    }

    /// A size that fits is left alone, and a display list that could not be
    /// read is not evidence against the saved size.
    #[test]
    fn a_size_that_fits_is_left_alone_and_no_monitors_means_no_clamp() {
        let saved = Geometry { width: 1000, height: 700, maximized: true };

        assert_eq!(saved.fitted(&[(1536, 960)]), saved);
        assert_eq!(saved.fitted(&[]), saved);
    }

    /// A monitor smaller than this window's own floor is not a reason to open a
    /// 200-pixel window. The clamp lands under the floor, so the default comes
    /// back instead and GTK shrinks it to whatever really fits.
    #[test]
    fn a_clamp_that_lands_under_the_floor_falls_back_to_the_default() {
        let saved = Geometry { width: 1000, height: 700, maximized: false };
        let fitted = saved.fitted(&[(160, 100)]);

        assert_eq!(fitted.width, DEFAULT_WIDTH);
        assert_eq!(fitted.height, DEFAULT_HEIGHT);
    }

    /// The spec's rule, and the reason for it: a relative `$XDG_STATE_HOME`
    /// resolves against the working directory, so honouring one would give the
    /// window a different remembered size per directory it was launched from.
    #[test]
    fn a_relative_xdg_state_home_is_ignored_as_the_spec_says() {
        let resolved = state_home_from(Some("relative/state".into()), Some("/home/anna".into()));
        assert_eq!(resolved, Some(PathBuf::from("/home/anna/.local/state")));
    }

    /// Unset falls back to the spec's default, and no `$HOME` at all means
    /// there is nowhere to save — which the window treats as "opens at its
    /// default size and forgets", not as an error.
    #[test]
    fn the_state_home_falls_back_the_way_the_spec_says() {
        assert_eq!(
            state_home_from(Some("/state".into()), Some("/home/anna".into())),
            Some(PathBuf::from("/state"))
        );
        assert_eq!(
            state_home_from(None, Some("/home/anna".into())),
            Some(PathBuf::from("/home/anna/.local/state"))
        );
        assert_eq!(state_home_from(None, None), None);
    }

    /// The path is the one the module header promises, spelled out here because
    /// it is a file on the user's disk forever once it ships.
    #[test]
    fn the_file_sits_in_our_own_directory_under_the_state_home() {
        let state = WindowState::in_state_home(Path::new("/home/anna/.local/state"));
        assert_eq!(
            state.path(),
            Path::new("/home/anna/.local/state/adguard-ui/window.state")
        );
    }

    /// A directory that cannot be written to is an error and not a panic: the
    /// GUI swallows it and the window still works, and the one thing it must
    /// not do is take the application down over a window size.
    ///
    /// Skipped under root, where a 0o500 directory is not enforced at all — the
    /// same allowance `helper.rs` makes, and for the same reason: CI is a
    /// container and a container runs as root.
    #[test]
    fn an_unwritable_directory_is_an_error_and_not_a_panic() {
        // SAFETY: `geteuid` reads a process attribute, takes no arguments and
        // cannot fail. It is `unsafe` only because it is an extern fn.
        if unsafe { libc::geteuid() == 0 } {
            eprintln!("skipping: running as root, which is not refused by a read-only directory");
            return;
        }

        use std::os::unix::fs::PermissionsExt;
        let sandbox = Sandbox::new("read-only");
        let state = sandbox.state();
        state.save(&Geometry::default()).expect("save while it is still writable");

        let dir = state.path().parent().expect("a parent").to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("make it read-only");
        let refused = state.save(&Geometry { width: 1000, height: 700, maximized: false });
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("put it back");

        assert!(refused.is_err(), "a read-only directory should refuse the write");
        // And the geometry that was there before is still readable, which is
        // the point of staging the write rather than truncating in place.
        assert_eq!(state.load(), Geometry::default());
    }
}
