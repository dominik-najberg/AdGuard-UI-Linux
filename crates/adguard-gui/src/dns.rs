//! The DNS page: the DNS filter catalogue, plus the `dns_filtering` settings
//! that surround it.
//!
//! Two backing stores meet on one page. The catalogue comes from
//! `agflm_dns.db` and is rendered by [`crate::filters::FiltersPage`]; the
//! settings above it come from `proxy.yaml` and are written with `adguard-cli
//! config set` / `config list-add` / `config list-remove`. The page supplies
//! those settings to the catalogue as a [`Host`] prelude so the two halves
//! share one scroll, and takes the user-rules row over from it — see below.
//!
//! Three measured facts shape this page, all in `docs/cli-contract.md` §5.
//!
//! **The DNS user-rules row cannot go through `dns filters enable`.** In
//! `agflm_dns.db` the pseudo-filter is `is_enabled = 1, is_installed = 0`
//! permanently, the CLI refuses to enable something that was never added, and
//! the real switch is whether `dns_user.txt` appears in the
//! `dns_filtering.filters` list in `proxy.yaml`. So the row is built here and
//! written with the list commands, and [`Host::owns_user_rules`] stops the
//! catalogue from rendering its own broken copy.
//!
//! **`config list-add` does not deduplicate.** Adding a value the list already
//! holds appends it twice and reports success. Every add here is therefore
//! decided against the last reading of the file, and skipped when it would
//! change nothing — the switch cannot be trusted to imply the file's state,
//! because a reconcile may have moved it.
//!
//! **The listener is pinned to loopback.** Measured: with a port set, the proxy
//! listens on `127.0.0.1` over UDP and TCP, and moving `listen_address`
//! elsewhere takes the HTTP and SOCKS5 proxies with it while this one stays put.
//! So unlike `listen_address` on the Advanced page, this control needs no
//! confirmation dialog and no standing exposure warning — it cannot expose
//! anything. The row says so rather than leaving the user to wonder.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::config::key;
use adguard_core::{Applied, Cli, Config, DnsListenPort, FilterSet};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::filters::{FiltersPage, Host};
use crate::{abbreviate, toast, worker};

/// The file whose presence in `dns_filtering.filters` is the user-rules switch.
///
/// Matched exactly, and written exactly. The CLI stores whatever string it is
/// given — measured, `/tmp/foo.txt` is kept verbatim — so a bare filename is
/// what goes in and what is looked for.
const DNS_USER_RULES_ENTRY: &str = "dns_user.txt";

/// The three states of the listen-port control, in combo-row order.
const MODES: [&str; 3] = ["Disabled", "Automatic port", "Fixed port"];

/// The widgets of the settings half.
///
/// Rebuilt whenever the catalogue rebuilds, because the page they live on is
/// dropped with it — so this is replaced rather than reused, and every handler
/// reaches the page through a `Weak` instead of capturing a row.
struct Widgets {
    mode: adw::ComboRow,
    port: adw::SpinRow,
    port_caveat: gtk::Image,
    upstream: adw::EntryRow,
    fallbacks: adw::EntryRow,
    bootstraps: adw::EntryRow,
    user_rules: adw::SwitchRow,
}

/// What each row of the settings half was last painted from.
///
/// One field per row the user can see, so `paint` can report how many of them
/// actually moved rather than how many keys did — the mode row alone reads
/// three settings, and an edit to any of them moves exactly one row.
///
/// It lives on the page rather than on [`Widgets`], which is replaced whenever
/// the catalogue rebuilds. Resetting it there would make the first reconcile
/// after any rebuild report all five rows as moved, including when the edit was
/// to a key this page never shows.
struct Painted {
    mode: String,
    upstream: String,
    fallbacks: String,
    bootstraps: String,
    user_rules: String,
}

impl Painted {
    /// How many of the five rows differ. Field by field rather than a whole-
    /// struct comparison, because the answer is a count and not a yes/no.
    fn moved_to(&self, next: &Self) -> usize {
        [
            self.mode != next.mode,
            self.upstream != next.upstream,
            self.fallbacks != next.fallbacks,
            self.bootstraps != next.bootstraps,
            self.user_rules != next.user_rules,
        ]
        .into_iter()
        .filter(|differs| *differs)
        .count()
    }
}

pub struct DnsPage {
    cli: Cli,
    toasts: adw::ToastOverlay,
    /// The catalogue, which owns the page's actual widget.
    catalogue: RefCell<Option<Rc<FiltersPage>>>,
    widgets: RefCell<Option<Widgets>>,
    /// `None` until the first paint. A page that has never rendered has nothing
    /// the user could have been looking at, so its first paint moves no rows —
    /// the same rule the other pages state as "no rows yet returns zero".
    painted: RefCell<Option<Painted>>,
    /// The reading every decision on this page is made against. `config set`
    /// and `list-add` both report success for changes they did not make, so the
    /// file is the only witness — and for the list commands it is also the only
    /// way to avoid writing a duplicate.
    last: RefCell<Option<Config>>,
    /// Set while this page writes widget state itself, so a handler can tell a
    /// user's action from a repaint.
    painting: Cell<bool>,
    /// A write is in flight. A second one would race the first's re-read.
    pending: Cell<bool>,
}

impl DnsPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            cli: cli.clone(),
            toasts: toasts.clone(),
            catalogue: RefCell::new(None),
            widgets: RefCell::new(None),
            painted: RefCell::new(None),
            last: RefCell::new(None),
            painting: Cell::new(false),
            pending: Cell::new(false),
        });

        let prelude = {
            let this = Rc::downgrade(&this);
            Box::new(move || {
                let Some(this) = this.upgrade() else {
                    return Vec::new();
                };
                this.settings_groups()
            }) as Box<dyn Fn() -> Vec<adw::PreferencesGroup>>
        };

        let catalogue = FiltersPage::hosted(
            cli,
            toasts,
            FilterSet::Dns,
            Some(Host {
                prelude,
                owns_user_rules: true,
            }),
        );
        *this.catalogue.borrow_mut() = Some(catalogue);

        // The catalogue's own reload is already in flight; this fills in the
        // reading its prelude will be painted from.
        this.refresh_config();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.catalogue
            .borrow()
            .as_ref()
            .map(|catalogue| catalogue.widget().clone().upcast())
            .unwrap_or_else(|| adw::Bin::new().upcast())
    }

    /// Re-read `proxy.yaml` and the catalogue, and rebuild.
    ///
    /// The explicit refresh, and how an edit made in a terminal reaches this
    /// page when the watch is not running.
    pub fn reload(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(
            || Config::load().map_err(|err| err.to_string()),
            move |result: Result<Config, String>| {
                if let Ok(config) = result {
                    *this.last.borrow_mut() = Some(config);
                }
                // Rebuilds the catalogue, and with it the prelude, which paints
                // itself from the reading just stored.
                if let Some(catalogue) = this.catalogue.borrow().as_ref() {
                    catalogue.reload();
                }
            },
        );
    }

    /// Repaint from a reading of `proxy.yaml` this page did not ask for.
    ///
    /// The external-edit entry point, driven by [`crate::watch`]. Patches in
    /// place rather than rebuilding, for the reason the Advanced page does:
    /// a rebuild would discard a half-typed entry. A page that has no widgets
    /// yet has nothing to patch, so it reloads instead and heals.
    ///
    /// Returns how many rows the user could have been looking at actually
    /// moved, which is what [`crate::watch`] gates its toast on. A page with no
    /// widgets yet returns zero even though it reloads: there was nothing on
    /// screen to change.
    pub fn reconcile(self: &Rc<Self>, config: &Config) -> usize {
        *self.last.borrow_mut() = Some(config.clone());
        if self.widgets.borrow().is_some() {
            self.paint(config)
        } else {
            if let Some(catalogue) = self.catalogue.borrow().as_ref() {
                catalogue.reload();
            }
            0
        }
    }

    /// Read the file into [`Self::last`] and paint from it, without rebuilding.
    fn refresh_config(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(
            || Config::load().ok(),
            move |config: Option<Config>| {
                if let Some(config) = config {
                    this.paint(&config);
                    *this.last.borrow_mut() = Some(config);
                }
            },
        );
    }

    // ---- building -------------------------------------------------------

    fn settings_groups(self: &Rc<Self>) -> Vec<adw::PreferencesGroup> {
        let filtering = adw::PreferencesGroup::builder()
            .title("DNS filtering")
            .description(
                "The local DNS proxy always listens on 127.0.0.1, whatever the proxy's \
                 listen address is set to. A change takes effect when the proxy restarts.",
            )
            .build();

        let port_caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        port_caveat.set_visible(false);

        let mode = adw::ComboRow::builder()
            .title("Local DNS proxy")
            .model(&gtk::StringList::new(&MODES))
            .build();
        mode.set_use_markup(false);
        mode.set_subtitle_lines(3);
        mode.add_prefix(&port_caveat);

        let adjustment = gtk::Adjustment::new(
            5353.0,
            DnsListenPort::MIN as f64,
            DnsListenPort::MAX as f64,
            1.0,
            10.0,
            0.0,
        );
        let port = adw::SpinRow::builder()
            .title("Port")
            .adjustment(&adjustment)
            .build();
        port.set_use_markup(false);
        port.set_subtitle("The port the local DNS proxy listens on");

        filtering.add(&mode);
        filtering.add(&port);

        let servers = adw::PreferencesGroup::builder()
            .title("DNS servers")
            .description(
                "`default` uses the system resolver. Several servers may be given, \
                 separated by spaces.",
            )
            .build();

        let upstream = entry_row("Upstream", "An address, or a DNS-over-TLS/HTTPS/QUIC URL");
        let fallbacks = entry_row("Fallbacks", "Used when the upstream fails");
        let bootstraps = entry_row("Bootstraps", "Resolve the upstream's own name. IP addresses only");
        servers.add(&upstream);
        servers.add(&fallbacks);
        servers.add(&bootstraps);

        let rules = adw::PreferencesGroup::builder().title("Your DNS rules").build();
        let user_rules = adw::SwitchRow::new();
        user_rules.set_use_markup(false);
        user_rules.set_title("Use my own DNS rules");
        user_rules.set_subtitle_lines(2);
        rules.add(&user_rules);

        self.connect(&mode, &port, &upstream, &fallbacks, &bootstraps, &user_rules);
        *self.widgets.borrow_mut() = Some(Widgets {
            mode,
            port,
            port_caveat,
            upstream,
            fallbacks,
            bootstraps,
            user_rules,
        });

        // The prelude is built before the worker in `reload` has necessarily
        // finished, so paint from whatever reading is current — and refresh if
        // there is none yet.
        match self.last.borrow().as_ref() {
            // The count is not interesting here — this is the first paint of a
            // freshly built prelude, and `paint` reports zero for it anyway.
            Some(config) => {
                self.paint(config);
            }
            None => self.refresh_config(),
        }

        vec![filtering, servers, rules]
    }

    fn connect(
        self: &Rc<Self>,
        mode: &adw::ComboRow,
        port: &adw::SpinRow,
        upstream: &adw::EntryRow,
        fallbacks: &adw::EntryRow,
        bootstraps: &adw::EntryRow,
        user_rules: &adw::SwitchRow,
    ) {
        mode.connect_selected_notify({
            let this = Rc::downgrade(self);
            move |row| {
                let Some(this) = this.upgrade() else { return };
                if this.painting.get() {
                    return;
                }
                this.write_listen_port(row.selected());
            }
        });

        // A spin row emits on every step, so the write waits for the value to
        // settle — the same reason the Advanced page debounces its numbers.
        port.connect_value_notify({
            let this = Rc::downgrade(self);
            move |row| {
                let Some(this) = this.upgrade() else { return };
                if this.painting.get() {
                    return;
                }
                this.schedule_port(row.value() as i64);
            }
        });

        for (row, setting) in [
            (upstream, key::DNS_UPSTREAM),
            (fallbacks, key::DNS_FALLBACKS),
            (bootstraps, key::DNS_BOOTSTRAPS),
        ] {
            row.connect_apply({
                let this = Rc::downgrade(self);
                move |row| {
                    let Some(this) = this.upgrade() else { return };
                    if this.painting.get() {
                        return;
                    }
                    this.write_server(setting, &row.text());
                }
            });
        }

        user_rules.connect_active_notify({
            let this = Rc::downgrade(self);
            move |row| {
                let Some(this) = this.upgrade() else { return };
                if this.painting.get() {
                    return;
                }
                this.write_user_rules(row.is_active());
            }
        });
    }

    // ---- painting -------------------------------------------------------

    /// Render every row from one reading of the file.
    ///
    /// Returns the number of rows that actually moved. The rows are still
    /// written unconditionally — the count is taken from a snapshot beside
    /// them, not by skipping work — because this page's writes go out without a
    /// per-row guard and a repaint is what corrects a `config set` the CLI
    /// accepted without acting on.
    ///
    /// A write of our own in flight returns zero without painting, which is
    /// what stops the app announcing its own change: by the time the monitor
    /// looks, `proxy.yaml` really has moved, and the only reason that is not
    /// news is that we moved it.
    fn paint(self: &Rc<Self>, config: &Config) -> usize {
        let widgets = self.widgets.borrow();
        let Some(widgets) = widgets.as_ref() else {
            return 0;
        };
        // A write in flight owns the rows until it settles; a stale snapshot
        // would otherwise revert the value the user just chose.
        if self.pending.get() {
            return 0;
        }

        // One entry per row, taken before anything is written, and covering
        // every setting that row displays rather than only the one it writes:
        // the mode row's subtitle reads `dns_filtering.enabled` too, and moves
        // when that does.
        let fresh = Painted {
            mode: format!(
                "{:?} filtering={:?}",
                config.dns_listen_port(),
                config.bool_at(key::DNS_FILTERING)
            ),
            upstream: format!("{:?}", config.str_at(key::DNS_UPSTREAM)),
            fallbacks: format!("{:?}", config.str_at(key::DNS_FALLBACKS)),
            bootstraps: format!("{:?}", config.str_at(key::DNS_BOOTSTRAPS)),
            user_rules: format!("{:?}", config.lists(key::DNS_FILTERS, DNS_USER_RULES_ENTRY)),
        };
        let moved = match self.painted.borrow().as_ref() {
            Some(before) => before.moved_to(&fresh),
            None => 0,
        };
        *self.painted.borrow_mut() = Some(fresh);

        self.painting.set(true);

        match config.dns_listen_port() {
            Some(state) => {
                widgets.mode.set_sensitive(true);
                widgets.mode.set_selected(match state {
                    DnsListenPort::Disabled => 0,
                    DnsListenPort::Automatic => 1,
                    DnsListenPort::Fixed(_) => 2,
                });
                if let DnsListenPort::Fixed(port) = state {
                    widgets.port.set_value(f64::from(port));
                }
                widgets.port.set_visible(matches!(state, DnsListenPort::Fixed(_)));
                self.explain_listen_port(widgets, config, state);
            }
            None => {
                // A value the CLI accepted that no listener could use — 70000,
                // or the float `config set … 3.5` writes. Never clamped: showing
                // a clamped value invites writing it back by accident.
                widgets.mode.set_sensitive(false);
                widgets.port.set_visible(false);
                widgets.port_caveat.set_visible(true);
                widgets.mode.set_subtitle(&format!(
                    "Unavailable — {} holds a value outside its three states",
                    key::DNS_LISTEN_PORT
                ));
            }
        }

        for (row, setting) in [
            (&widgets.upstream, key::DNS_UPSTREAM),
            (&widgets.fallbacks, key::DNS_FALLBACKS),
            (&widgets.bootstraps, key::DNS_BOOTSTRAPS),
        ] {
            match config.str_at(setting) {
                Some(value) => {
                    row.set_sensitive(true);
                    row.set_text(value);
                }
                None => {
                    row.set_text("");
                    row.set_sensitive(false);
                }
            }
        }

        // `Some(false)` for an absent or emptied list, `None` only for a shape
        // that cannot be a list at all — see `Config::lists`. Rendering the
        // emptied case as unavailable would grey the row out at the instant the
        // user successfully turned it off.
        match config.lists(key::DNS_FILTERS, DNS_USER_RULES_ENTRY) {
            Some(on) => {
                widgets.user_rules.set_sensitive(true);
                widgets.user_rules.set_active(on);
                widgets.user_rules.set_subtitle(&user_rules_subtitle(on));
            }
            None => {
                widgets.user_rules.set_sensitive(false);
                widgets.user_rules.set_subtitle(&format!(
                    "Unavailable — {} is not a list in this file",
                    key::DNS_FILTERS
                ));
            }
        }

        self.painting.set(false);
        moved
    }

    /// Say what the chosen state actually does, including when it does nothing.
    ///
    /// The dependency runs both ways and neither direction is enforced by the
    /// CLI: a port with the switch off brings up no listener, and the switch on
    /// with no port filters nothing. `Config::dns_filtering_is_inert` drives the
    /// caveat the Protection page shows for the second; this row is where both
    /// are visible at once.
    fn explain_listen_port(&self, widgets: &Widgets, config: &Config, state: DnsListenPort) {
        let filtering_on = config.bool_at(key::DNS_FILTERING) == Some(true);
        let inert = !state.listens();

        widgets.port_caveat.set_visible(inert && filtering_on || state.listens() && !filtering_on);
        widgets.mode.set_subtitle(&match (state, filtering_on) {
            (DnsListenPort::Disabled, true) => {
                "No DNS proxy. DNS filtering is on but filters nothing while this is disabled."
                    .to_owned()
            }
            (DnsListenPort::Disabled, false) => "No DNS proxy.".to_owned(),
            (_, false) => {
                "Nothing will listen until DNS filtering is switched on, on the Protection page."
                    .to_owned()
            }
            (DnsListenPort::Automatic, true) => {
                "Listening on 127.0.0.1, on a port chosen by AdGuard.".to_owned()
            }
            (DnsListenPort::Fixed(port), true) => {
                format!("Listening on 127.0.0.1:{port}.")
            }
        });
    }

    // ---- writing --------------------------------------------------------

    fn write_listen_port(self: &Rc<Self>, selected: u32) {
        let state = match selected {
            0 => DnsListenPort::Disabled,
            1 => DnsListenPort::Automatic,
            _ => {
                let port = self
                    .widgets
                    .borrow()
                    .as_ref()
                    .map_or(5353, |widgets| widgets.port.value() as i64);
                match DnsListenPort::from_int(port) {
                    Some(state) => state,
                    None => return,
                }
            }
        };
        if let Some(widgets) = self.widgets.borrow().as_ref() {
            widgets.port.set_visible(matches!(state, DnsListenPort::Fixed(_)));
        }
        self.write_int(key::DNS_LISTEN_PORT, state.to_int());
    }

    /// Collapse a burst of spin-row steps into one write.
    fn schedule_port(self: &Rc<Self>, port: i64) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            let Some(this) = this.upgrade() else { return };
            let current = this
                .widgets
                .borrow()
                .as_ref()
                .map_or(port, |widgets| widgets.port.value() as i64);
            // Superseded by a later step.
            if current != port {
                return;
            }
            // The file may already agree — `config set` would accept it and
            // report success either way.
            let unchanged = this
                .last
                .borrow()
                .as_ref()
                .and_then(Config::dns_listen_port)
                == DnsListenPort::from_int(port);
            if unchanged {
                return;
            }
            this.write_int(key::DNS_LISTEN_PORT, port);
        });
    }

    fn write_int(self: &Rc<Self>, setting: &'static str, value: i64) {
        let cli = self.cli.clone();
        self.write(move || cli.set_int(setting, value));
    }

    fn write_server(self: &Rc<Self>, setting: &'static str, value: &str) {
        let unchanged = self
            .last
            .borrow()
            .as_ref()
            .and_then(|config| config.str_at(setting).map(str::to_owned))
            == Some(value.to_owned());
        if unchanged {
            return;
        }
        let cli = self.cli.clone();
        let value = value.to_owned();
        self.write(move || cli.config_set(setting, &value));
    }

    /// The user-rules toggle: membership of `dns_user.txt` in
    /// `dns_filtering.filters`.
    ///
    /// Decided against the file rather than against the switch, because
    /// `list-add` does not deduplicate — issuing an add for a value already
    /// there appends a second copy and reports success, so a switch that has
    /// been moved by a reconcile is not a safe thing to write from.
    fn write_user_rules(self: &Rc<Self>, on: bool) {
        let present = self
            .last
            .borrow()
            .as_ref()
            .and_then(|config| config.lists(key::DNS_FILTERS, DNS_USER_RULES_ENTRY));

        // Already where the user asked for, or unreadable — either way, writing
        // would be wrong.
        if present == Some(on) || present.is_none() {
            return;
        }

        let cli = self.cli.clone();
        self.write(move || {
            if on {
                cli.list_add(key::DNS_FILTERS, DNS_USER_RULES_ENTRY)
            } else {
                cli.list_remove(key::DNS_FILTERS, DNS_USER_RULES_ENTRY)
            }
        });
    }

    /// act -> re-read -> reconcile, for every write on this page.
    ///
    /// The CLI's confirmation is never the evidence: `Config has been updated`
    /// is printed for a no-op and for a change it declined to make. The reading
    /// taken afterwards is what the rows are painted from.
    fn write<F>(self: &Rc<Self>, action: F)
    where
        F: FnOnce() -> Result<Applied, adguard_core::Error> + Send + 'static,
    {
        if self.pending.get() {
            return;
        }
        self.pending.set(true);

        let this = self.clone();
        worker::run(
            move || {
                let outcome = action();
                let config = Config::load().ok();
                (outcome, config)
            },
            move |(outcome, config)| {
                this.pending.set(false);

                match outcome {
                    Ok(applied) => {
                        if applied.restart_required {
                            this.toasts
                                .add_toast(toast("Restart the proxy for this to take effect"));
                        }
                    }
                    Err(err) => this.toasts.add_toast(toast(&err.to_string())),
                }

                if let Some(config) = config {
                    this.paint(&config);
                    *this.last.borrow_mut() = Some(config);
                }
            },
        );
    }
}

fn entry_row(title: &str, subtitle: &str) -> adw::EntryRow {
    let row = adw::EntryRow::new();
    // AdGuard's own text and the user's, neither of which is markup.
    row.set_use_markup(false);
    row.set_title(title);
    // An apply button, so a value is not written on every keystroke.
    row.set_show_apply_button(true);
    row.set_tooltip_text(Some(subtitle));
    row
}

fn user_rules_subtitle(on: bool) -> String {
    let path = FilterSet::Dns.user_rules_file();
    match (on, path.as_deref()) {
        (true, Some(path)) => format!("Reading {}", abbreviate(path)),
        (false, Some(path)) => format!("Not in use — {} is ignored", abbreviate(path)),
        (true, None) => "In use".to_owned(),
        (false, None) => "Not in use".to_owned(),
    }
}
