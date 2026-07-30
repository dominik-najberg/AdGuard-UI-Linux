//! The Filters page: the filter catalogue, one switch per filter.
//!
//! State is read from the SQLite catalogue and written through
//! `adguard-cli filters` — the two directions never cross (see
//! `docs/architecture.md` §3). Every toggle therefore follows
//! act -> re-read -> reconcile: the CLI reports semantic failures at exit 0,
//! so the switch is only allowed to settle on a state the database confirms.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::filters::{self, Catalogue};
use adguard_core::{Cli, Filter, FilterCatalogue, FilterSet, FilterState, Locale};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, worker};

/// One read of everything the page renders.
struct Loaded {
    catalogue: FilterCatalogue,
    /// How many rules the user has actually written, for the "Your rules"
    /// subtitle. `None` when the file is unreadable or absent.
    user_rules: Option<usize>,
    user_rules_path: Option<PathBuf>,
}

/// A rendered filter and the state it was rendered from.
struct Row {
    switch: adw::SwitchRow,
    /// Last state the database confirmed. `Filter::action_for` reads from
    /// this, so a switch flip is always decided against observed reality
    /// rather than against what the UI happens to be showing.
    filter: RefCell<Filter>,
}

/// What a page hosting a catalogue contributes to it, and what it takes over.
///
/// The DNS page needs both halves. Its settings come from `proxy.yaml` and the
/// catalogue from `agflm_dns.db`, and they belong in one scrolling page rather
/// than two — so the host supplies the groups that go above, and says that the
/// user-rules row is its own.
pub struct Host {
    /// Groups rendered above the catalogue. Called on every build, and must
    /// return **fresh** widgets each time: the page they were added to is
    /// dropped when the catalogue is rebuilt, and a widget cannot be re-parented
    /// out of a dying one.
    pub prelude: Box<dyn Fn() -> Vec<adw::PreferencesGroup>>,

    /// The host renders the user-rules row itself, so the catalogue must not.
    ///
    /// Set for DNS, where the built-in row is wrong three ways over: the
    /// database has it permanently `is_enabled = 1`, `Filter::action_for` would
    /// send `dns filters enable -2147483648`, which is refused outright
    /// (contract §6), and the verification read afterwards looks at a database
    /// that cannot see the `proxy.yaml` list where the real switch lives.
    pub owns_user_rules: bool,
}

pub struct FiltersPage {
    /// The page content is swapped wholesale — spinner, error, or catalogue —
    /// which is simpler and less error-prone than reconciling child lists.
    bin: adw::Bin,
    cli: Cli,
    toasts: adw::ToastOverlay,
    set: FilterSet,
    locale: Locale,
    rows: RefCell<HashMap<i64, Row>>,
    /// Set while we write switch states ourselves, so the `active` handler can
    /// tell a user's click from our own reconcile. Property notifications are
    /// synchronous, so a plain flag around the write is enough.
    reconciling: Cell<bool>,
    host: Option<Host>,
}

impl FiltersPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay, set: FilterSet) -> Rc<Self> {
        Self::hosted(cli, toasts, set, None)
    }

    /// A catalogue rendered inside another page's content. See [`Host`].
    pub fn hosted(
        cli: Cli,
        toasts: adw::ToastOverlay,
        set: FilterSet,
        host: Option<Host>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            cli,
            toasts,
            set,
            locale: Locale::from_env(),
            rows: RefCell::new(HashMap::new()),
            reconciling: Cell::new(false),
            host,
        });
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.bin
    }

    /// Re-read the catalogue and rebuild the page.
    ///
    /// Used for the initial load and the explicit refresh. Individual toggles
    /// do **not** come through here — they patch the one row they touched, so
    /// flipping a switch two thirds of the way down "Language-specific" does
    /// not throw away the scroll position.
    pub fn reload(self: &Rc<Self>) {
        self.bin.set_child(Some(&loading_view()));

        let set = self.set;
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let catalogue = Catalogue::open_set(set).map_err(|err| err.to_string())?;
                let read = catalogue.read(&locale).map_err(|err| err.to_string())?;
                let path = set.user_rules_file();
                Ok(Loaded {
                    catalogue: read,
                    user_rules: path.as_deref().and_then(filters::user_rule_count),
                    user_rules_path: path,
                })
            },
            move |result: Result<Loaded, String>| match result {
                Ok(loaded) => {
                    let page = this.build(&loaded);
                    this.bin.set_child(Some(&page));
                }
                Err(err) => this.bin.set_child(Some(&error_view(&err))),
            },
        );
    }

    fn build(self: &Rc<Self>, loaded: &Loaded) -> adw::PreferencesPage {
        self.rows.borrow_mut().clear();

        let page = adw::PreferencesPage::new();

        // The host's groups go above the catalogue, and are rebuilt with it.
        if let Some(host) = &self.host {
            for group in (host.prelude)() {
                page.add(&group);
            }
        }

        let host_owns_user_rules = self.host.as_ref().is_some_and(|host| host.owns_user_rules);
        if let Some(user_rules) = &loaded.catalogue.user_rules {
            if !host_owns_user_rules {
                page.add(&self.user_rules_group(user_rules, loaded));
            }
        }

        for (group, filters) in loaded.catalogue.grouped() {
            // Group names come from AdGuard's own categories, which carry no
            // markup; row text does, so only the rows opt out of Pango.
            let rendered = adw::PreferencesGroup::builder().title(&group.name).build();
            for filter in filters {
                rendered.add(&self.row(filter));
            }
            page.add(&rendered);
        }

        page
    }

    /// The user's own rules — a toggle, not a subscribable list, so it gets
    /// its own group above the catalogue.
    fn user_rules_group(
        self: &Rc<Self>,
        user_rules: &Filter,
        loaded: &Loaded,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("Your rules").build();

        let row = self.row(user_rules);
        row.set_subtitle(&match (loaded.user_rules, &loaded.user_rules_path) {
            (Some(0), Some(path)) => format!("No rules yet — edit {}", abbreviate(path)),
            (Some(count), Some(path)) => {
                let plural = if count == 1 { "rule" } else { "rules" };
                format!("{count} {plural} in {}", abbreviate(path))
            }
            (_, Some(path)) => format!("Edit {} to add your own rules", abbreviate(path)),
            (_, None) => "Your own filtering rules".to_owned(),
        });
        group.add(&row);

        group
    }

    fn row(self: &Rc<Self>, filter: &Filter) -> adw::SwitchRow {
        let switch = adw::SwitchRow::new();

        // Filter names and descriptions are data, not markup: filter 216 is
        // literally "Official Polish filters for AdBlock, uBlock Origin &
        // AdGuard", and 251's description quotes '$' and '&'.
        //
        // This must precede the title. `AdwPreferencesRow:use-markup` defaults
        // to true and the label is rendered the moment the title is assigned,
        // so passing a title to the builder warns and mangles the text however
        // the property is set afterwards. Turning markup off first covers the
        // subtitle too.
        switch.set_use_markup(false);
        switch.set_title(&filter.name);

        if !filter.description.is_empty() {
            switch.set_subtitle(&filter.description);
            switch.set_subtitle_lines(2);
        }

        // Before the handler is connected, so the initial state is not read as
        // a click.
        switch.set_active(filter.enabled);

        let id = filter.id;
        let this = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.reconciling.get() {
                return; // our own write, not a click
            }
            this.toggle(id, switch.is_active());
        });

        self.rows.borrow_mut().insert(
            filter.id,
            Row {
                switch: switch.clone(),
                filter: RefCell::new(filter.clone()),
            },
        );

        switch
    }

    /// Send one switch flip to the CLI, then confirm it against the database.
    fn toggle(self: &Rc<Self>, filter_id: i64, on: bool) {
        let (action, name) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(&filter_id) else {
                return;
            };
            // Insensitive until the database has spoken, so a second click
            // cannot race the first one's verification.
            row.switch.set_sensitive(false);
            let filter = row.filter.borrow();
            (filter.action_for(on), filter.name.clone())
        };

        let cli = self.cli.clone();
        let set = self.set;
        let this = self.clone();
        worker::run(
            move || {
                let refused = cli
                    .filter_action(set, action, filter_id)
                    .err()
                    .map(|err| err.to_string());
                // Verify from the database. A CLI that printed a confirmation
                // is not proof, and one that complained is not disproof.
                let state = Catalogue::open_set(set)
                    .ok()
                    .and_then(|catalogue| catalogue.state(filter_id).ok().flatten());
                (refused, state)
            },
            move |(refused, state)| this.settle(filter_id, on, &name, refused, state),
        );
    }

    /// Reconcile one row against what the database now says.
    fn settle(
        &self,
        filter_id: i64,
        requested: bool,
        name: &str,
        refused: Option<String>,
        state: Option<FilterState>,
    ) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(&filter_id) else {
            return;
        };
        row.switch.set_sensitive(true);

        if let Some(state) = state {
            {
                let mut filter = row.filter.borrow_mut();
                filter.enabled = state.enabled;
                filter.installed = state.installed;
            }
            self.set_active(&row.switch, state.enabled);

            // The switch is where the user asked it to be; whether the CLI was
            // chatty about it is not interesting.
            if state.enabled == requested {
                return;
            }
        }

        let message = refused.unwrap_or_else(|| {
            let verb = if requested { "enable" } else { "disable" };
            format!("Could not {verb} {name}")
        });
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
        .title("Filters unavailable")
        .description(message)
        .build()
}
