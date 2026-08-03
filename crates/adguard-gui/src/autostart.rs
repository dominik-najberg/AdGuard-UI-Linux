//! The *Start at login* switch, at the foot of the Advanced page.
//!
//! One switch over one file: `~/.config/autostart/…AdGuardUI.desktop`, written
//! and removed by [`adguard_core::Autostart`]. It is the only control in this
//! application that changes nothing in `proxy.yaml` and runs no CLI command —
//! it arrives here the way the backup buttons and the root-helper rows do,
//! built in this module and added to that page during its build.
//!
//! It keeps the discipline of the settings it sits under even so. **The file
//! decides**: the switch is painted from a fresh read after every write, never
//! from what was just asked for, so an entry a startup-applications editor
//! disabled behind our back reads as off and a write that failed leaves the
//! switch where the disk says it should be.
//!
//! **What it starts is `--background`, not a second flag.** The request that
//! prompted this asked for a `--silent`/`--quiet` switch so the window stays
//! closed at login; that flag already exists under the name the autostart entry
//! in `data/autostart/` has been running since v1.0, and a third spelling of it
//! would be one more thing to keep in step with `HANDLES_COMMAND_LINE` for no
//! gain (`architecture.md` §4).
//!
//! **The one caveat it carries is the tray.** `--background` presents no window,
//! so a session with no StatusNotifierItem host leaves the application with
//! nothing on screen and no way to be reached — it says so and exits 1, which
//! is the single place a tray that will not register is fatal. That is invisible
//! at login, where stderr goes to the journal, so the row says it *here* while
//! the user is deciding, and says it as a fact about this session rather than a
//! general warning: the window this switch is in already knows whether the tray
//! registered.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{autostart::quote_exec, Autostart};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, BACKGROUND};

/// The `[[bin]]` name, for the entry we write when this process cannot say
/// where its own binary is. Bare, so it is resolved against the session's
/// `$PATH` — which is the same bet `data/autostart/`'s example entry makes.
const BINARY: &str = "adguard-ui";

/// What the switch does, stated once where the user is deciding.
///
/// The last sentence says what this switch does **not** do, and it is worded
/// as a fact about this application rather than about AdGuard. Whether the
/// proxy comes up at login is AdGuard's own arrangement — on the reference
/// machine an enabled `adguard-cli.service` user unit does it — and this
/// application neither installs nor reads that, so "it starts either way" would
/// be a reassurance we cannot check and would be false on a machine without
/// one. What *is* checkable is that the entry runs `adguard-ui --background`,
/// which never calls `start`.
const DESCRIPTION: &str = "AdGuard UI starts with only its tray icon, so nothing opens on screen \
                           at login, and the window is one click away in the tray menu. It \
                           neither starts nor stops AdGuard's protection: what runs the proxy at \
                           login is AdGuard's own arrangement, not this.";

/// Shown when the tray could not be registered in *this* session, which makes
/// a background start a process with nowhere to appear.
///
/// Appended to the **group description** rather than to the row's subtitle, the
/// way the listen-address group carries its credential requirement: the row's
/// subtitle already names a path, and a path plus three sentences is four lines
/// in a row that shows three — measured, with the last of them being this
/// caveat, truncated. It belongs in the description on its own merits too. It
/// is a standing condition of the session rather than a fact about the entry.
const NO_TRAY: &str = "No tray icon could be registered in this session, so a background start \
                       would have nothing on screen and would exit instead — GNOME needs an \
                       AppIndicator extension for one.";

pub struct AutostartView {
    group: adw::PreferencesGroup,
    row: adw::SwitchRow,
    caveat: gtk::Image,
    toasts: adw::ToastOverlay,
    /// The entry itself. `None` in a session with neither `$XDG_CONFIG_HOME`
    /// nor `$HOME`, where there is nowhere to put a login entry at all.
    entry: Option<Autostart>,
    /// Whether a tray registered in this session. Assumed until told otherwise:
    /// the window is built before the tray is, so the alternative would be a
    /// warning that flashes up on every launch and then withdraws itself.
    tray: Cell<bool>,
    /// The last state rendered, so a re-check that found nothing new does not
    /// move a switch under the user's pointer.
    painted: RefCell<Option<String>>,
    /// Set while the switch is being moved to match the file, so the handler can
    /// tell that from a click.
    reconciling: Cell<bool>,
}

impl AutostartView {
    pub fn new(toasts: &adw::ToastOverlay) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("Start at login")
            .description(DESCRIPTION)
            .build();

        let row = adw::SwitchRow::new();
        row.set_use_markup(false);
        row.set_title("Start AdGuard UI at login");
        row.set_subtitle_lines(3);

        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(false);
        row.add_prefix(&caveat);
        group.add(&row);

        let this = Rc::new(Self {
            group,
            row,
            caveat,
            toasts: toasts.clone(),
            entry: Autostart::locate(),
            tray: Cell::new(true),
            painted: RefCell::new(None),
            reconciling: Cell::new(false),
        });

        this.row.connect_active_notify({
            let view = Rc::downgrade(&this);
            move |row| {
                let Some(view) = view.upgrade() else { return };
                if view.reconciling.get() {
                    return; // our own paint, not a click
                }
                view.switched(row.is_active());
            }
        });

        this.paint();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Tell the row whether a tray registered in this session.
    ///
    /// Called from where that is known, which is not here: the tray is
    /// registered against the window, after the pages are built.
    pub fn set_tray_available(&self, available: bool) {
        if self.tray.replace(available) != available {
            self.paint();
        }
    }

    /// Re-read the entry and render it.
    ///
    /// Public because the window calls it when it regains focus, for the same
    /// reason it re-checks the root helper there: the user's other way to change
    /// this is a startup-applications editor in another window, and a switch
    /// that disagreed with it would be the more confusing of the two
    /// (`architecture.md` §6). One `stat` and a short read.
    pub fn paint(&self) {
        let Some(entry) = &self.entry else {
            // Nowhere to write. Insensitive and explicit, the way an unreadable
            // `proxy.yaml` key is: a switch that silently did nothing would be
            // worse than one that says why it cannot.
            self.row.set_sensitive(false);
            self.caveat.set_visible(true);
            self.row.set_subtitle(
                "Unavailable — neither XDG_CONFIG_HOME nor HOME is set, so there is nowhere \
                 to put a login entry",
            );
            return;
        };

        let state = entry.is_enabled();
        let snapshot = format!("{state:?} tray={}", self.tray.get());
        if self.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return;
        }
        self.painted.replace(Some(snapshot));

        // Before the match, so it holds in every branch: an entry that has
        // become unreadable does not make the session grow a tray.
        self.group.set_description(Some(&if self.tray.get() {
            DESCRIPTION.to_owned()
        } else {
            format!("{DESCRIPTION} {NO_TRAY}")
        }));

        let path = abbreviate(entry.path());
        match state {
            Ok(on) => {
                self.row.set_sensitive(true);
                self.reconciling.set(true);
                self.row.set_active(on);
                self.reconciling.set(false);

                // The row says where the entry is; the group says what is wrong
                // with this session. The caveat marker goes on the row even so,
                // because that is what the eye lands on, and it applies
                // whichever way the switch is pointing — switched on it is a
                // login start that will not survive, switched off it is a
                // reason not to.
                self.caveat.set_visible(!self.tray.get());
                self.row.set_subtitle(&if on {
                    format!("Written to {path}")
                } else {
                    format!("Writes a desktop entry to {path}")
                });
            }
            // There and unreadable, which is neither on nor off. Left
            // insensitive rather than guessed at, because both guesses are
            // wrong in a way the user would only find out at the next login.
            Err(err) => {
                self.row.set_sensitive(false);
                self.caveat.set_visible(true);
                self.row
                    .set_subtitle(&format!("Unavailable — {path} could not be read ({err})"));
            }
        }
    }

    /// The switch was clicked.
    fn switched(&self, on: bool) {
        let Some(entry) = &self.entry else { return };

        // A single small write into the user's own configuration directory, so
        // it stays on the main loop where the root-helper `stat` and the
        // certificate read already are. The things this application moves to a
        // worker are the ones that measure in tens of milliseconds — spawning
        // `adguard-cli`, opening SQLite — and this is not one of them.
        let outcome = if on {
            entry.enable(&command())
        } else {
            entry.disable()
        };

        if let Err(err) = outcome {
            self.toasts.add_toast(toast(&format!(
                "Could not {} {} — {err}",
                if on { "write" } else { "remove" },
                abbreviate(entry.path())
            )));
        }

        // Forced, and from the file rather than from `on`: a write that failed
        // has left the switch showing a state the disk does not have, and this
        // is what puts it back. The same rule as every row on the page above.
        self.painted.replace(None);
        self.paint();
    }
}

/// The `Exec` line the entry runs.
///
/// This binary's own path, not the bare name: a session's `$PATH` need not
/// carry `~/.local/bin`, which is where `building.md` §4 installs to, and an
/// entry naming a binary the session manager cannot find fails at login with
/// nothing on screen to say so. Falling back to the bare name keeps the entry
/// plausible if the path cannot be read.
fn command() -> String {
    match std::env::current_exe() {
        Ok(exe) => format!("{} --{BACKGROUND}", quote_exec(&exe)),
        Err(_) => format!("{BINARY} --{BACKGROUND}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry the switch writes and the example in `data/autostart/` are the
    /// same file under the same name, so they have to start the application the
    /// same way. They are edited in different places by different people — one
    /// is Rust, the other is packaging — and nothing but this would notice them
    /// drifting apart.
    #[test]
    fn the_switch_starts_the_app_the_way_the_shipped_entry_does() {
        const SHIPPED: &str =
            include_str!("../../../data/autostart/io.github.dominik-najberg.AdGuardUI.desktop");

        let exec = SHIPPED
            .lines()
            .find_map(|line| line.strip_prefix("Exec="))
            .expect("the shipped entry has an Exec line");
        assert!(
            exec.ends_with(&format!("--{BACKGROUND}")),
            "the shipped entry runs {exec:?}, which is not --{BACKGROUND}"
        );
        assert!(
            command().ends_with(&format!("--{BACKGROUND}")),
            "the switch writes {:?}",
            command()
        );
    }

    /// The filename is the application id, which is also the `.desktop` the
    /// launcher installs and the `StartupWMClass` GNOME groups on. Written out
    /// in `adguard-core`, which cannot see this constant.
    #[test]
    fn the_entry_is_named_after_the_application_id() {
        assert_eq!(
            adguard_core::autostart::ENTRY,
            format!("{}.desktop", crate::APP_ID)
        );
    }
}
