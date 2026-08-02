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
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adguard_core::config::{is_port_list, key, listen_address_plan};
use adguard_core::{
    AddressPlan, Applied, Cli, Config, Kind, RootHelper, Setting, SettingGroup,
};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::root_helper::{join_with_and, RootHelperView};
use crate::{abbreviate, toast, worker};

/// The `proxy_mode` value AdGuard *gates* on its root helper, and so the one
/// whose row carries a caveat when the helper is not set up.
///
/// Not the only value that needs the helper: `manual`'s HTTP proxy fails every
/// request without it (contract §8). The difference is what the row can say —
/// choosing `auto` with the helper unmet selects a mode that does nothing,
/// which is this row's business, whereas the manual-mode breakage is the whole
/// application's and is reported where the user meets it, on Status.
const AUTO: &str = "auto";

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
    /// The table this page renders. `ADVANCED` for the Advanced page, `STEALTH`
    /// for the Stealth one — everything else here is identical, so the second
    /// page is this table and a sidebar entry rather than a second module.
    ///
    /// The `listen_address` special-casing below keys off `key::LISTEN_ADDRESS`
    /// and so is simply inert for a table that does not contain it.
    table: &'static [SettingGroup],
    cli: Cli,
    toasts: adw::ToastOverlay,
    rows: RefCell<Vec<Rc<Row>>>,
    /// The rendered groups, positionally against [`Self::table`], so a link from
    /// the Status page can be resolved to the part of this page it meant.
    ///
    /// The helper group is deliberately absent: it is not one of the table's and
    /// including it would put every group after `proxy_mode` one place out.
    groups: RefCell<Vec<adw::PreferencesGroup>>,
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
    /// The root-helper rows under the proxy-mode group. `None` on a table
    /// without `proxy_mode`, which is how the Stealth page gets none of this
    /// for free.
    helper_view: RefCell<Option<Rc<RootHelperView>>>,
    /// Where the helper check is read from.
    ///
    /// A field rather than a call to [`RootHelper::detect`] so a test — or
    /// anyone wanting to see the *met* branch on a machine where the helper is
    /// shipped unmet, which is every machine — can point it somewhere else.
    /// `$ADGUARD_ROOT_HELPER` is read once, at construction.
    helper_path: Option<PathBuf>,
}

impl AdvancedPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay, table: &'static [SettingGroup]) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            table,
            cli,
            toasts,
            rows: RefCell::new(Vec::new()),
            groups: RefCell::new(Vec::new()),
            reconciling: Cell::new(false),
            last: RefCell::new(None),
            listen_group: RefCell::new(None),
            helper_view: RefCell::new(None),
            // Read once, here, rather than at every check: an override that
            // changed underneath a running window would make the row's history
            // impossible to follow. Absent — the normal case — means AdGuard's
            // own helper, wherever this machine's CLI is installed.
            helper_path: std::env::var_os("ADGUARD_ROOT_HELPER")
                .map(PathBuf::from)
                .or_else(adguard_core::paths::root_helper),
        });
        this.reload();
        this
    }

    /// The root-helper check, as it stands right now.
    ///
    /// Read fresh every time rather than cached: the whole point of the focus
    /// re-check is that the user has just run AdGuard's `sudo` command in a
    /// terminal, and a cache would be exactly wrong at the one moment that
    /// matters. It is a `stat`, so it is cheap enough to do on the main loop.
    fn helper(&self) -> Option<std::io::Result<RootHelper>> {
        self.helper_path.as_ref().map(RootHelper::inspect)
    }

    /// Whether auto mode would actually work. Anything this check could not
    /// establish reads as **not** set up — the same rule as everywhere else
    /// here, that a fact we could not read is never rendered as the reassuring
    /// answer.
    fn helper_is_set_up(&self) -> bool {
        matches!(self.helper(), Some(Ok(helper)) if helper.is_set_up())
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
    ///
    /// Returns how many rows the user could have been looking at actually
    /// moved, which is what [`crate::watch`] gates its toast on. A page with no
    /// rows yet returns zero even though it reloads: there was nothing on
    /// screen to change.
    pub fn reconcile(self: &Rc<Self>, config: &Config) -> usize {
        let unbuilt = self.rows.borrow().is_empty();
        if unbuilt {
            self.reload();
            0
        } else {
            self.apply(config)
        }
    }

    /// Scroll to the group that holds `setting` and mark it, as a link from the
    /// Status page asks.
    ///
    /// Addressed by setting rather than by group title, because the caller is
    /// naming a `proxy.yaml` key it already knows — the same key it would hand
    /// `config set` — and titles are prose that can be reworded without anyone
    /// thinking about the links pointing at them.
    ///
    /// Silently does nothing while this page is still building, or for a setting
    /// this table does not carry. Either way the page has already been switched
    /// to and is showing its first group, which is where an unscrolled page is
    /// anyway.
    pub fn reveal(&self, setting: &str) {
        let Some(index) = self
            .table
            .iter()
            .position(|group| group.settings.iter().any(|s| s.key == setting))
        else {
            return;
        };
        if let Some(group) = self.groups.borrow().get(index) {
            crate::reveal(group);
        }
    }

    /// Has this page ever painted? A page with no rows has nothing to patch,
    /// which is the one case [`Self::reconcile`] answers with a rebuild.
    ///
    /// Public because a *hosted* table's rebuild belongs to its host: the bin
    /// this page would rebuild into is not the widget anyone is looking at.
    pub fn is_built(&self) -> bool {
        !self.rows.borrow().is_empty()
    }

    /// Build this table's groups as a **prelude inside another page**,
    /// unparented and ready to hand to [`crate::filters::Host`].
    ///
    /// The host contract is that a prelude returns **fresh** widgets on every
    /// call, because the page they were added to is dropped when the catalogue
    /// rebuilds and a widget cannot be re-parented out of a dying one.
    /// [`Self::build`] already makes fresh groups each time, so the only extra
    /// work is taking them back off the `PreferencesPage` it parents them to.
    ///
    /// Going through `build` rather than hand-rolling a group is the whole
    /// point. The DNS page's prelude is hand-built and therefore carries its
    /// own paint, its own write-then-re-read and its own reconcile; a second
    /// hand-built prelude would be a third copy of rules that have already been
    /// wrong once for being duplicated (`handoff.md` §3 item 13). This way the
    /// Filters page's switch is written, verified and reconciled by exactly the
    /// code that does it for the other forty rows.
    pub fn host_groups(self: &Rc<Self>, config: &Config) -> Vec<adw::PreferencesGroup> {
        let page = self.build(config);
        let groups = self.groups.borrow().clone();
        for group in &groups {
            page.remove(group);
        }
        self.last.replace(Some(config.clone()));
        groups
    }

    fn build(self: &Rc<Self>, config: &Config) -> adw::PreferencesPage {
        self.rows.borrow_mut().clear();
        self.groups.borrow_mut().clear();
        self.listen_group.replace(None);
        self.helper_view.replace(None);

        let page = adw::PreferencesPage::new();

        for (index, group) in self.table.iter().enumerate() {
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

            // The logs bundle belongs beside `log_level`, which is the setting
            // that decides what ends up in it — `architecture.md` §5. Keyed off
            // the setting rather than the group title, so a retitled group does
            // not silently drop the row.
            if group.settings.iter().any(|s| s.key == key::LOG_LEVEL) {
                widget.add(&crate::backup::logs_row(&self.cli, &self.toasts));
            }
            page.add(&widget);
            // In table order and with nothing skipped, which is what lets
            // `reveal` index this by the position of the group in the table.
            self.groups.borrow_mut().push(widget);

            // The helper rows belong immediately under the mode they explain,
            // not at the foot of the page: keyed off `proxy_mode` the way the
            // listen-address special-casing is keyed off its own setting, so a
            // table without that key — Stealth — never builds them.
            if group.settings.iter().any(|s| s.key == key::PROXY_MODE) {
                let view = RootHelperView::new(&self.toasts);
                page.add(view.widget());
                view.paint();
                self.helper_view.replace(Some(view));
            }
        }

        // Backup and restore, after the last group. Only on the Advanced
        // table: `STEALTH` and the Filters settings share this page type and
        // neither is where a user looks for a backup.
        if self.table.iter().any(|g| g.settings.iter().any(|s| s.key == key::LOG_LEVEL)) {
            let view = crate::backup::BackupView::new(&self.cli, &self.toasts);
            page.add(view.widget());
        }

        self.apply(config);
        page
    }

    /// Re-read the helper check and repaint the rows that report it.
    ///
    /// Public because the window calls it when it regains focus: the user runs
    /// AdGuard's `sudo` command in a terminal and comes back, and the row
    /// should have changed without them hunting for a refresh
    /// (`architecture.md` §6). Cheap enough for that — one `stat`.
    ///
    /// Also repaints the mode row, whose caveat depends on the same check: a
    /// helper that has just been set up turns `auto` from a warning into an
    /// ordinary value, and vice versa.
    pub fn recheck_helper(self: &Rc<Self>) {
        let view = self.helper_view.borrow().clone();
        let Some(view) = view else { return };
        view.paint();

        // The mode row's rendering reads the check, and `render` skips a row
        // whose snapshot has not moved — which is exactly what happens here,
        // because `proxy.yaml` has not changed at all. Force it.
        let mode_row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.setting.key == key::PROXY_MODE)
            .cloned();
        if let (Some(row), Some(config)) = (mode_row, self.last.borrow().clone()) {
            self.repaint(&row);
            self.render(&row, &config);
        }
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
                    page.chose(&this, chosen);
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
    ///
    /// Returns the number of rows that actually moved. A row with a write in
    /// flight is skipped and so never counts, which is what makes the app's own
    /// writes stop announcing themselves: the file has genuinely changed by the
    /// time the monitor looks, and the only reason that is not news is that we
    /// are the ones who changed it.
    fn apply(&self, config: &Config) -> usize {
        let mut moved = 0;
        for row in self.rows.borrow().iter() {
            // Someone else's snapshot must not overwrite a row still waiting on
            // its own write.
            if row.pending.get() {
                continue;
            }
            if self.render(row, config) {
                moved += 1;
            }
        }

        // The credential requirement, stated before the user meets it the hard
        // way. Appended to the group description rather than a row subtitle
        // because it is a property of the group: it is about what the address
        // field will accept, and it is the username and password rows that fix
        // it.
        if let Some(group) = self.listen_group.borrow().as_ref() {
            let base = self
                .table
                .iter()
                .find(|g| g.settings.iter().any(|s| s.key == key::LISTEN_ADDRESS))
                .map(|g| g.description)
                .unwrap_or_default();
            match config.listen_auth().exposure_blocker() {
                Some(note) => group.set_description(Some(&format!("{base} {note}."))),
                None => group.set_description(Some(base)),
            }
        }

        // Not counted towards `moved`. These rows report the filesystem, not
        // `proxy.yaml`, so they cannot have moved because of the edit that
        // brought us here — and a toast saying the config changed would be
        // wrong about a row that changed for another reason entirely.
        if let Some(view) = self.helper_view.borrow().as_ref() {
            view.paint();
        }

        self.last.replace(Some(config.clone()));
        moved
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

    /// Returns whether the row actually moved — false when the snapshot below
    /// matches what is already on screen and nothing was touched. That answer is
    /// what `apply` counts, so it has to cover **everything the row displays**,
    /// not just the setting it writes.
    fn render(&self, row: &Rc<Row>, config: &Config) -> bool {
        let setting = row.setting;

        // What the file says about this one setting, in a form that distinguishes
        // absent from empty — plus, for the two settings that have one, the
        // dependency `mark_unmet_dependency` reads. That second half is not
        // decoration: `https_filtering.encrypted_client_hello` renders a caveat
        // that depends on `dns_filtering.enabled`, so keying the snapshot on the
        // row's own value alone left the caveat stale when the *dependency* was
        // the thing that moved, and would now also have the row reporting that
        // nothing changed while its subtitle did.
        let mut snapshot = match setting.kind {
            Kind::Switch => format!("{:?}", config.bool_at(setting.key)),
            Kind::Number { .. } => format!("{:?}", config.int_at(setting.key)),
            // The null flag is not decoration either. `str_at` answers `None`
            // for a null *and* for a wrong type, so without it a row moving
            // between those two states would snapshot identically and never
            // repaint — leaving a usable empty entry where the file had just
            // become unreadable, or the reverse.
            Kind::Text { .. } => format!(
                "{:?} null={}",
                config.str_at(setting.key),
                config.is_null_at(setting.key)
            ),
            Kind::Choice { options } => format!("{:?}", config.choice_at(setting.key, options)),
        };
        if let Some(required) = setting.requires() {
            snapshot.push_str(&format!(" requires {:?}", config.bool_at(required)));
        }
        // The mode row's caveat reads the root-helper check, which lives outside
        // `proxy.yaml` entirely and can move while the file does not — that is
        // the whole point of the focus re-check. Without this the row would keep
        // warning about a helper the user had just set up.
        if setting.key == key::PROXY_MODE {
            snapshot.push_str(&format!(" helper_set_up={}", self.helper_is_set_up()));
        }
        if row.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return false;
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
                // Nothing to show, which for one setting is a real value rather
                // than a failure to read one: `outbound_interface` ships null,
                // meaning the system chooses. An empty, *usable* entry is the
                // honest rendering of that — greying it out would report the
                // shipped state of a stock install as broken.
                None if setting.may_be_absent() && config.is_null_at(setting.key) => {
                    entry.set_sensitive(true);
                    self.without_feedback(|| entry.set_text(""));
                    entry.set_tooltip_text(Some(setting.description));
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

                    // `auto` in the file with the helper unmet is a real state,
                    // not one this page can prevent — `config set proxy_mode
                    // auto` succeeds regardless (contract §8), so a terminal or
                    // a text editor reaches it. Marked rather than corrected,
                    // for the same reason the Protection page marks DNS
                    // filtering that is on but inert: the value is what it is,
                    // and quietly writing `manual` over the user's setting
                    // would be a change nobody asked for.
                    let inert = setting.key == key::PROXY_MODE
                        && chosen == AUTO
                        && !self.helper_is_set_up();
                    row.caveat.set_visible(inert);
                    combo.set_subtitle(if inert {
                        "Set to automatic, but AdGuard's root helper is not set up — \
                         see below. Traffic is not being redirected system-wide."
                    } else {
                        setting.description
                    });
                }
                None => self.mark_unavailable(row, "holds a value this page does not recognise"),
            },
        }

        self.mark_unmet_dependency(row, config);
        true
    }

    /// Flag a setting that reads fine but currently does nothing.
    ///
    /// Two `https_filtering` keys are documented as requiring `dns_filtering`,
    /// and the CLI enforces neither — it accepts the write and reports success,
    /// leaving a row that says "on" for protection the user does not have.
    /// This is the same judgement the Protection page makes about DNS filtering
    /// that is enabled but has no listen port: the honest rendering is the real
    /// value plus a warning, not a lie in either direction.
    ///
    /// Deliberately **not** insensitive. The value is real and writable, and
    /// greying it out would strand a user who wants to set it before switching
    /// the dependency on — which is a perfectly sensible order to work in.
    fn mark_unmet_dependency(&self, row: &Rc<Row>, config: &Config) {
        let Some(required) = row.setting.requires() else {
            return;
        };
        // A row already marked unavailable has a more urgent problem, and its
        // explanation should not be overwritten by this one.
        if !config.resolves(row.setting) || config.bool_at(required) == Some(true) {
            return;
        }

        row.caveat.set_visible(true);
        row.control.set_explanation(&format!(
            "{} — no effect until {required} is on",
            row.setting.description
        ));
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

    /// A combo row was changed.
    ///
    /// `proxy_mode` is the one choice with a precondition, and it is the GUI's
    /// alone to enforce: **`config set proxy_mode auto` succeeds with the root
    /// helper unmet** — exit 0, `Config has been updated`, and the file really
    /// holds `auto` afterwards (contract §8). Nothing downstream would stop the
    /// user, and what they would get is a mode that quietly does nothing, which
    /// is the `dns_filtering` mistake this app already refuses to repeat.
    ///
    /// So this refuses locally and says which of the three properties is
    /// missing, and the row goes back to whatever the file says rather than
    /// being forced to `manual` — if a hand edit really has left `auto` in
    /// there with an unmet helper, the honest thing is to keep showing it.
    fn chose(self: &Rc<Self>, row: &Rc<Row>, chosen: &str) {
        if row.setting.key == key::PROXY_MODE && chosen == AUTO {
            let refusal = match self.helper() {
                Some(Ok(helper)) if helper.is_set_up() => None,
                Some(Ok(helper)) => Some(format!(
                    "Automatic mode needs AdGuard's root helper, which is missing {}. \
                     Run `{}` in a terminal first — the row below has it.",
                    join_with_and(&helper.unmet()),
                    helper.setup_command()
                )),
                Some(Err(err)) => Some(format!(
                    "Automatic mode needs AdGuard's root helper, which could not be \
                     read — {err}"
                )),
                None => Some(
                    "Automatic mode needs AdGuard's root helper, and adguard-cli \
                     could not be located"
                        .to_owned(),
                ),
            };
            if let Some(message) = refusal {
                self.toasts.add_toast(toast(&message));
                self.reset_row(row);
                return;
            }
        }

        self.write(row, chosen.to_owned());
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
        // The only value this page checks before asking the CLI. `settle` toasts
        // a refusal verbatim because the CLI's wording beats ours — and for this
        // one key it does not: it answers *"Valid values are: space-separated
        // list of valid ports or range of port"*, and space-separated is exactly
        // what it rejects. So we say what `proxy.yaml` says instead.
        // `is_port_list` is deliberately no stricter than the CLI, so this
        // cannot refuse a value the CLI would have taken.
        // Clearing a row whose null is a real value writes the *word* `null`,
        // not the empty string. Both restore "the system decides", but only
        // this one restores the stock line byte-identically: an empty string
        // leaves a bare `outbound_interface:`, which every YAML reader calls
        // null while `config get` reads it back as an empty string. Measured,
        // `cli-contract.md` §5 — two readers disagreeing about one line is a
        // state to avoid, not to choose.
        if row.setting.may_be_absent() && text.trim().is_empty() {
            self.write(row, "null".to_owned());
            return;
        }
        if row.setting.key == key::FILTERED_PORTS && !is_port_list(text) {
            self.toasts.add_toast(toast(PORT_LIST_ADVICE));
            self.reset_row(row);
            return;
        }
        // `config set har_writer.location "~/har-dumps"` stores the tilde
        // literally at exit 0 — measured, `cli-contract.md` §9 — so the daemon
        // would make a directory actually called `~`. Nothing goes through a
        // shell here, so nothing else would expand it. This is the one place
        // the page is deliberately *more* permissive than the CLI rather than
        // stricter, and it is still the CLI that decides: the expanded path is
        // what gets written, so a refusal is still worded by `settle`.
        if row.setting.key == key::HAR_LOCATION {
            let home = std::env::var("HOME").unwrap_or_default();
            if !home.is_empty() {
                self.write(row, adguard_core::config::expand_home(text, &home));
                return;
            }
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

/// What the user is told when `filtered_ports` will not be accepted.
///
/// A named constant rather than a literal at the call site so a test can hold
/// it, because this is the one refusal on this page the CLI must not be allowed
/// to word: `adguard-cli` answers *"Valid values are: space-separated list of
/// valid ports or range of port"*, and `80 443` is refused. `cli-contract.md`
/// §5, measured. The wording here is `proxy.yaml`'s.
const PORT_LIST_ADVICE: &str = "Filtered ports are single ports and low:high ranges \
                                separated by commas, such as 80,443,8080 or \
                                80:5221,5300:49151";

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

/// The refusal wording for `filtered_ports`, which exists because the CLI's own
/// is wrong — `architecture.md` §5 and `cli-contract.md` §5.
#[cfg(test)]
mod tests {
    use super::PORT_LIST_ADVICE;
    use adguard_core::config::is_port_list;

    /// The trap this whole row was built around. `adguard-cli` answers a bad
    /// value with *"Valid values are: space-separated list of valid ports or
    /// range of port"* — and `80 443` is the one thing it refuses. Anyone
    /// aligning our toast with the CLI's message would be handing the user the
    /// form that cannot work, so the word cannot appear here.
    #[test]
    fn the_advice_never_repeats_the_cli_wrong_separator() {
        assert!(
            !PORT_LIST_ADVICE.contains("space-separated")
                && !PORT_LIST_ADVICE.contains("space separated"),
            "{PORT_LIST_ADVICE}"
        );
        assert!(PORT_LIST_ADVICE.contains("commas"), "{PORT_LIST_ADVICE}");
    }

    /// Advice a user cannot act on is worse than none, so the two forms it
    /// offers have to be forms the CLI actually takes. Checked against the
    /// validator rather than asserted, so an edit to either has to keep them
    /// agreeing.
    #[test]
    fn every_example_in_the_advice_is_one_the_cli_accepts() {
        let examples: Vec<&str> = PORT_LIST_ADVICE
            .split_whitespace()
            .filter(|word| word.starts_with(|c: char| c.is_ascii_digit()))
            .collect();
        assert_eq!(examples.len(), 2, "the advice stopped offering two examples: {examples:?}");
        for example in examples {
            assert!(is_port_list(example), "the advice offers {example:?}, which is refused");
        }
    }
}
