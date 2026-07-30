//! The Advanced page: ports, listen address, authentication, outbound proxy,
//! worker threads and log level (`docs/architecture.md` §5).
//!
//! Structurally this is the Protection page with four control types instead of
//! one, and it keeps that page's discipline: state is read from `proxy.yaml`,
//! every write goes through `adguard-cli config set`, and each write is
//! confirmed by re-reading the file rather than by believing the CLI, which
//! prints `Config has been updated` for a no-op and for a change it declined to
//! make.
//!
//! Two things here are not just more of the same.
//!
//! **The CLI range-checks nothing.** It verifies that an integer setting was
//! given an integer and stops: `listen_ports.http_proxy` will accept `99999`,
//! `-2`, and `3.5` — the last landing a float in the YAML where every later read
//! expects an integer. The bounds in [`adguard_core::Setting`] are the only
//! thing standing between a spin row and an unusable config.
//!
//! **`listen_address` is a security control, so it is the one setting that does
//! not use the generic write path.** Moving it off loopback exposes the proxy to
//! the network, and doing so requires `listen_auth` to be *fully* configured
//! first — enabled, with a non-empty username **and** password. Without that the
//! CLI tries to prompt, gives up, keeps the old address, and still reports
//! success. [`adguard_core::config::listen_address_plan`] encodes the calls and
//! their order; this page adds the parts a plan cannot: a confirmation before
//! exposing the proxy, and a refusal to switch authentication back off while it
//! is exposed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adguard_core::config::{key, listen_address_plan};
use adguard_core::{AddressPlan, Applied, Cli, Config, Kind, Setting};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, worker};

/// How long a spin row sits still before its value is written.
///
/// Without this, holding down the `+` button issues one `config set` per
/// increment. The alternative — going insensitive for the ~40 ms of each
/// write — makes the control feel broken while it is being dragged.
const SPIN_SETTLE: Duration = Duration::from_millis(500);

/// The widget behind one setting.
enum Control {
    Switch(adw::SwitchRow),
    Number(adw::SpinRow),
    Text(adw::EntryRow),
    Secret(adw::PasswordEntryRow),
    Choice(adw::ComboRow),
}

impl Control {
    fn widget(&self) -> &gtk::Widget {
        match self {
            Self::Switch(row) => row.upcast_ref(),
            Self::Number(row) => row.upcast_ref(),
            Self::Text(row) => row.upcast_ref(),
            Self::Secret(row) => row.upcast_ref(),
            Self::Choice(row) => row.upcast_ref(),
        }
    }

    /// `AdwEntryRow` is an `AdwPreferencesRow`, not an `AdwActionRow`, so it has
    /// no subtitle to explain itself in. Those rows carry their description as a
    /// tooltip instead, and anything row-specific we would have put in a
    /// subtitle has to go to a toast or the group description.
    fn set_explanation(&self, text: &str) {
        match self {
            Self::Switch(row) => row.set_subtitle(text),
            Self::Number(row) => row.set_subtitle(text),
            Self::Choice(row) => row.set_subtitle(text),
            Self::Text(row) => row.set_tooltip_text(Some(text)),
            Self::Secret(row) => row.set_tooltip_text(Some(text)),
        }
    }
}

struct Row {
    setting: Setting,
    control: Control,
    /// Shown when the file holds something this page will not write — a port
    /// outside the permitted range, say.
    caveat: gtk::Image,
    /// A write for this row is in flight; reconciling leaves it alone. Without
    /// this, editing a second field while the first is settling lets the first
    /// one's staler snapshot revert it.
    pending: Cell<bool>,
    /// Bumped on every spin-row change; a scheduled write only fires if it is
    /// still the newest.
    generation: Cell<u64>,
    /// The file value this row was last painted from.
    ///
    /// Reconciling repaints the whole page, which is right for switches but
    /// destructive for an entry: type half a username, flip an unrelated
    /// switch, and that switch's reconcile would overwrite the field with the
    /// file's value mid-edit. So a row whose setting has not moved in the file
    /// since it was last painted is left alone entirely.
    ///
    /// Cleared deliberately — by [`AdvancedPage::repaint`] — when the widget
    /// must be brought back to the file even though the file did not change:
    /// after a refused edit, and after a write that the CLI accepted and
    /// silently declined to make.
    painted: RefCell<Option<String>>,
}

pub struct AdvancedPage {
    bin: adw::Bin,
    cli: Cli,
    toasts: adw::ToastOverlay,
    rows: RefCell<Vec<Rc<Row>>>,
    /// Set while we write control states ourselves, so the change handlers can
    /// tell a user's edit from our own reconcile. Property notifications are
    /// synchronous, so a plain flag around the write is enough.
    reconciling: Cell<bool>,
    /// The most recent reading of `proxy.yaml`.
    ///
    /// The listen-address controls need to answer questions *before* issuing a
    /// write — is the proxy exposed right now, are the credentials usable — and
    /// re-reading the file inside a click handler would mean blocking the main
    /// thread on I/O. This is that snapshot, refreshed by every reconcile.
    last: RefCell<Option<Config>>,
    /// The "Listen address" group, whose description carries the credential
    /// requirement when it is not yet met.
    listen_group: RefCell<Option<adw::PreferencesGroup>>,
}

impl AdvancedPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            cli,
            toasts,
            rows: RefCell::new(Vec::new()),
            reconciling: Cell::new(false),
            last: RefCell::new(None),
            listen_group: RefCell::new(None),
        });
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.bin
    }

    /// Re-read `proxy.yaml` and rebuild the page. Also how an edit made in a
    /// terminal reaches the UI.
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

    /// Repaint from a reading of `proxy.yaml` that this page did not ask for.
    ///
    /// The external-edit entry point, driven by [`crate::watch`]. Deliberately
    /// not `reload`: that swaps in a spinner and rebuilds every widget, which
    /// on this page loses nothing but on Advanced discards the `painted` guard
    /// and with it any half-typed entry. `apply` patches the rows in place and
    /// already skips any row with a write in flight.
    ///
    /// The one case that does need a rebuild is a page showing a spinner or an
    /// error, which has no rows to patch — so an unreadable config that becomes
    /// readable heals itself rather than staying stuck on "unavailable".
    pub fn reconcile(self: &Rc<Self>, config: &Config) {
        let unbuilt = self.rows.borrow().is_empty();
        if unbuilt {
            self.reload();
        } else {
            self.apply(config);
        }
    }

    fn build(self: &Rc<Self>, config: &Config) -> adw::PreferencesPage {
        self.rows.borrow_mut().clear();
        self.listen_group.replace(None);

        let page = adw::PreferencesPage::new();

        for (index, group) in adguard_core::ADVANCED.iter().enumerate() {
            let widget = adw::PreferencesGroup::builder().title(group.title).build();
            // The first group names the file, the way the Protection page does.
            if index == 0 {
                widget.set_description(Some(&format!(
                    "{} Read from {}.",
                    group.description,
                    abbreviate(config.path())
                )));
            } else if !group.description.is_empty() {
                widget.set_description(Some(group.description));
            }

            for setting in group.settings {
                widget.add(self.row(*setting).control.widget());
            }

            if group.settings.iter().any(|s| s.key == key::LISTEN_ADDRESS) {
                self.listen_group.replace(Some(widget.clone()));
            }
            page.add(&widget);
        }

        self.apply(config);
        page
    }

    fn row(self: &Rc<Self>, setting: Setting) -> Rc<Row> {
        let control = match setting.kind {
            Kind::Switch => Control::Switch(adw::SwitchRow::new()),
            Kind::Number { min, max, .. } => {
                Control::Number(adw::SpinRow::with_range(min as f64, max as f64, 1.0))
            }
            Kind::Text { secret: false } => Control::Text(adw::EntryRow::new()),
            Kind::Text { secret: true } => Control::Secret(adw::PasswordEntryRow::new()),
            Kind::Choice { options } => {
                let combo = adw::ComboRow::new();
                combo.set_model(Some(&gtk::StringList::new(options)));
                Control::Choice(combo)
            }
        };

        // Our own literals carry no markup, but `AdwPreferencesRow:use-markup`
        // defaults to true and the label is rendered as the title is assigned —
        // so the property goes first, as it does on every other row in this app.
        let row: &adw::PreferencesRow = match &control {
            Control::Switch(row) => row.upcast_ref(),
            Control::Number(row) => row.upcast_ref(),
            Control::Text(row) => row.upcast_ref(),
            Control::Secret(row) => row.upcast_ref(),
            Control::Choice(row) => row.upcast_ref(),
        };
        row.set_use_markup(false);
        row.set_title(setting.title);

        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(false);

        match &control {
            Control::Switch(row) => {
                row.set_subtitle_lines(2);
                row.add_prefix(&caveat);
            }
            Control::Number(row) => {
                row.set_subtitle_lines(2);
                row.add_prefix(&caveat);
            }
            Control::Choice(row) => {
                row.set_subtitle_lines(2);
                row.add_prefix(&caveat);
            }
            // An apply button keeps a text field from writing on every
            // keystroke: one `config set` per character would be absurd, and
            // half-typed values would be written and read back.
            Control::Text(row) => {
                row.set_show_apply_button(true);
                row.add_prefix(&caveat);
            }
            Control::Secret(row) => {
                row.set_show_apply_button(true);
                row.add_prefix(&caveat);
            }
        }
        control.set_explanation(setting.description);

        let row = Rc::new(Row {
            setting,
            control,
            caveat,
            pending: Cell::new(false),
            generation: Cell::new(0),
            painted: RefCell::new(None),
        });

        self.connect(&row);
        self.rows.borrow_mut().push(row.clone());
        row
    }

    /// Wire the control's change signal to a write.
    fn connect(self: &Rc<Self>, row: &Rc<Row>) {
        let page = Rc::downgrade(self);
        let this = row.clone();

        match &row.control {
            Control::Switch(switch) => {
                switch.connect_active_notify(move |switch| {
                    let Some(page) = page.upgrade() else { return };
                    if page.reconciling.get() {
                        return; // our own write, not a click
                    }
                    page.switched(&this, switch.is_active());
                });
            }
            Control::Number(spin) => {
                spin.connect_value_notify(move |spin| {
                    let Some(page) = page.upgrade() else { return };
                    if page.reconciling.get() {
                        return;
                    }
                    page.schedule_number(&this, spin.value().round() as i64);
                });
            }
            Control::Text(entry) => {
                entry.connect_apply(move |entry| {
                    let Some(page) = page.upgrade() else { return };
                    page.entered(&this, &entry.text());
                });
            }
            Control::Secret(entry) => {
                entry.connect_apply(move |entry| {
                    let Some(page) = page.upgrade() else { return };
                    page.entered(&this, &entry.text());
                });
            }
            Control::Choice(combo) => {
                combo.connect_selected_notify(move |combo| {
                    let Some(page) = page.upgrade() else { return };
                    if page.reconciling.get() {
                        return;
                    }
                    let options = this.setting.options();
                    let Some(chosen) = options.get(combo.selected() as usize) else {
                        return;
                    };
                    page.write(&this, chosen.to_string());
                });
            }
        }
    }

    // ---- rendering ----

    /// Render every row from one reading of the file.
    ///
    /// Done for the whole page rather than the row that changed: a `config set`
    /// can touch more than the key it was given — setting `listen_address`
    /// echoes `listen_auth` back too — so re-rendering everything from the file
    /// we just read is the cheapest way to stay truthful.
    fn apply(&self, config: &Config) {
        for row in self.rows.borrow().iter() {
            // Someone else's snapshot must not overwrite a row still waiting on
            // its own write.
            if row.pending.get() {
                continue;
            }
            self.render(row, config);
        }

        // The credential requirement, stated before the user meets it the hard
        // way. Appended to the group description rather than a row subtitle
        // because it is a property of the group: it is about what the address
        // field will accept, and it is the username and password rows that fix
        // it.
        if let Some(group) = self.listen_group.borrow().as_ref() {
            let base = adguard_core::ADVANCED
                .iter()
                .find(|g| g.settings.iter().any(|s| s.key == key::LISTEN_ADDRESS))
                .map(|g| g.description)
                .unwrap_or_default();
            match config.listen_auth().exposure_blocker() {
                Some(note) => group.set_description(Some(&format!("{base} {note}."))),
                None => group.set_description(Some(base)),
            }
        }

        self.last.replace(Some(config.clone()));
    }

    /// Force the next [`Self::render`] of this row to repaint, even if the file
    /// has not moved.
    ///
    /// Needed in exactly the two cases where the widget and the file disagree
    /// while the file is unchanged: an edit we refused locally, and a write the
    /// CLI accepted without acting on. The second is the important one — it is
    /// the failure mode this whole page is built around, and skipping the
    /// repaint would leave the control showing a value that never landed.
    fn repaint(&self, row: &Rc<Row>) {
        row.painted.replace(None);
    }

    fn render(&self, row: &Rc<Row>, config: &Config) {
        let setting = row.setting;

        // What the file says about this one setting, in a form that distinguishes
        // absent from empty. Every branch below depends only on this key, so an
        // unchanged value means an unchanged row.
        let snapshot = match setting.kind {
            Kind::Switch => format!("{:?}", config.bool_at(setting.key)),
            Kind::Number { .. } => format!("{:?}", config.int_at(setting.key)),
            Kind::Text { .. } => format!("{:?}", config.str_at(setting.key)),
            Kind::Choice { options } => format!("{:?}", config.choice_at(setting.key, options)),
        };
        if row.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return;
        }
        row.painted.replace(Some(snapshot));

        row.caveat.set_visible(false);

        match &row.control {
            Control::Switch(switch) => match config.bool_at(setting.key) {
                Some(on) => {
                    switch.set_sensitive(true);
                    self.without_feedback(|| switch.set_active(on));
                    switch.set_subtitle(setting.description);
                }
                None => self.mark_unavailable(row, "is missing from the config file"),
            },

            Control::Number(spin) => match config.int_at(setting.key) {
                Some(value) if setting.permits_number(value) => {
                    // Undo any widening a previous out-of-range render did:
                    // `apply` patches rows in place, so those bounds would
                    // otherwise survive and let the user dial straight back out
                    // of the permitted range.
                    if let Kind::Number { min, max, .. } = setting.kind {
                        let adjustment = spin.adjustment();
                        adjustment.set_lower(min as f64);
                        adjustment.set_upper(max as f64);
                    }
                    spin.set_sensitive(true);
                    self.without_feedback(|| spin.set_value(value as f64));
                    spin.set_subtitle(&describe_number(setting, value));
                }
                // The CLI accepts `99999` and `-2` for a port, and `3.5` writes
                // a float. Such a row goes read-only and names what is actually
                // there rather than writing a clamped value back.
                Some(value) => {
                    // Widen the adjustment far enough to *display* the real
                    // number first. A spin row clamps whatever it is given, so
                    // without this the row would show its own lower bound while
                    // the subtitle named a different value — the control
                    // contradicting its own explanation.
                    let adjustment = spin.adjustment();
                    adjustment.set_lower(adjustment.lower().min(value as f64));
                    adjustment.set_upper(adjustment.upper().max(value as f64));
                    self.without_feedback(|| spin.set_value(value as f64));
                    // Insensitive *after* the write, so the value lands.
                    spin.set_sensitive(false);
                    row.caveat.set_visible(true);
                    spin.set_subtitle(&format!(
                        "Set to {value} in proxy.yaml, outside the {} this page can \
                         set — edit the file directly to change it",
                        permitted_range(setting),
                    ));
                }
                None => self.mark_unavailable(row, "is missing, or is not a whole number"),
            },

            Control::Text(entry) => match config.str_at(setting.key) {
                Some(value) => {
                    entry.set_sensitive(true);
                    self.without_feedback(|| entry.set_text(value));

                    // The listen address is the one row whose *current* value is
                    // a security fact worth flagging, the way the Protection
                    // page flags DNS filtering that is switched on but inert.
                    // Anything outside loopback means anything that can reach
                    // this machine can use it as a proxy.
                    let exposed =
                        setting.key == key::LISTEN_ADDRESS && config.listens_beyond_loopback();
                    row.caveat.set_visible(exposed);
                    entry.set_tooltip_text(Some(if exposed {
                        "Reachable from your network — any machine that can reach this \
                         one can use the proxy. Set 127.0.0.1 to keep it local"
                    } else {
                        setting.description
                    }));
                }
                None => self.mark_unavailable(row, "is missing from the config file"),
            },

            Control::Secret(entry) => match config.str_at(setting.key) {
                Some(value) => {
                    entry.set_sensitive(true);
                    self.without_feedback(|| entry.set_text(value));
                    entry.set_tooltip_text(Some(setting.description));
                }
                None => self.mark_unavailable(row, "is missing from the config file"),
            },

            Control::Choice(combo) => match config.choice_at(setting.key, setting.options()) {
                Some(chosen) => {
                    combo.set_sensitive(true);
                    let index = setting
                        .options()
                        .iter()
                        .position(|option| *option == chosen)
                        .unwrap_or(0);
                    self.without_feedback(|| combo.set_selected(index as u32));
                    combo.set_subtitle(setting.description);
                }
                None => self.mark_unavailable(row, "holds a value this page does not recognise"),
            },
        }
    }

    /// A key we could not read. Insensitive and explicit, never rendered as a
    /// plausible default — claiming a port is disabled, or that authentication
    /// is off, when we merely could not read the setting is the more dangerous
    /// of the two errors.
    fn mark_unavailable(&self, row: &Rc<Row>, why: &str) {
        row.control.widget().set_sensitive(false);
        row.caveat.set_visible(true);
        row.control
            .set_explanation(&format!("Unavailable — {} {why}", row.setting.key));
    }

    // ---- writing ----

    /// A switch was clicked.
    fn switched(self: &Rc<Self>, row: &Rc<Row>, on: bool) {
        // Authentication may not be switched off while the proxy is listening
        // beyond loopback. `proxy.yaml`'s own comment says authentication is
        // required there, and `architecture.md` §5 asks the GUI to enforce that
        // rather than warn about it. The CLI will happily do it.
        if row.setting.key == key::LISTEN_AUTH_ENABLED && !on {
            let exposed = self
                .last
                .borrow()
                .as_ref()
                .is_some_and(Config::listens_beyond_loopback);
            if exposed {
                let address = self
                    .last
                    .borrow()
                    .as_ref()
                    .and_then(|config| config.str_at(key::LISTEN_ADDRESS).map(str::to_owned))
                    .unwrap_or_else(|| "a public address".to_owned());
                self.toasts.add_toast(toast(&format!(
                    "The proxy is listening on {address}. Return the listen address to \
                     127.0.0.1 before turning authentication off"
                )));
                // Back to whatever the file says, rather than forcing the switch
                // on: if a hand edit really has left it off while exposed, the
                // honest thing is to keep showing that.
                self.reset_row(row);
                return;
            }
        }

        self.write(row, if on { "true" } else { "false" }.to_owned());
    }

    /// A spin row moved. Collapse a burst of changes into one write.
    fn schedule_number(self: &Rc<Self>, row: &Rc<Row>, value: i64) {
        let generation = row.generation.get().wrapping_add(1);
        row.generation.set(generation);

        let page = Rc::downgrade(self);
        let row = row.clone();
        glib::timeout_add_local_once(SPIN_SETTLE, move || {
            // Superseded by a later change, or the page is gone.
            if row.generation.get() != generation {
                return;
            }
            let Some(page) = page.upgrade() else { return };
            // The file may already agree — the user may have dialled back to
            // where they started. `config set` would accept it and report
            // success either way, so skipping it saves a pointless write.
            let unchanged = page
                .last
                .borrow()
                .as_ref()
                .and_then(|config| config.int_at(row.setting.key))
                == Some(value);
            if unchanged {
                return;
            }
            page.write(&row, value.to_string());
        });
    }

    /// A text or password field was applied.
    fn entered(self: &Rc<Self>, row: &Rc<Row>, text: &str) {
        if row.setting.key == key::LISTEN_ADDRESS {
            self.move_listen_address(row, text.trim().to_owned());
            return;
        }
        self.write(row, text.to_owned());
    }

    /// Write one setting, then confirm it against the file.
    fn write(self: &Rc<Self>, row: &Rc<Row>, value: String) {
        row.control.widget().set_sensitive(false);
        row.pending.set(true);

        let cli = self.cli.clone();
        let key = row.setting.key;
        let secret = row.setting.is_secret();
        let this = self.clone();
        let row = row.clone();

        worker::run(
            move || {
                let outcome = if secret {
                    cli.set_secret(key, &value)
                } else {
                    cli.config_set(key, &value)
                };
                // The file decides, not the CLI: it prints a confirmation for a
                // no-op and for a change it silently declined to make.
                (outcome.map_err(|err| err.to_string()), Config::load().ok())
            },
            move |(outcome, config)| this.settle(&row, outcome, config),
        );
    }

    /// Move `listen_address`, which is the one setting with a precondition.
    fn move_listen_address(self: &Rc<Self>, row: &Rc<Row>, address: String) {
        // The CLI is the authority here — it refuses anything that is not a bare
        // IP, with better wording than we would write. This check only avoids
        // asking "expose the proxy?" about a typo: `is_loopback` answers false
        // for anything it cannot parse, which is the right default for safety
        // but would otherwise turn a slip of the keyboard into a scary prompt.
        if address.parse::<std::net::IpAddr>().is_err() {
            self.toasts.add_toast(toast(&format!(
                "{address:?} is not an IP address. Use a bare address such as \
                 127.0.0.1, with no port"
            )));
            self.reset_row(row);
            return;
        }

        let auth = self
            .last
            .borrow()
            .as_ref()
            .map(Config::listen_auth)
            .unwrap_or(adguard_core::AuthState {
                enabled: false,
                username_set: false,
                password_set: false,
            });

        let plan = listen_address_plan(&address, auth);
        if let Some(reason) = plan.blocked_reason() {
            // Nothing is issued. The plan cannot invent a password, and writing
            // the address anyway would report success and change nothing.
            self.toasts.add_toast(toast(&reason));
            self.reset_row(row);
            return;
        }

        if adguard_core::config::is_loopback(&address) {
            self.issue(row, plan);
            return;
        }

        // Leaving loopback puts the proxy on the network. Confirm it.
        let this = self.clone();
        let row = row.clone();
        glib::spawn_future_local(async move {
            if this.confirm_exposure(&address).await {
                this.issue(&row, plan);
            } else {
                this.reset_row(&row);
            }
        });
    }

    /// Ask before exposing the proxy beyond this machine.
    async fn confirm_exposure(&self, address: &str) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("Let other machines use this proxy?"),
            Some(&format!(
                "AdGuard will accept connections on {address}, so anything that can \
                 reach this machine on the network can use it as a proxy. Connections \
                 will need the username and password set below."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("expose", "Listen on the network");
        dialog.set_response_appearance("expose", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "expose"
    }

    /// Issue a plan's calls in order, stopping at the first failure.
    fn issue(self: &Rc<Self>, row: &Rc<Row>, plan: AddressPlan) {
        let calls: Vec<(String, String)> = plan
            .calls()
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        if calls.is_empty() {
            return;
        }

        row.control.widget().set_sensitive(false);
        row.pending.set(true);

        let cli = self.cli.clone();
        let this = self.clone();
        let row = row.clone();

        worker::run(
            move || {
                let mut restart_required = false;
                let mut outcome = Ok(());
                for (key, value) in &calls {
                    // The order is load-bearing: authentication has to be on
                    // before the address moves, or the second call silently does
                    // nothing. So a failure stops the sequence rather than
                    // pressing on to a call that cannot now succeed.
                    match cli.config_set(key, value) {
                        Ok(applied) => restart_required |= applied.restart_required,
                        Err(err) => {
                            outcome = Err(err.to_string());
                            break;
                        }
                    }
                }
                (
                    outcome.map(|()| Applied { restart_required }),
                    Config::load().ok(),
                )
            },
            move |(outcome, config)| this.settle(&row, outcome, config),
        );
    }

    /// Reconcile one row against what `proxy.yaml` now says.
    ///
    /// The file decides. A refusal and a success are indistinguishable by exit
    /// code, and the confirmation line is printed in both cases.
    fn settle(
        &self,
        row: &Rc<Row>,
        outcome: Result<Applied, String>,
        config: Option<Config>,
    ) {
        // Cleared first, so this is the one row `apply` renders from the
        // snapshot it was verified against — and forced, because the whole point
        // is that the file may *not* have changed: `config set` reports success
        // for a no-op and for a change it declined to make.
        row.pending.set(false);
        self.repaint(row);
        // If the file became unreadable between the write and the re-read there
        // is nothing to render from, so at least leave the control usable
        // rather than stranding it insensitive.
        row.control.widget().set_sensitive(true);

        let Some(config) = config else {
            self.toasts.add_toast(toast(&format!(
                "Changed {}, but could not re-read proxy.yaml to confirm it",
                row.setting.title
            )));
            return;
        };
        self.apply(&config);

        match outcome {
            // The CLI explained itself, and its wording beats ours — it names
            // the valid values of an enum, or the key it did not recognise.
            Err(message) => self.toasts.add_toast(toast(&message)),
            Ok(applied) => {
                if applied.restart_required {
                    self.toasts
                        .add_toast(toast("Restart the proxy to apply this change"));
                }
            }
        }
    }

    /// Put a control back to whatever the file says, after an edit we refused
    /// without asking the CLI.
    fn reset_row(&self, row: &Rc<Row>) {
        self.repaint(row);
        if let Some(config) = self.last.borrow().as_ref() {
            self.render(row, config);
        }
    }

    /// Move a control without the change reading as a user action.
    fn without_feedback(&self, write: impl FnOnce()) {
        self.reconciling.set(true);
        write();
        self.reconciling.set(false);
    }
}

/// The subtitle for a number row that holds a value we can write.
fn describe_number(setting: Setting, value: i64) -> String {
    match setting.kind {
        Kind::Number {
            disabled_value: Some(disabled),
            ..
        } if value == disabled => format!("{} — currently disabled", setting.description),
        _ => setting.description.to_owned(),
    }
}

fn permitted_range(setting: Setting) -> String {
    match setting.kind {
        Kind::Number { min, max, .. } => format!("range {min} to {max}"),
        _ => "range".to_owned(),
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
