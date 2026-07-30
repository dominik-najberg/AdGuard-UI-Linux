//! The Protection page: the top-level filtering switches.
//!
//! State is read from `proxy.yaml` and written through `adguard-cli config
//! set` — the two directions never cross (see `docs/architecture.md` §3).
//!
//! Every toggle follows act -> re-read -> reconcile, and here that matters
//! more than usual. `config set` prints `Config has been updated` even when it
//! declined to make the change, so the confirmation is not evidence; only the
//! file is. Each flip therefore re-reads `proxy.yaml` and re-renders *every*
//! row from it, which costs one 9 KB read and keeps the page honest even when
//! a write moves more than the key we asked about.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{Applied, Cli, Config, Toggle};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, worker};

/// A rendered switch and the caveat icon that belongs to it.
struct Row {
    toggle: Toggle,
    switch: adw::SwitchRow,
    /// Shown when the setting is on paper but inert in practice — currently
    /// only DNS filtering without a listen port.
    caveat: gtk::Image,
    /// A write for this row is in flight.
    ///
    /// Reconciling reads the whole file and re-renders every row, which is
    /// right when a write moves more than the key it was given — but the
    /// snapshot is only as fresh as the moment that write finished. Without
    /// this flag, flipping a second switch while the first is still settling
    /// lets the first one's stale snapshot revert it, and re-enable it
    /// mid-flight. Rows with a pending write are left alone; their own
    /// `settle` renders them.
    pending: Cell<bool>,
}

pub struct ProtectionPage {
    /// Swapped wholesale between spinner, error and content. Once content is
    /// up, reconciling patches the existing rows in place rather than
    /// rebuilding, so a flip never moves the page under the pointer.
    bin: adw::Bin,
    cli: Cli,
    toasts: adw::ToastOverlay,
    rows: RefCell<Vec<Row>>,
    /// Set while we write switch states ourselves, so the `active` handler can
    /// tell a user's click from our own reconcile. Property notifications are
    /// synchronous, so a plain flag around the write is enough.
    reconciling: Cell<bool>,

    /// Notified with every fresh reading of `proxy.yaml`.
    ///
    /// The tray shows these same six toggles, and this is how it learns their
    /// state — it holds no `Cli` and reads no config of its own.
    observer: RefCell<Option<Box<dyn Fn(&Config)>>>,
}

impl ProtectionPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            cli,
            toasts,
            rows: RefCell::new(Vec::new()),
            reconciling: Cell::new(false),
            observer: RefCell::new(None),
        });
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.bin
    }

    /// Report every reading of `proxy.yaml` to `observer` — the tray's source
    /// of toggle state.
    pub fn connect_config(&self, observer: impl Fn(&Config) + 'static) {
        self.observer.replace(Some(Box::new(observer)));
    }

    /// Flip a toggle on the tray's behalf.
    ///
    /// Deliberately the *same* entry point a switch click uses, rather than a
    /// second write path: the row goes insensitive, the write is verified
    /// against the file, and the switch on this page ends up showing what the
    /// tray asked for. Two writers that could disagree is exactly what merging
    /// the tray into this process was meant to prevent.
    pub fn request(self: &Rc<Self>, toggle: Toggle, on: bool) {
        self.toggle(toggle, on);
    }

    /// Re-read `proxy.yaml` and rebuild the page.
    ///
    /// For the initial load and the explicit refresh — the latter is how an
    /// edit made in a terminal, or by `adguard-cli configure`, reaches the UI.
    pub fn reload(self: &Rc<Self>) {
        self.bin.set_child(Some(&loading_view()));

        let this = self.clone();
        worker::run(
            || Config::load().map_err(|err| err.to_string()),
            move |result: Result<Config, String>| match result {
                Ok(config) => {
                    let page = this.build(&config);
                    this.bin.set_child(Some(&page));
                }
                Err(err) => this.bin.set_child(Some(&error_view(&err))),
            },
        );
    }

    fn build(self: &Rc<Self>, config: &Config) -> adw::PreferencesPage {
        self.rows.borrow_mut().clear();

        let group = adw::PreferencesGroup::builder()
            .title("Protection")
            .description(format!(
                "Read from {}. Changes are written with `adguard-cli config set`.",
                abbreviate(config.path())
            ))
            .build();

        for toggle in Toggle::ALL {
            group.add(&self.row(toggle));
        }

        let page = adw::PreferencesPage::new();
        page.add(&group);
        self.apply(config);
        page
    }

    fn row(self: &Rc<Self>, toggle: Toggle) -> adw::SwitchRow {
        let switch = adw::SwitchRow::new();

        // Our own literals carry no markup, but `AdwPreferencesRow:use-markup`
        // defaults to true and the label renders as the title is assigned — so
        // the property goes first here for the same reason it does on the
        // filter rows, and keeps the convention uniform across the app.
        switch.set_use_markup(false);
        switch.set_title(toggle.title());
        switch.set_subtitle(toggle.description());
        switch.set_subtitle_lines(2);

        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(false);
        switch.add_prefix(&caveat);

        let this = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.reconciling.get() {
                return; // our own write, not a click
            }
            this.toggle(toggle, switch.is_active());
        });

        self.rows.borrow_mut().push(Row {
            toggle,
            switch: switch.clone(),
            caveat,
            pending: Cell::new(false),
        });

        switch
    }

    /// Render every row from one reading of the file.
    ///
    /// Done for the whole page rather than the one row that was clicked: a
    /// `config set` can touch more than the key it was given (setting
    /// `listen_address` echoes `listen_auth` back too), so the cheapest way to
    /// stay truthful is to re-render everything from the file we just read.
    fn apply(&self, config: &Config) {
        if let Some(observer) = self.observer.borrow().as_ref() {
            observer(config);
        }

        let dns_is_inert = config.dns_filtering_is_inert();

        for row in self.rows.borrow().iter() {
            // Someone else's snapshot must not overwrite a row that is still
            // waiting on its own write.
            if row.pending.get() {
                continue;
            }

            match config.toggle(row.toggle) {
                Some(on) => {
                    row.switch.set_sensitive(true);
                    self.set_active(&row.switch, on);

                    // The one dependency the CLI does not enforce: in manual
                    // proxy mode nothing listens for DNS unless
                    // `dns_filtering.listen_port` names a port. Only worth
                    // saying while the switch is on — that is the state that
                    // promises protection it is not delivering.
                    let inert = on && dns_is_inert && row.toggle == Toggle::DnsFiltering;
                    row.caveat.set_visible(inert);
                    row.switch.set_subtitle(if inert {
                        "No effect in manual proxy mode until dns_filtering.listen_port \
                         names a port, such as 5353"
                    } else {
                        row.toggle.description()
                    });
                }
                // Absent, or holding something that is not a boolean. Saying
                // "unavailable" is honest; showing it as off would be a claim
                // about the user's protection that we cannot support.
                None => {
                    row.switch.set_sensitive(false);
                    row.caveat.set_visible(false);
                    row.switch.set_subtitle(&format!(
                        "Unavailable — {} is missing from the config file",
                        row.toggle.key()
                    ));
                }
            }
        }
    }

    /// Send one switch flip to the CLI, then confirm it against the file.
    fn toggle(self: &Rc<Self>, toggle: Toggle, on: bool) {
        // Insensitive until the file has spoken, so a second click cannot race
        // the first one's verification, and marked pending so a *different*
        // row's reconcile cannot overwrite this one from a staler snapshot.
        if let Some(row) = self.rows.borrow().iter().find(|row| row.toggle == toggle) {
            row.switch.set_sensitive(false);
            row.pending.set(true);
        }

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                let outcome = cli.set_bool(toggle.key(), on).map_err(|err| err.to_string());
                // Verify from the file. A CLI that printed a confirmation is
                // not proof — it prints one for a no-op, and for a change it
                // silently declined to make.
                (outcome, Config::load().ok())
            },
            move |(outcome, config)| this.settle(toggle, on, outcome, config),
        );
    }

    /// Reconcile the page against what `proxy.yaml` now says.
    ///
    /// The file decides, not the CLI. That ordering is the whole point: a
    /// refusal and a success are indistinguishable by exit code, and the
    /// confirmation line is printed in both cases.
    fn settle(
        &self,
        toggle: Toggle,
        requested: bool,
        outcome: Result<Applied, String>,
        config: Option<Config>,
    ) {
        // Clear the pending mark first, so this row is the one row `apply`
        // *does* render from the snapshot it was verified against.
        if let Some(row) = self.rows.borrow().iter().find(|row| row.toggle == toggle) {
            row.pending.set(false);
            // If the file became unreadable between the write and the re-read
            // there is nothing to render from, so at least leave the switch
            // usable rather than stranding it insensitive.
            row.switch.set_sensitive(true);
        }

        if let Some(config) = &config {
            self.apply(config);
        }

        // `None` — the file could not be read; `Some(None)` — it was read but
        // this key was not. The two call for different wording, and neither is
        // the same as "the change failed".
        let landed = config.as_ref().map(|config| config.toggle(toggle));

        if landed == Some(Some(requested)) {
            // The file agrees with the user. Whatever the CLI said about it is
            // no longer interesting — except for the one thing only it knows:
            // that the change has not reached the running proxy.
            if outcome.is_ok_and(|applied| applied.restart_required) {
                self.toasts
                    .add_toast(toast("Restart the proxy to apply this change"));
            }
            return;
        }

        let message = match (outcome, landed) {
            // The CLI explained itself, and its wording beats ours — it names
            // the valid values for an enum, or the key it did not recognise.
            (Err(message), _) => message,
            // Accepted, and the file was read, but the value is not what was
            // asked for. This is the shape of a silent refusal.
            (Ok(_), Some(Some(_))) => {
                let verb = if requested { "enable" } else { "disable" };
                format!("Could not {verb} {}", toggle.title())
            }
            // Accepted, but we cannot see the result. Saying it failed would be
            // a guess, and probably a wrong one.
            (Ok(_), _) => format!(
                "Changed {}, but could not re-read proxy.yaml to confirm it",
                toggle.title()
            ),
        };
        self.toasts.add_toast(toast(&message));
    }

    /// Move a switch without the change reading as a user action.
    fn set_active(&self, switch: &adw::SwitchRow, active: bool) {
        self.reconciling.set(true);
        switch.set_active(active);
        self.reconciling.set(false);
    }
}

fn loading_view() -> adw::Spinner {
    adw::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build()
}

fn error_view(message: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Configuration unavailable")
        .description(message)
        .build()
}
