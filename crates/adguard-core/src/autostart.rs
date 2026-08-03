//! The login entry: `~/.config/autostart/io.github.dominik-najberg.AdGuardUI.desktop`.
//!
//! The switch that writes it is on the Advanced page; this is the file half.
//! Nothing here knows what the entry runs — the flag that starts the
//! application without a window belongs to the binary that parses it, so the
//! caller composes the `Exec` line and this module writes, reads and removes the
//! file around it.
//!
//! **The name is the one shipped in `data/autostart/`, deliberately.** That
//! example entry is installed by `packaging/tarball.sh --autostart` and by the
//! instructions in `building.md` §4, and an entry written here under any other
//! name would be a *second* login entry beside it: two launches at login, and a
//! switch that reads "off" while the tray comes up anyway. Sharing the name
//! makes the switch and the shipped file the same fact, so turning it off from
//! the application removes the entry the packaging installed.
//!
//! **Absence is what "off" means**, which is why turning it off deletes the file
//! rather than flagging it. `X-GNOME-Autostart-enabled=false` is what a
//! startup-applications editor writes and it is honoured on the way *in* — an
//! entry disabled out there reads as off here — but writing it ourselves would
//! leave a dead `Exec` behind naming a binary that may since have moved.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The entry's filename, which is the application id — see the module header
/// for why it may not be anything else.
pub const ENTRY: &str = "io.github.dominik-najberg.AdGuardUI.desktop";

/// Where the XDG autostart spec puts login entries, under `$XDG_CONFIG_HOME`.
const SUBDIR: &str = "autostart";

/// The half-written file, staged beside the real one so an interrupted write
/// cannot leave a truncated entry where a session manager would find it.
///
/// The suffix deliberately does **not** end in `.desktop`: a leftover
/// `…desktop.part` in this directory is ignored by every session manager,
/// whereas a leftover `…part.desktop` would be a second entry.
const STAGING: &str = "desktop.part";

/// The login entry, wherever it belongs on this machine.
///
/// Holds a path and nothing else — every question is answered by reading the
/// file, never by remembering what was last written. The switch on the Advanced
/// page can be out of date the moment a startup-applications editor is opened,
/// and cached state would be exactly wrong at the one moment the user looks.
pub struct Autostart {
    path: PathBuf,
}

impl Autostart {
    /// The entry under this machine's `$XDG_CONFIG_HOME`, or `~/.config` when
    /// the variable is unset or relative — the fallback the XDG basedir spec
    /// names.
    ///
    /// `None` only when there is no `$HOME` to fall back to either, which is a
    /// session with nowhere to write a login entry at all.
    pub fn locate() -> Option<Self> {
        Some(Self::in_config_home(&config_home()?))
    }

    /// The entry under an explicitly given configuration directory.
    ///
    /// What the tests use, and the same split `paths::config_file_under` makes
    /// for the same reason: a check that reads this process's environment cannot
    /// be pointed at a sandbox.
    pub fn in_config_home(config_home: &Path) -> Self {
        Self {
            path: config_home.join(SUBDIR).join(ENTRY),
        }
    }

    /// The file this reads and writes. Shown to the user, so they can find it
    /// without being told where to look.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether logging in would actually start the application.
    ///
    /// Not merely whether the file is there: an entry a startup-applications
    /// editor has disabled, or one with no `Exec` to run, is on disk and starts
    /// nothing. Reporting either as "on" would leave a switch claiming a login
    /// start the user does not have.
    ///
    /// A missing file is `Ok(false)` — the ordinary off state, not a failure.
    /// `Err` is a file that is there and could not be read, which the switch
    /// renders as unavailable rather than guessing in either direction.
    pub fn is_enabled(&self) -> io::Result<bool> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(starts_something(&text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Write the entry, so `exec` runs at login.
    ///
    /// `exec` is a desktop-entry `Exec` value, quoted by [`quote_exec`] if it
    /// names a path that needs it. Rewrites the file wholesale rather than
    /// patching it, which is what clears the `X-GNOME-Autostart-enabled=false`
    /// an editor may have left behind — switching it on out of that state has to
    /// mean the entry runs again.
    pub fn enable(&self, exec: &str) -> io::Result<()> {
        let dir = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the entry has no directory")
        })?;
        fs::create_dir_all(dir)?;

        let staging = self.path.with_extension(STAGING);
        fs::write(&staging, entry(exec))?;
        if let Err(err) = fs::rename(&staging, &self.path) {
            // Leaving the staging file would be harmless — nothing starts a
            // `.desktop.part` — but it would also be a puzzle for whoever opened
            // this directory next.
            let _ = fs::remove_file(&staging);
            return Err(err);
        }
        Ok(())
    }

    /// Remove the entry. Already absent is success, not an error: the caller
    /// asked for a state, and that state is the one on disk.
    pub fn disable(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
///
/// A relative `$XDG_CONFIG_HOME` is ignored, as the spec says to: it would
/// resolve against this process's working directory, which is wherever the user
/// happened to launch us from.
fn config_home() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg);
        }
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".config"))
}

/// The entry's text, around one `Exec` line.
///
/// Field by field this is `data/autostart/`'s example, minus the commentary
/// that file carries for whoever reads it in the repository and plus one line
/// saying where this copy came from — the user did not put it here by hand and
/// should not have to wonder what did.
fn entry(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         # Written by AdGuard UI's \"Start at login\" switch. Deleting this file\n\
         # turns it off; so does the switch.\n\
         Type=Application\n\
         Name=AdGuard UI (tray)\n\
         Comment=Keep the AdGuard tray icon available from login\n\
         Exec={exec}\n\
         Icon=io.github.dominik-najberg.AdGuardUI\n\
         Terminal=false\n\
         # Nothing appears when this runs, so a startup notification would leave\n\
         # the cursor spinning until the shell timed it out.\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// A path as a desktop-entry `Exec` value.
///
/// Three rules from the spec, and each one is a way a perfectly ordinary home
/// directory breaks the entry:
///
/// - `%` introduces a field code, so a literal one is doubled. `%f` in a path
///   would otherwise be replaced with a filename — usually nothing at all,
///   leaving a command that does not exist.
/// - A value containing whitespace has to be quoted, or `/opt/My Apps/adguard-ui`
///   is read as a program plus an argument.
/// - Inside quotes, `"`, `` ` ``, `$` and `\` are escaped, because the spec has
///   the value read as if by a shell.
///
/// Paths needing none of that are returned unquoted, which is every ordinary
/// install and keeps the entry readable.
pub fn quote_exec(path: &Path) -> String {
    let path = path.to_string_lossy().replace('%', "%%");
    let plain = !path.is_empty()
        && !path.contains(|c: char| c.is_whitespace() || "\"`$\\'><~|&;*?#()".contains(c));
    if plain {
        return path;
    }

    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Whether this entry's text would start anything at login.
///
/// Keys are only read inside `[Desktop Entry]`. A `.desktop` file may carry
/// action groups after it — `[Desktop Action new-window]` — and a `Hidden=true`
/// down there says nothing about the entry itself.
fn starts_something(text: &str) -> bool {
    let mut in_entry = false;
    let mut seen_entry = false;
    let mut has_exec = false;
    let mut enabled = true;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            seen_entry |= in_entry;
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Exec" => has_exec = !value.is_empty(),
            // The spec's own word for "treat this file as deleted".
            "Hidden" => enabled &= !value.eq_ignore_ascii_case("true"),
            // What GNOME's startup-applications editor flips instead.
            "X-GNOME-Autostart-enabled" => enabled &= !value.eq_ignore_ascii_case("false"),
            _ => {}
        }
    }

    seen_entry && has_exec && enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of our own under `/tmp`, named after the test so a failure
    /// leaves something identifiable behind.
    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("adguard-ui-autostart-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create the sandbox");
            Self { dir }
        }

        fn autostart(&self) -> Autostart {
            Autostart::in_config_home(&self.dir)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// The round trip the switch makes, against a real directory: nothing there
    /// reads as off, `enable` creates the parent directory it needs, and the
    /// file it leaves reads back as on.
    #[test]
    fn enabling_writes_an_entry_that_reads_back_as_enabled() {
        let sandbox = Sandbox::new("round-trip");
        let autostart = sandbox.autostart();

        assert!(!autostart.is_enabled().expect("a missing entry is not an error"));
        assert!(!autostart.path().exists());

        autostart.enable("/usr/bin/adguard-ui --background").expect("enable");
        assert!(autostart.is_enabled().expect("read back"));

        let text = fs::read_to_string(autostart.path()).expect("read the entry");
        assert!(text.contains("Exec=/usr/bin/adguard-ui --background"), "{text}");
    }

    /// Off means gone, and asking twice is not an error — the switch reports a
    /// state, and a user who removed the file by hand between the two clicks
    /// has already got what they asked for.
    #[test]
    fn disabling_removes_the_entry_and_is_idempotent() {
        let sandbox = Sandbox::new("disable");
        let autostart = sandbox.autostart();

        autostart.enable("adguard-ui --background").expect("enable");
        autostart.disable().expect("disable");
        assert!(!autostart.path().exists());
        assert!(!autostart.is_enabled().expect("a missing entry is not an error"));

        autostart.disable().expect("disabling an absent entry is success");
    }

    /// Switching it back on has to *undo* a disable made elsewhere, which is
    /// the one thing a patch-in-place implementation would get wrong: the entry
    /// is rewritten, so the editor's flag goes with it.
    #[test]
    fn enabling_clears_a_flag_a_startup_editor_left() {
        let sandbox = Sandbox::new("re-enable");
        let autostart = sandbox.autostart();

        autostart.enable("adguard-ui --background").expect("enable");
        let disabled = fs::read_to_string(autostart.path())
            .expect("read")
            .replace("X-GNOME-Autostart-enabled=true", "X-GNOME-Autostart-enabled=false");
        fs::write(autostart.path(), disabled).expect("write the editor's version");
        assert!(!autostart.is_enabled().expect("read back"));

        autostart.enable("adguard-ui --background").expect("re-enable");
        assert!(autostart.is_enabled().expect("read back"));
    }

    /// The two ways an entry on disk starts nothing, and the two shapes that
    /// are simply not an entry. Each one would otherwise render as a switch
    /// promising a login start that does not happen.
    #[test]
    fn an_entry_that_starts_nothing_reads_as_off() {
        let live = "[Desktop Entry]\nType=Application\nExec=adguard-ui --background\n";
        assert!(starts_something(live));

        assert!(!starts_something(&live.replace(
            "Type=Application",
            "X-GNOME-Autostart-enabled=false"
        )));
        assert!(!starts_something(&live.replace("Type=Application", "Hidden=true")));
        assert!(!starts_something(&live.replace("Exec=adguard-ui --background", "Exec=")));
        assert!(
            !starts_something("Exec=adguard-ui --background\n"),
            "keys before any group header are not in the entry"
        );
    }

    /// `Hidden` in an action group is about the action, not the entry — and a
    /// commented-out flag is not a flag at all.
    #[test]
    fn only_the_desktop_entry_group_is_read() {
        assert!(starts_something(
            "[Desktop Entry]\n\
             Exec=adguard-ui --background\n\
             [Desktop Action new-window]\n\
             Hidden=true\n"
        ));
        assert!(starts_something(
            "[Desktop Entry]\n\
             # Hidden=true\n\
             Exec=adguard-ui --background\n"
        ));
    }

    /// What the switch actually writes reads back as on, which is the pairing
    /// the two halves of this module rest on.
    #[test]
    fn the_entry_this_module_writes_is_one_it_calls_enabled() {
        assert!(starts_something(&entry("adguard-ui --background")));
    }

    /// An ordinary install is left alone; a home directory with a space in it,
    /// or one of the characters the spec reserves, is quoted and escaped.
    #[test]
    fn exec_values_are_quoted_only_where_the_spec_requires_it() {
        assert_eq!(quote_exec(Path::new("/usr/bin/adguard-ui")), "/usr/bin/adguard-ui");
        assert_eq!(
            quote_exec(Path::new("/home/anna maria/.local/bin/adguard-ui")),
            "\"/home/anna maria/.local/bin/adguard-ui\""
        );
        assert_eq!(
            quote_exec(Path::new("/opt/50%/adguard-ui")),
            "/opt/50%%/adguard-ui"
        );
        assert_eq!(
            quote_exec(Path::new("/opt/$HOME`x\\y/adguard-ui")),
            "\"/opt/\\$HOME\\`x\\\\y/adguard-ui\""
        );
    }
}
