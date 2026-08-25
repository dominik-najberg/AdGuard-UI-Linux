//! The Status page: runtime state, lifecycle control, and the licence.
//!
//! # Why this page does not look like the others
//!
//! Every other page in this app is a list of settings, and `AdwPreferencesPage`
//! is exactly right for that. This one answers a question instead — *am I
//! protected?* — and a labelled row reading "Status: Running" answers it in the
//! same visual weight as the eleven rows around it. So the answer is lifted out
//! into a tinted panel with the one action that changes it, and the rows below
//! keep their place as the detail behind the answer.
//!
//! The three figures under the panel come from files this page can read
//! directly — `proxy.yaml` and the two filter catalogues — never from
//! `adguard-cli`. That is not an optimisation: `status` is on a 2 s timer and
//! two concurrent `adguard-cli` invocations against one data directory is the
//! shape measured to make one of them fail (contract §3), so a figure that
//! needed the CLI could not be refreshed as freely as this one is.
//!
//! # Everything on it that reports a setting is a way in to that setting
//!
//! This page reads and does not write — apart from the proxy's own lifecycle and
//! the licence, it holds no control that changes anything. That is what makes it
//! readable, and it was also what made it a dead end: a figure reading "4 of 6"
//! is a question about the other two, and a row reading "Disabled" is a question
//! about how to enable it, and the answer to both was for the user to already
//! know which of the five other pages to go and look on.
//!
//! So each of them is now a link. The figures are buttons, the endpoint and
//! filtering rows are activatable, and each one names a [`Destination`] — the
//! page that owns the setting it is reporting, and where on it. This page picks
//! the destination and stops there; the window resolves it, in `main.rs`.
//!
//! **No link writes anything.** They lead to the control rather than operating
//! it, which keeps the one-writer-per-setting rule this app is built on: a
//! shortcut here that flipped `proxy_mode` would be a second writer for a key
//! the Advanced page owns, and the two would reconcile over each other.
//!
//! # Activation is user-driven, and that is the only shape the CLI supports
//!
//! The obvious design — open the activation URL, then poll `license` until it
//! reports `APP_ACTIVE` — cannot be written. Two measured facts rule it out
//! (contract §7): `license` is itself licence-gated, so while unlicensed it
//! refuses rather than reporting a status to poll for; and the CLI's own
//! message says the flow is completed by running `activate` **again**, not by
//! waiting. A poll would have no readable exit condition and might never see
//! one.
//!
//! So the flow is: run `activate`, take the log-in URL out of its no-TTY
//! message, open it with `gtk::UriLauncher`, and offer a *finish activation*
//! button that runs `activate` once more and then reads `license`. The button
//! is not a lesser version of the poll; it is the only shape there is. What
//! makes it work rather than merely defensible is that the link is stable —
//! measured, the `appid` in it belongs to the data directory, so running
//! `activate` again asks after the same pending activation the user was sent to
//! log into.
//!
//! Everything after that follows this app's usual discipline: `license` decides
//! the outcome, not anything `activate` printed.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use adguard_core::config::key;
use adguard_core::{
    access, helper, orphan, Activation, Autostart, Catalogue, Cli, Config, Daemon, FilterSet,
    Filtering, HelperProcess, License, ProxyStatus, RootHelper, Toggle,
};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{style, toast, worker, Destination};

/// `status` costs ~10 ms and there is no event mechanism to subscribe to, so
/// polling is the only way to notice the proxy going down underneath us.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// With the window hidden and only the tray showing, poll one tick in five —
/// `architecture.md` §3's "~10 s when only the tray is visible".
///
/// That policy is only expressible now that the tray lives in this process: a
/// separate tray binary could not know whether a window was open. It matters
/// for more than power, too — every invocation rewrites `proxy.yaml` and
/// touches its mtime (contract §5), so the idle poll rate is also the idle
/// churn rate a future file monitor has to see through.
const HIDDEN_POLL_EVERY: u32 = 5;

/// How often the access log is read back through.
///
/// **Not on the two-second poll, because the signal is not a two-second one.**
/// AdGuard's internal requests are roughly hourly and the verdict needs three of
/// them, so nothing this reads can change more than once an hour — and the read
/// is four mebibytes rather than the ten milliseconds a `status` costs. Five
/// minutes is twelve times finer than the fastest the answer can move, and 150
/// times fewer reads than the poll it rides on.
///
/// The cache it implies is dropped whenever this page acts on the proxy, so the
/// restart that cures a bypass clears the panel on the next tick rather than
/// five minutes later. See [`StatusPage::act`].
const ACCESS_LOG_EVERY: Duration = Duration::from_secs(5 * 60);

const PLACEHOLDER: &str = "—";

/// The primary button's label, icon and style class, by what it would do.
///
/// Named because two of the four hero states reach for the start button — the
/// one that offers it and the one that is only holding its place while the first
/// reading is outstanding — and they must not drift apart.
const START_BUTTON: (&str, &str, &str) = (
    "Start protection",
    "media-playback-start-symbolic",
    "suggested-action",
);
const STOP_BUTTON: (&str, &str, &str) = (
    "Stop protection",
    "media-playback-stop-symbolic",
    "destructive-action",
);

/// Where the licence stands, as far as this page knows.
///
/// The two failure variants are deliberately not one. "The licence is not
/// active" and "we could not read the licence" call for different words and,
/// more importantly, for different offers: activation is offered from the first
/// and **never** from the second, so a `license` we failed to parse can never
/// point `activate` at an install whose licence was fine all along.
enum Licence {
    /// Not read yet.
    Unknown,
    /// `license` answered. Its `status` is shown verbatim when it is anything
    /// other than active, rather than mapped to a word of ours.
    Read(License),
    /// `license` refused because the install is not licensed — [`Error::Unlicensed`],
    /// carrying the CLI's own sentence.
    ///
    /// [`Error::Unlicensed`]: adguard_core::Error::Unlicensed
    Inactive { message: String },
    /// `license` failed for some other reason: a timeout, or output we could not
    /// parse. Says so and offers nothing.
    Unreadable { message: String },
    /// `activate` handed us a log-in URL and it has been opened. Held until the
    /// user says they are done, because nothing polls for them.
    AwaitingLogin { url: String },
}

/// What is known about a proxy that is running and not filtering.
///
/// Two independent routes reach the same conclusion and they are **not** the
/// same news, so the panel is given the difference rather than a bool.
///
/// The two differ in latency as well as in wording, which is why neither
/// replaces the other: a corpse in `/proc` is visible the instant it appears,
/// and the log answers two hours later for every bypass that leaves no corpse.
enum Bypass {
    /// AdGuard's root helper is positively seen to be dead — cause-specific,
    /// immediate, and with a measured cure.
    ///
    /// `redirected` carries [`Config::redirects_traffic`], because a dead helper
    /// does two different things and the panel must not describe the wrong one.
    /// In `auto` mode the redirect stops and nothing is filtered at all. In
    /// `manual` — the CLI's default — the HTTP proxy answers 502 while the
    /// SOCKS5 proxy beside it serves normally, so a user reaching AdGuard over
    /// SOCKS5 is still filtered and must not be told otherwise, nor sent to
    /// clear a cache that was never filled unfiltered.
    Helper { redirected: bool },
    /// AdGuard's own requests through the proxy have been failing for hours, and
    /// the helper is not the reason — or not a reason we can see.
    ///
    /// **Deliberately says nothing about the proxy mode.** The `redirected`
    /// distinction above exists because a dead helper was *measured* to do two
    /// different things in the two modes. Nothing has measured what an unknown
    /// cause does, and inventing a mode-specific sentence for it would be the
    /// guess [`Bypass::Helper`] was given a mode to avoid.
    ///
    /// See [`adguard_core::access`] for the rule and the fourteen days it was
    /// measured against.
    Unreached,
}

/// What the page can say about the proxy, and the three shapes the hero panel
/// takes.
///
/// `Unreadable` is a state of its own rather than "stopped with a message": a
/// `status` we could not read says nothing about whether the proxy is up, and
/// offering *Start protection* on the strength of it would be a guess. It is
/// also the state a lapsed licence arrives in, which is why the panel has to be
/// able to hold a sentence of the CLI's rather than one of ours.
enum Runtime {
    /// Before the first reading comes back.
    Unknown,
    Up,
    /// Running, and not filtering anything.
    ///
    /// `status` reports what is **configured and listening**, never whether
    /// traffic reaches it, so this state and [`Self::Up`] are the same reading
    /// of the same command. What separates them is a second fact `status` does
    /// not carry, and [`Bypass`] is which one was found.
    ///
    /// The distinction is worth a state of its own because the failure is
    /// **silent and deceptive**: the panel would otherwise say "Protection is
    /// on" over a browser full of ads, which is the one thing this page exists
    /// not to do.
    Bypassed(Bypass),
    Down,
    Unreadable { message: String },
}

pub struct StatusPage {
    page: adw::PreferencesPage,
    cli: Cli,
    toasts: adw::ToastOverlay,

    /// The hero panel: the answer to "am I protected?", and the one control
    /// that changes it.
    hero: gtk::Box,
    shield: gtk::Image,
    badge: gtk::Label,
    headline: gtk::Label,
    /// The sentence under the headline. Ours while things are working, and the
    /// CLI's when they are not.
    detail: gtk::Label,
    /// Start or Stop, depending on [`Self::runtime`] — one button rather than
    /// two, so the page never offers an action that cannot apply.
    primary: gtk::Button,
    primary_content: adw::ButtonContent,
    restart: gtk::Button,
    runtime: RefCell<Runtime>,

    /// The three at-a-glance figures. Read from files, never from the CLI.
    modules: gtk::Label,
    web_filters: gtk::Label,
    dns_filters: gtk::Label,

    http: adw::ActionRow,
    socks5: adw::ActionRow,
    http_value: gtk::Label,
    socks5_value: gtk::Label,
    /// Where AdGuard's root helper is read from, for the HTTP endpoint's
    /// caveat. A field rather than a call to [`RootHelper::detect`] for the
    /// reason the Advanced page has one: `$ADGUARD_ROOT_HELPER` is what makes
    /// both branches reachable on a machine that sits on one side of the check.
    helper_path: Option<PathBuf>,
    manual_dns: StateRow,
    system_filtering: StateRow,
    system_dns: StateRow,

    /// The login entry, read but never written here — the switch that writes it
    /// is on the Advanced page, and this row leads there. `None` in a session
    /// with nowhere to put one, which the row reports rather than hides.
    autostart: Option<Autostart>,
    /// The word beside that row. Not a [`StateRow`], because its two states are
    /// not "enabled" and "disabled" and neither of them is good news.
    autostart_value: gtk::Label,

    licence_state: adw::ActionRow,
    licence_owner: adw::ActionRow,
    licence_key: adw::ActionRow,
    /// The log-in link, shown while one is outstanding. Present even when the
    /// browser opened, because that is the only way a user on a machine with no
    /// browser — or a portal that refused — can still finish.
    licence_link: adw::ActionRow,
    activate: gtk::Button,
    finish: gtk::Button,
    licence: RefCell<Licence>,

    /// Set while a lifecycle command — or an activation — is in flight.
    ///
    /// Polling pauses, so a reply that predates the command cannot re-enable
    /// the buttons mid-flight. Activation holds it for a second reason: it may
    /// run for up to `NETWORK_TIMEOUT`, and a poll left running would put
    /// dozens of `adguard-cli` invocations into the same data directory
    /// alongside it (contract §3).
    busy: Cell<bool>,

    /// What AdGuard's own log last said about traffic reaching the proxy —
    /// **and the proxy run it said it about**.
    ///
    /// Cached rather than read on every poll, for the reason
    /// [`ACCESS_LOG_EVERY`] gives, and the run is what keeps that cache honest.
    /// A verdict is scoped to one run; a restart ends the run and the evidence
    /// with it. This page cannot rely on having caused that restart — a
    /// terminal, a packaging script or `systemd` can end a run without it
    /// noticing — so the cache expires by *identity* and not only by age. See
    /// [`read_evidence`], which discards a verdict whose run no longer matches
    /// and re-reads on the spot rather than waiting out the interval.
    filtering: Cell<Cached>,
    filtering_read: Cell<Option<Instant>>,

    /// Whether the main window is on screen. False while only the tray is.
    window_visible: Cell<bool>,
    /// Poll ticks since start, for the hidden-window rate.
    ticks: Cell<u32>,

    /// Notified after every successful `status` read.
    ///
    /// The tray renders the same runtime state this page does, and this is how
    /// it gets it — rather than polling `status` itself, which is what a second
    /// process had to do.
    observer: RefCell<Option<Box<dyn Fn(&ProxyStatus, bool)>>>,

    /// Notified when a figure or a row here is clicked to go somewhere else.
    ///
    /// The same shape as `observer` and for the same reason: this page names
    /// what it wants and the window decides how to do it. It cannot hold the
    /// sidebar or the other pages itself — it is built before any of them exist,
    /// and the sidebar already holds this page.
    navigate: RefCell<Option<Box<dyn Fn(Destination)>>>,
}

impl StatusPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        // ---- the hero panel ----

        let shield = gtk::Image::builder()
            .pixel_size(56)
            .valign(gtk::Align::Center)
            .build();

        let badge = gtk::Label::builder().halign(gtk::Align::Start).build();
        badge.add_css_class(style::BADGE);

        let headline = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build();
        headline.add_css_class("title-2");

        // Wrapping, because this is where a `status` failure lands and the CLI's
        // sentences run to two lines.
        let detail = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build();
        detail.add_css_class("dim-label");

        let primary_content = adw::ButtonContent::new();
        let primary = gtk::Button::builder()
            .child(&primary_content)
            .halign(gtk::Align::Start)
            .sensitive(false)
            .build();
        primary.add_css_class("pill");
        let restart = gtk::Button::with_label("Restart");
        restart.add_css_class("pill");
        restart.set_sensitive(false);

        let hero_buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_top(14)
            .halign(gtk::Align::Start)
            .build();
        hero_buttons.append(&primary);
        hero_buttons.append(&restart);

        let hero_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        hero_text.append(&badge);
        hero_text.append(&headline);
        hero_text.append(&detail);
        hero_text.append(&hero_buttons);

        let hero = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(20)
            .build();
        hero.add_css_class("card");
        hero.add_css_class(style::HERO);
        hero.append(&shield);
        hero.append(&hero_text);

        let hero_group = adw::PreferencesGroup::new();
        hero_group.add(&hero);

        // ---- the three figures ----

        let modules = stat_value();
        let web_filters = stat_value();
        let dns_filters = stat_value();

        let stats = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            // So three tiles of very different text lengths still divide the
            // width evenly — the figures are meant to scan as a row.
            .homogeneous(true)
            .build();
        stats.add_css_class("card");
        stats.add_css_class(style::STATS);
        // No dividers between the tiles. A `GtkSeparator` in a homogeneous box
        // is allocated a full share of the width like any other child, so three
        // figures and two rules divide the card into fifths — the rules render
        // as grey slabs and the captions wrap inside what is left. Whitespace
        // separates them perfectly well.
        //
        // Collected rather than connected here, because the handler each one
        // needs is a method on a page that does not exist yet.
        let tiles: Vec<(gtk::Button, Destination)> = [
            (
                &modules,
                "Protection modules",
                "Show which protection modules are on",
                Destination::Protection,
            ),
            (
                &web_filters,
                "Web filters",
                "Show the web filter lists",
                Destination::WebFilters,
            ),
            (
                &dns_filters,
                "DNS filters",
                "Show the DNS filter lists",
                Destination::DnsFilters,
            ),
        ]
        .into_iter()
        .map(|(value, caption, tooltip, destination)| {
            let button = stat_button(value, caption, tooltip);
            stats.append(&button);
            (button, destination)
        })
        .collect();

        let stats_group = adw::PreferencesGroup::new();
        stats_group.add(&stats);

        // ---- the detail behind the answer ----

        // Where the ports are changed goes in the description rather than on
        // each row, because both rows lead to the same group and saying it twice
        // would read as two different offers.
        let endpoint_group = adw::PreferencesGroup::builder()
            .title("Proxy endpoints")
            .description(
                "Point applications at these local addresses to filter their traffic. \
                 Their ports are set on the Advanced page.",
            )
            .build();
        let (http, http_value) = endpoint_row("HTTP", "Change the HTTP proxy port");
        let (socks5, socks5_value) = endpoint_row("SOCKS5", "Change the SOCKS5 proxy port");
        // The HTTP row is the one that carries the root-helper caveat, and that
        // caveat is a sentence rather than the two words "Not listening".
        http.set_subtitle_lines(2);
        for r in [&http, &socks5] {
            endpoint_group.add(r);
        }

        // These three rows lead to two different pages, so each says where it
        // goes itself. The wording names the setting behind the state rather
        // than claiming the mechanism: `status` reports what the daemon is
        // doing, and how it came to be doing it is the other page's to explain.
        let filtering_group = adw::PreferencesGroup::builder().title("Filtering").build();
        let manual_dns = StateRow::new(
            "Manual DNS proxy",
            "Set the local DNS proxy's port on the DNS page",
        );
        let system_filtering = StateRow::new(
            "System-wide filtering",
            "Follows the proxy mode, on the Advanced page",
        );
        let system_dns = StateRow::new(
            "System-wide DNS filtering",
            "Follows the proxy mode, on the Advanced page",
        );
        for r in [&manual_dns, &system_filtering, &system_dns] {
            filtering_group.add(&r.row);
        }

        let licence_group = adw::PreferencesGroup::builder()
            .title("Licence")
            .description(
                "AdGuard will not report its state or touch your filters without \
                 an active licence, so everything above this depends on it.",
            )
            .build();
        let licence_state = row("State", "Checking…");
        // The CLI's refusal is two sentences, its own wording kept.
        licence_state.set_subtitle_lines(3);
        let licence_owner = row("Owner", PLACEHOLDER);
        let licence_key = row("Key", PLACEHOLDER);
        let licence_link = row("Log-in link", PLACEHOLDER);
        licence_link.set_subtitle_lines(3);
        // So the link can be got at by hand as well as by the button beside it.
        licence_link.set_subtitle_selectable(true);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy the log-in link"));
        copy.set_valign(gtk::Align::Center);
        copy.add_css_class("flat");
        licence_link.add_suffix(&copy);

        let activate = gtk::Button::with_label("Activate…");
        activate.add_css_class("suggested-action");
        let finish = gtk::Button::with_label("Finish activation");
        let licence_buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        for b in [&activate, &finish] {
            b.set_visible(false);
            licence_buttons.append(b);
        }

        for r in [&licence_state, &licence_owner, &licence_key, &licence_link] {
            licence_group.add(r);
        }
        licence_group.add(&licence_buttons);

        // ---- the application, which is not the protection ----
        //
        // Last, and below the licence, because it is the one thing on this page
        // that does not depend on it — and because it is not an answer to the
        // question the page asks. It is here for reach: the switch is at the
        // foot of a forty-row page, and this is the page people land on.
        //
        // **The description is the reason this row can be here at all.** On a
        // page that answers *am I protected?*, a row reading "Start at login —
        // No" invites exactly one wrong conclusion, and the entry this reports
        // starts `adguard-ui --background`, which never runs `start`. So the
        // line is drawn in the group description rather than left to be
        // inferred, and it says what this application does rather than what
        // AdGuard does — whether AdGuard's own proxy comes up at login is
        // AdGuard's arrangement, not ours to claim either way.
        let app_group = adw::PreferencesGroup::builder()
            .title("This application")
            .description(
                "Whether the AdGuard UI window and tray icon come back when you log in. \
                 It does not start or stop AdGuard's protection.",
            )
            .build();
        let autostart_row = link_row("Start at login");
        autostart_row.set_subtitle("Set at the foot of the Advanced page");
        let autostart_value = gtk::Label::builder().valign(gtk::Align::Center).build();
        // Dim in **both** states, unlike every other value on this page. The
        // green there means "you are protected"; there is no protection in this
        // row either way, and colouring "Yes" would say there was.
        autostart_value.add_css_class("dim-label");
        autostart_row.add_suffix(&autostart_value);
        autostart_row.add_suffix(&chevron());
        app_group.add(&autostart_row);

        for g in [
            &hero_group,
            &stats_group,
            &endpoint_group,
            &filtering_group,
            &licence_group,
            &app_group,
        ] {
            page.add(g);
        }

        let this = Rc::new(Self {
            page,
            cli,
            toasts,
            hero,
            shield,
            badge,
            headline,
            detail,
            primary: primary.clone(),
            primary_content,
            restart: restart.clone(),
            runtime: RefCell::new(Runtime::Unknown),
            modules,
            web_filters,
            dns_filters,
            http,
            socks5,
            http_value,
            socks5_value,
            helper_path: std::env::var_os("ADGUARD_ROOT_HELPER")
                .map(PathBuf::from)
                .or_else(adguard_core::paths::root_helper),
            manual_dns,
            system_filtering,
            system_dns,
            autostart: Autostart::locate(),
            autostart_value,
            licence_state,
            licence_owner,
            licence_key,
            licence_link,
            activate: activate.clone(),
            finish: finish.clone(),
            licence: RefCell::new(Licence::Unknown),
            busy: Cell::new(false),
            filtering: Cell::new(None),
            filtering_read: Cell::new(None),
            window_visible: Cell::new(true),
            ticks: Cell::new(0),
            observer: RefCell::new(None),
            navigate: RefCell::new(None),
        });

        // The primary button's action follows the state the panel is showing,
        // so there is one place where "what does this button do" is decided —
        // `render_runtime`, which is also what wrote the label the user read.
        primary.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                let Some(this) = this.upgrade() else { return };
                if let Some(action) = this.primary_action() {
                    this.act(action);
                }
            }
        });

        restart.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.act(Action::Restart);
                }
            }
        });

        // ---- the links out ----
        //
        // Every one of these reports a setting that is written on another page,
        // and each opens that page rather than offering a control of its own —
        // see the note on one writer per setting at the top of this module.
        for (button, destination) in tiles {
            button.connect_clicked({
                let this = Rc::downgrade(&this);
                move |_| {
                    if let Some(this) = this.upgrade() {
                        this.go(destination);
                    }
                }
            });
        }

        for (row, destination) in [
            // The two endpoints lead to their own port, not merely to the group
            // holding both: the group is what gets scrolled to either way, and
            // naming the port keeps each row honest about what it is reporting.
            (&this.http, Destination::Advanced(key::LISTEN_PORT_HTTP)),
            (&this.socks5, Destination::Advanced(key::LISTEN_PORT_SOCKS5)),
            (&this.manual_dns.row, Destination::DnsProxy),
            (
                &this.system_filtering.row,
                Destination::Advanced(key::PROXY_MODE),
            ),
            (&this.system_dns.row, Destination::Advanced(key::PROXY_MODE)),
            // The one destination that is not a `proxy.yaml` key, and the one
            // link on this page that leads to something which is not a setting.
            (&autostart_row, Destination::Autostart),
        ] {
            row.connect_activated({
                let this = Rc::downgrade(&this);
                move |_| {
                    if let Some(this) = this.upgrade() {
                        this.go(destination);
                    }
                }
            });
        }

        activate.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.begin_activation();
                }
            }
        });

        finish.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.finish_activation();
                }
            }
        });

        copy.connect_clicked({
            let this = Rc::downgrade(&this);
            move |button| {
                let Some(this) = this.upgrade() else { return };
                let Some(url) = this.login_url() else { return };
                button.clipboard().set_text(&url);
                this.toasts.add_toast(toast("Log-in link copied"));
            }
        });

        // Before the first read comes back, so nothing is on screen holding
        // placeholders for the ~20 ms it takes: the panel says "Checking…"
        // rather than claiming a state, and the licence rows for owner, key and
        // the log-in link are absent until there is something to put in them.
        this.render_runtime();
        this.render_licence();
        // Reads a file rather than waiting on the CLI, so it can be answered
        // now instead of by a placeholder.
        this.recheck_autostart();

        this.start_polling();
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    /// Re-read everything this page shows, licence included.
    ///
    /// The user-driven refresh, and deliberately not what the 2 s poll calls:
    /// the licence is never polled (contract §7), and nothing about it changes
    /// on its own.
    ///
    /// # The two reads are sequential, and that is load-bearing
    ///
    /// One worker, one command after the other, rather than the obvious
    /// `refresh(); read_licence();` — which would put two `adguard-cli`
    /// processes into the same data directory at the same instant. Measured
    /// (contract §3): against a directory that has never been used, that race
    /// leaves one of them exiting 1 with `Filter manager initialization failed`
    /// on stdout, eight runs in twelve. `status` shrugs it off because the poll
    /// repeats it two seconds later; the licence read is never on a timer, so
    /// it would sit there showing an initialisation error until the user found
    /// the refresh button. Not racing is cheaper than recovering.
    pub fn reload(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let (log_due, cached) = self.access_log_due();
        let this = self.clone();
        worker::run(
            move || {
                let status = read_status(&cli);
                let licence = read_licence(&cli);
                // Not a third `adguard-cli`, so it races nothing the note above
                // is about: a walk of `/proc`, a tail of one log, and no
                // subprocess at all.
                let evidence = read_evidence(&cli, &status, log_due, cached);
                (status, licence, evidence)
            },
            move |(status, licence, evidence)| {
                this.settle_status(status, evidence);
                match licence {
                    Ok(licence) => this.licence_read(licence),
                    Err((unlicensed, message)) => this.licence_refused(unlicensed, message),
                }
            },
        );
        // Its own worker, and safely so: the figures come from `proxy.yaml` and
        // the two SQLite catalogues, so this read runs alongside the two CLI
        // invocations above without being a third one.
        self.refresh_stats();
    }

    /// Re-read the three figures under the panel.
    ///
    /// Cheap and CLI-free — one ~9 KB YAML parse and two `COUNT(*)`s — so it is
    /// called from anywhere the figures could have gone stale: start-up, the
    /// refresh button, and a page becoming visible after the user changed
    /// something on Protection or Filters. Nothing here can fail in a way worth
    /// reporting; a figure that could not be read shows a dash.
    pub fn refresh_stats(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(Stats::read, move |stats: Stats| this.apply_stats(&stats));
        // Read here rather than in the worker beside the figures: it is one
        // small file in the user's own configuration directory, and the answer
        // is wanted in the same frame the page appears in rather than one hop
        // later. Arriving at this page is exactly the moment it can have gone
        // stale — the switch that writes it is two pages away.
        self.recheck_autostart();
    }

    /// Repaint the login row from the file.
    ///
    /// Public for the window's focus handler, which is the other moment the
    /// answer can have changed without this application doing anything: a
    /// startup-applications editor writes the same file (`architecture.md` §4).
    pub fn recheck_autostart(&self) {
        let Some(entry) = &self.autostart else {
            // No configuration directory at all. Reported rather than hidden,
            // and the link still leads to the switch that explains why.
            self.autostart_value.set_label("Unavailable");
            return;
        };
        self.autostart_value.set_label(match entry.is_enabled() {
            Ok(true) => "Yes",
            Ok(false) => "No",
            // There and unreadable, which is neither. The same three-way answer
            // the switch itself gives, in one word.
            Err(_) => "Unavailable",
        });
    }

    /// Repaint the module count from a reading of `proxy.yaml` this page did not
    /// ask for — the external-edit path, driven by [`crate::watch`].
    ///
    /// Takes the config it was given rather than reading the file again: the
    /// watch has just read it, and the two must not be able to disagree. The
    /// filter counts are untouched, since `proxy.yaml` says nothing about them.
    ///
    /// **This is the one reconcile that reports no count**, and the exception is
    /// deliberate. Every other page gates its count on a per-row `pending` flag,
    /// which is what makes the app's own writes silent: the page that issued the
    /// write is holding the row, so the row does not count as having moved.
    ///
    /// This figure has no such flag and could not have one. It is derived from
    /// the six `Toggle::ALL` keys the *Protection* page writes, so it moves
    /// whenever any of them does — measured: flipping ad blocking in the app
    /// left Protection's own row correctly silent and this figure reporting a
    /// change, which raised a toast announcing the user's own click back at
    /// them. Counting it defeats the entire point of counting.
    ///
    /// Nothing is lost by leaving it out. `module_count` reads exactly the keys
    /// Protection displays, so this figure cannot move without a Protection row
    /// moving too — there is no edit that would go unreported because of this.
    pub fn reconcile(&self, config: &Config) {
        self.set_modules(Some(module_count(config)));
    }

    /// Re-read the runtime status alone. What the 2 s poll calls, and what
    /// every lifecycle command calls after acting.
    pub fn refresh(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let (log_due, cached) = self.access_log_due();
        let this = self.clone();
        worker::run(
            move || {
                let status = read_status(&cli);
                let evidence = read_evidence(&cli, &status, log_due, cached);
                (status, evidence)
            },
            move |(status, evidence)| this.settle_status(status, evidence),
        );
    }

    /// Whether this reading may re-read the access log, and what to reuse if
    /// not.
    ///
    /// Decided here rather than on the worker because the clock and the cache
    /// belong to the page. **Permission, not a promise**: the worker declines
    /// when `status` reports no proxy or `/proc` cannot name one, and the clock
    /// is therefore stamped by [`Self::settle_status`] from what actually
    /// happened. Stamping it here instead would start the five minutes running
    /// on a read that never took place — and the first tick of every start-up,
    /// where `status` has yet to answer, is exactly such a tick.
    fn access_log_due(&self) -> (bool, Cached) {
        let due = self
            .filtering_read
            .get()
            .is_none_or(|read| read.elapsed() >= ACCESS_LOG_EVERY);
        (due, self.filtering.get())
    }

    /// Render one `status` reading, and notice when it contradicts the licence.
    fn settle_status(
        self: &Rc<Self>,
        result: Result<ProxyStatus, (bool, String)>,
        evidence: Evidence,
    ) {
        let (bypass, cached, read) = evidence;
        self.filtering.set(cached);
        if read {
            self.filtering_read.set(Some(Instant::now()));
        }
        match result {
            Ok(status) => self.apply(&status, bypass),
            Err((unlicensed, message)) => {
                // Kept in the panel rather than a toast: a failing `status`
                // repeats every two seconds and would bury the UI in toasts.
                // The panel is where it belongs anyway — it is the answer to the
                // question this page exists to answer, and right now the answer
                // is that we do not know.
                self.set_runtime(Runtime::Unreadable { message });

                // A licence that lapses while the window is open is the one
                // thing that can make this page contradict itself: `status`
                // starts saying "you need to activate a licence" every two
                // seconds while the group below goes on showing the licence we
                // read at start-up, offering nothing to do about it. Re-read
                // it — once, on the change.
                //
                // This cannot become a poll by the back door. It fires only
                // while the licence group still claims to be active, and the
                // read it triggers ends that claim whichever way it goes.
                if unlicensed && self.claims_active_licence() {
                    self.read_licence();
                }
            }
        }
    }

    /// Report every `status` read to `observer` — the tray's source of state.
    ///
    /// The second argument is whether the proxy is running and **bypassed**: a
    /// root helper positively seen to be dead, so nothing reaches the filters.
    /// It is passed separately because [`ProxyStatus`] cannot carry it —
    /// `status` reports what is configured and listening and nothing else, and
    /// that gap is the whole reason this page has a `Bypassed` state.
    ///
    /// Handing the tray only `ProxyStatus` was this check's blind spot: in
    /// `--background`, which the autostart entry runs at login, the tray is the
    /// only surface there is, and it would have gone on showing a healthy icon
    /// for the whole session.
    pub fn connect_status(&self, observer: impl Fn(&ProxyStatus, bool) + 'static) {
        self.observer.replace(Some(Box::new(observer)));
    }

    /// Where a click on a figure or a row here should take the user.
    ///
    /// Set by the window once the pages this can lead to exist. Until it is —
    /// and it never is on the licence-less startup path, where there are no
    /// pages at all — the links are inert rather than broken: they highlight and
    /// they can be pressed, and pressing one does nothing.
    pub fn connect_navigate(&self, navigate: impl Fn(Destination) + 'static) {
        self.navigate.replace(Some(Box::new(navigate)));
    }

    /// Ask the window to open the page behind whatever was just clicked.
    ///
    /// The borrow is held across the call, as [`Self::apply`] holds `observer`'s
    /// across its own. Safe for the same reason: what the window does in
    /// response — select a sidebar row, scroll a page — comes back into this
    /// page at `refresh_stats` and no further.
    fn go(&self, destination: Destination) {
        if let Some(navigate) = self.navigate.borrow().as_ref() {
            navigate(destination);
        }
    }

    /// Tell the page whether the main window is on screen, which sets the poll
    /// rate. Hiding the window does not stop polling: the tray still shows
    /// whether the proxy is up.
    pub fn set_window_visible(&self, visible: bool) {
        self.window_visible.set(visible);
    }

    /// Start the proxy, as the tray's "Start proxy" item does. Goes through the
    /// same act -> re-read -> reconcile path as the button.
    pub fn start_proxy(self: &Rc<Self>) {
        self.act(Action::Start);
    }

    pub fn stop_proxy(self: &Rc<Self>) {
        self.act(Action::Stop);
    }

    /// Restart the proxy, as the tray's "Restart proxy" item does — the item
    /// that only appears while the helper is dead, because a restart is the
    /// only thing measured to bring it back.
    pub fn restart_proxy(self: &Rc<Self>) {
        self.act(Action::Restart);
    }

    /// Clear a wedged leftover proxy process left behind by a previous session.
    ///
    /// Run once, when the application starts. The state it looks for outlives
    /// the application that was running when it happened — the leftover is
    /// reparented to `systemd --user` and sits there indefinitely — so the
    /// likeliest way to meet it is to come back to the machine and open this
    /// window, with no failed start to notice it by.
    ///
    /// # It clears, and stops there
    ///
    /// Deliberately no start afterwards, which is the one place this differs
    /// from [`Action::perform`]. There the user pressed *Start protection* and
    /// finishing the job is what they asked for. Here they opened a window, and
    /// a proxy that begins running because an application was launched is a
    /// decision this page has not been given. The panel will show it stopped,
    /// with a working button under it.
    ///
    /// # Nothing needs a snapshot here
    ///
    /// [`Action::perform`] compares against daemons listed before its own start
    /// because a start forks one that looks identical. Nothing has been started
    /// at this point, so every daemon found is by definition from before — and
    /// the `status` read still has to disagree with it before anything happens.
    pub fn sweep(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                let stranded = orphan::daemons(cli.binary());
                // The contradiction, in the order that costs least: no daemons
                // is the overwhelmingly common answer and it needs no `status`.
                // An unreadable status is not a licence to kill anything.
                if stranded.is_empty() || cli.status().is_ok_and(|status| status.running) {
                    return None;
                }
                match clear(&stranded) {
                    Some(pids) => Some(cleared_note(&pids, Outcome::NotAttempted)),
                    None => Some(couldnt_clear(&stranded)),
                }
            },
            move |note: Option<String>| {
                // Nothing found means nothing to say and nothing to re-read:
                // the page polls on its own timer and was about to anyway.
                let Some(note) = note else { return };
                this.toasts.add_toast(toast(&note));
                this.refresh();
            },
        );
    }

    fn start_polling(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(this) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let tick = this.ticks.get().wrapping_add(1);
            this.ticks.set(tick);

            let due = this.window_visible.get() || tick % HIDDEN_POLL_EVERY == 0;
            if due && !this.busy.get() {
                this.refresh();
            }
            glib::ControlFlow::Continue
        });
    }

    fn act(self: &Rc<Self>, action: Action) {
        self.busy.set(true);
        // Nothing is dropped here on purpose. A restart ends the run the log
        // verdict was scoped to, and `read_evidence` expires the cache on the
        // run's identity rather than on this page having pressed the button —
        // which is the only version that also covers a restart from a terminal.
        for b in [&self.primary, &self.restart] {
            b.set_sensitive(false);
        }

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || action.perform(&cli),
            move |(result, recovery): (Result<String, String>, Option<String>)| {
                this.busy.set(false);
                // The recovery first, because it explains the rest: a start that
                // needed one took a minute to get here, and a user who watched
                // that happen is owed the reason before the outcome.
                if let Some(note) = recovery {
                    this.toasts.add_toast(toast(&note));
                }
                if let Err(err) = result {
                    this.toasts.add_toast(toast(&err));
                }
                // act -> re-read -> reconcile: the command's own output is not
                // evidence that it worked (see docs/cli-contract.md §3).
                this.refresh();
            },
        );
    }

    /// What to say under the HTTP endpoint, or `None` when there is nothing to
    /// say about it.
    ///
    /// **This page reports a listener that is bound, and being bound is not the
    /// same as working.** `adguard-cli status` says only that the port is open;
    /// with AdGuard's root helper unmet, that port accepts connections, answers
    /// every one of them 502, and never opens an upstream socket at all
    /// (contract §8). So the endpoint this group exists to advertise was
    /// advertised as healthy in exactly the state where nothing at all could
    /// get through it, and the one row that explained why sat on the Advanced
    /// page, filed under a proxy mode the user had never selected.
    ///
    /// Read on every poll rather than cached, which is one `stat` beside a
    /// `status` invocation costing 10–30 ms — and it means the caveat clears
    /// itself within a tick of the user running the command, without the window
    /// needing to lose and regain focus the way the Advanced page's row does.
    ///
    /// It says the state and where to go, not which of the three properties is
    /// missing: that, and the command itself, belong to the page that owns them
    /// — the same division every other row on this page keeps.
    fn helper_caveat(&self) -> Option<&'static str> {
        match self.helper_path.as_ref().map(RootHelper::inspect)? {
            Ok(helper) if helper.is_set_up() => None,
            Ok(_) => Some(
                "Listening, but requests through it fail until AdGuard's root helper \
                 is set up — the Advanced page has the command",
            ),
            // Unreadable is not the same as unmet, and this row must not claim
            // the stronger of the two. The Advanced page reports the error.
            Err(_) => Some(
                "Listening. AdGuard's root helper could not be checked, and requests \
                 through here fail when it is not set up — see the Advanced page",
            ),
        }
    }

    fn apply(&self, status: &ProxyStatus, bypass: Option<Bypass>) {
        // A contradiction, and only a contradiction: a proxy `status` calls
        // running, and something else says nothing is getting through it.
        // Neither half means anything alone — a healthy install has a running
        // proxy, and an unknown helper is the answer on any machine whose
        // process tree this application does not recognise.
        //
        // `bypass` is `None` unless `status.running`, so the pairing is already
        // made by the time it arrives; see [`read_evidence`].
        self.set_runtime(match (status.running, bypass) {
            (true, Some(bypass)) => Runtime::Bypassed(bypass),
            (true, None) => Runtime::Up,
            (false, _) => Runtime::Down,
        });

        set_endpoint(
            &self.http,
            &self.http_value,
            status.http_proxy.as_deref(),
            self.helper_caveat(),
        );
        set_endpoint(
            &self.socks5,
            &self.socks5_value,
            status.socks5_proxy.as_deref(),
            // Measured unaffected: with the helper unmet the SOCKS5 proxy
            // serves requests normally while the HTTP one beside it fails
            // every single one (contract §8). Repeating the caveat here would
            // send a user to fix something that is not stopping them.
            None,
        );

        self.manual_dns.set(status.manual_dns_proxy);
        // Measured: with the redirect gone this setting reads on and does
        // nothing, so a green "Enabled" beside a panel reporting a bypass is the
        // one place two things on this page could be read as disagreeing.
        //
        // Narrowed to `auto` and to this row on purpose. It is the only setting
        // here a dead helper has been measured to stop — the two DNS rows are
        // left alone because nothing has measured what it does to them, and
        // guessing at that is the mistake `Bypassed` was given a mode to avoid.
        if matches!(
            &*self.runtime.borrow(),
            Runtime::Bypassed(Bypass::Helper { redirected: true })
        ) {
            self.system_filtering.set_stopped(status.system_wide_filtering);
        } else {
            self.system_filtering.set(status.system_wide_filtering);
        }
        self.system_dns.set(status.system_dns_filtering);

        if let Some(observer) = self.observer.borrow().as_ref() {
            observer(status, matches!(&*self.runtime.borrow(), Runtime::Bypassed(_)));
        }
    }

    // ---- the hero panel ----

    fn set_runtime(&self, runtime: Runtime) {
        self.runtime.replace(runtime);
        self.render_runtime();
    }

    /// What the primary button does right now, or `None` when the state it is
    /// showing does not license an action.
    fn primary_action(&self) -> Option<Action> {
        match &*self.runtime.borrow() {
            // Stopping is what the button offers in both, because in both the
            // proxy is up and stopping it is what that means. The *cure* for a
            // bypass is the Restart beside it, which is why that button is
            // shown in this state too.
            Runtime::Up | Runtime::Bypassed(_) => Some(Action::Stop),
            Runtime::Down => Some(Action::Start),
            // The button is insensitive in both, so this is belt-and-braces
            // rather than a reachable path — and it is the right answer if a
            // click ever did arrive: a `status` we could not read is no basis
            // for starting or stopping anything.
            Runtime::Unknown | Runtime::Unreadable { .. } => None,
        }
    }

    /// Render the panel from whatever this page last learned about the proxy.
    ///
    /// Every visual difference between the states is decided here — icon, tint,
    /// badge, wording, and the button's label, icon and class — so a state
    /// cannot end up half-rendered by one call site that forgot a field.
    fn render_runtime(&self) {
        let runtime = self.runtime.borrow();

        // (icon, tint, badge text, badge class, headline, detail, button)
        let (icon, tint, badge, badge_class, headline, detail, button) = match &*runtime {
            // The button is described but not enabled — `primary_action` returns
            // nothing in this state, which is what makes it insensitive below.
            // Describing it at all is for the ~20 ms before the first reading:
            // an empty button row would let the panel grow by a button's height
            // the instant `status` answered, on every start-up.
            Runtime::Unknown => (
                "security-medium-symbolic",
                None,
                None,
                None,
                "Checking…",
                "Reading the state of the local proxy.".to_owned(),
                Some(START_BUTTON),
            ),
            Runtime::Up => (
                "security-high-symbolic",
                Some(style::HERO_ON),
                Some("ACTIVE"),
                Some(style::BADGE_ON),
                "Protection is on",
                "The local proxy is running and your enabled modules are filtering traffic."
                    .to_owned(),
                Some(STOP_BUTTON),
            ),
            Runtime::Down => (
                "security-low-symbolic",
                Some(style::HERO_OFF),
                Some("STOPPED"),
                Some(style::BADGE_OFF),
                "Protection is off",
                "Nothing is being filtered. Start the proxy to put your modules back to work."
                    .to_owned(),
                Some(START_BUTTON),
            ),
            // Not a shade of `Up`. The proxy really is running, so the panel
            // must not say "off" either — what has stopped is the traffic
            // reaching it, and the wording is the only thing that can carry
            // that distinction.
            //
            // The cache sentence is not padding. Every page loaded while this
            // was true was fetched unfiltered and cached that way, so a restart
            // fixes the next request and leaves the ads already on disk —
            // measured on 2026-08-25, where a restart alone left 9gag serving
            // ads from Chrome's cache until it was cleared. A user told only to
            // restart would reasonably conclude the restart had not worked.
            Runtime::Bypassed(bypass) => (
                "dialog-warning-symbolic",
                Some(style::HERO_OFF),
                Some("NOT FILTERING"),
                Some(style::BADGE_OFF),
                match bypass {
                    Bypass::Helper { redirected: false } => "The HTTP proxy has stopped working",
                    _ => "Protection is not reaching your traffic",
                },
                match bypass {
                    // `auto`: the redirect is gone, so everything reaches the
                    // internet untouched and the cache fills with it.
                    Bypass::Helper { redirected: true } => {
                        "The proxy is running, but AdGuard's root helper has stopped and \
                         nothing is being filtered. Restart to recover — pages already \
                         loaded may keep their ads until you clear the browser's cache."
                    }
                    // `manual`: loud rather than silent, and only half the
                    // endpoints. Measured (contract §8) — so no cache advice,
                    // because nothing loaded unfiltered to be cached.
                    Bypass::Helper { redirected: false } => {
                        "The proxy is running, but AdGuard's root helper has stopped and \
                         requests through the HTTP proxy now fail. The SOCKS5 proxy is \
                         unaffected. Restart to recover."
                    }
                    // **Cause unknown, so the sentence claims no cause.** Both
                    // halves of what was seen are named — AdGuard's own checks
                    // failing, and nothing else arriving either — because
                    // together they are the evidence, and either alone would be
                    // consistent with a working proxy behind a dead upstream.
                    //
                    // The last sentence is the one that keeps this honest.
                    // `access.rs` can rule out a dead `filters.adtidy.org`
                    // while browsing works, because that leaves the log busy;
                    // it cannot rule out a machine that has been off the
                    // network altogether, which empties the log exactly as a
                    // bypass does. Naming that reading is what lets a user who
                    // knows they were offline dismiss this, instead of
                    // concluding the indicator lies.
                    //
                    // The cache advice is conditional here and unconditional
                    // above, and the difference is measurement: a dead helper
                    // was *seen* to leave a cache full of ads, and this state
                    // may not have loaded anything at all.
                    Bypass::Unreached => {
                        "The proxy is running, but for hours nothing has been getting \
                         through it — neither AdGuard's own hourly checks nor any other \
                         traffic. Restart to recover, and clear the browser's cache if \
                         pages still show ads afterwards. A machine that has been offline \
                         for hours looks the same from here."
                    }
                }
                .to_owned(),
                Some(STOP_BUTTON),
            ),
            // The CLI's own sentence, verbatim: it names the reason — a lapsed
            // licence, a timeout — where a sentence of ours could only say that
            // something went wrong.
            Runtime::Unreadable { message } => (
                "dialog-warning-symbolic",
                Some(style::HERO_UNKNOWN),
                Some("UNAVAILABLE"),
                Some(style::BADGE_UNKNOWN),
                "Protection state unknown",
                message.clone(),
                None,
            ),
        };

        self.shield.set_icon_name(Some(icon));
        // On the icon as well as the panel: a green shield over a red panel is
        // the one combination that could be read as "protected".
        swap_class(
            &self.shield,
            &["success", "warning", "error", "dim-label"],
            match &*runtime {
                Runtime::Unknown => Some("dim-label"),
                Runtime::Up => Some("success"),
                Runtime::Down => Some("warning"),
                // Red rather than the amber a deliberate stop gets. This one is
                // not a state the user chose.
                Runtime::Bypassed(_) => Some("error"),
                Runtime::Unreadable { .. } => Some("error"),
            },
        );
        swap_class(
            &self.hero,
            &[style::HERO_ON, style::HERO_OFF, style::HERO_UNKNOWN],
            tint,
        );

        // Absent rather than blank while the first reading is outstanding: an
        // empty pill would sit there as a smudge above the headline.
        self.badge.set_visible(badge.is_some());
        if let Some(text) = badge {
            self.badge.set_label(text);
        }
        swap_class(
            &self.badge,
            &[style::BADGE_ON, style::BADGE_OFF, style::BADGE_UNKNOWN],
            badge_class,
        );

        self.headline.set_label(headline);
        self.detail.set_label(&detail);

        match button {
            Some((label, icon, class)) => {
                self.primary_content.set_label(label);
                self.primary_content.set_icon_name(icon);
                swap_class(
                    &self.primary,
                    &["suggested-action", "destructive-action"],
                    Some(class),
                );
                // The single source of "is this clickable" is the same function
                // the click handler asks, so a button can never be sensitive in
                // a state that would drop its click on the floor.
                self.primary.set_sensitive(self.primary_action().is_some());
                self.primary.set_visible(true);
            }
            // Hidden rather than greyed out, and only for `Unreadable`: a
            // disabled "Start protection" beneath "Protection state unknown"
            // invites the user to keep clicking it.
            None => self.primary.set_visible(false),
        }

        // Shown in `Bypassed` as well as `Up`, and for a better reason than
        // symmetry: a restart is the *only* thing measured to clear a bypass,
        // so the state that reports one has to offer it.
        let restartable = matches!(&*runtime, Runtime::Up | Runtime::Bypassed(_));
        self.restart.set_visible(restartable);
        self.restart.set_sensitive(restartable);
    }

    // ---- the three figures ----

    fn apply_stats(&self, stats: &Stats) {
        self.set_modules(stats.modules);
        set_stat(&self.web_filters, stats.web_filters.map(enabled_count));
        set_stat(&self.dns_filters, stats.dns_filters.map(enabled_count));
    }

    fn set_modules(&self, modules: Option<(usize, usize)>) {
        set_stat(
            &self.modules,
            modules.map(|(on, total)| format!("{on} of {total}")),
        );
    }

    // ---- the licence ----

    /// Read `license` once. Never on a timer.
    fn read_licence(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || read_licence(&cli),
            move |result| match result {
                Ok(licence) => this.licence_read(licence),
                Err((unlicensed, message)) => this.licence_refused(unlicensed, message),
            },
        );
    }

    /// `license` answered.
    fn licence_read(&self, licence: License) {
        // An activation the user has not finished is not overwritten by a
        // reading that says what we already knew. Pressing the refresh button
        // to ask "did it work?" is the natural gesture at exactly that moment,
        // and it would otherwise take the log-in link and the finish button off
        // the screen and put the user back at the start of a flow they are in
        // the middle of.
        //
        // An *active* reading is the exception, and the whole point: that is
        // the answer the flow is waiting for, wherever it arrives from.
        let awaiting = matches!(*self.licence.borrow(), Licence::AwaitingLogin { .. });
        if awaiting && !licence.is_active() {
            return;
        }

        self.set_licence(Licence::Read(licence));
    }

    /// `license` would not answer.
    fn licence_refused(&self, unlicensed: bool, message: String) {
        // A refusal while the user is still logging in is the expected answer,
        // not news — it is what "not activated yet" looks like. Keep the link
        // and the finish button rather than sending them back to the start.
        let awaiting = matches!(*self.licence.borrow(), Licence::AwaitingLogin { .. });
        if awaiting {
            return;
        }

        self.set_licence(if unlicensed {
            Licence::Inactive { message }
        } else {
            Licence::Unreadable { message }
        });
    }

    /// Is the licence group currently telling the user their licence works?
    fn claims_active_licence(&self) -> bool {
        matches!(&*self.licence.borrow(), Licence::Read(licence) if licence.is_active())
    }

    fn set_licence(&self, licence: Licence) {
        self.licence.replace(licence);
        self.render_licence();
    }

    fn login_url(&self) -> Option<String> {
        match &*self.licence.borrow() {
            Licence::AwaitingLogin { url } => Some(url.clone()),
            _ => None,
        }
    }

    /// Ask the CLI where to send the user, then send them there.
    fn begin_activation(self: &Rc<Self>) {
        self.activate.set_sensitive(false);
        // The poll stands down for the same reason it does for start/stop, and
        // more urgently: `activate` is allowed 120 s for its completion leg
        // (`NETWORK_TIMEOUT`), and left running the poll would put up to sixty
        // `status` invocations into the same data directory alongside the one
        // command in this app that changes AdGuard's licensing state. Two
        // concurrent invocations are exactly the shape measured to make one of
        // them fail (contract §3).
        self.busy.set(true);
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.activate().map_err(|err| err.to_string()),
            move |result| {
                this.busy.set(false);
                this.activate.set_sensitive(true);
                match result {
                    Ok(Activation::NeedsLogin { url }) => this.open_login(url),
                    // A shape nobody has measured — an install that is already
                    // activated, most likely. Show what it said and let
                    // `license` settle where things actually stand, rather than
                    // reading anything into it.
                    Ok(Activation::Replied { message }) => {
                        this.toasts.add_toast(toast(&message));
                        this.read_licence();
                    }
                    Err(message) => this.toasts.add_toast(toast(&message)),
                }
            },
        );
    }

    /// Open the log-in page, and show the link either way.
    fn open_login(self: &Rc<Self>, url: String) {
        self.set_licence(Licence::AwaitingLogin { url: url.clone() });

        let launcher = gtk::UriLauncher::new(&url);
        // Given to the launcher so the browser — or the portal dialog asking
        // which one — comes up over this window rather than behind it.
        let window = self.page.root().and_downcast::<gtk::Window>();
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = launcher.launch_future(window.as_ref()).await {
                // Not a dead end: the row below is holding the same link.
                this.toasts.add_toast(toast(&format!(
                    "Could not open a browser ({err}). Copy the log-in link below \
                     and open it yourself"
                )));
            }
        });
    }

    /// The user says they have logged in.
    fn finish_activation(self: &Rc<Self>) {
        self.finish.set_sensitive(false);
        // As in `begin_activation`, and this is the leg that actually reaches
        // AdGuard's servers.
        self.busy.set(true);
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                // Once, and then the question is put to `license` instead.
                // `activate`'s own failure is kept only to explain a `license`
                // that still refuses: a timeout reaching AdGuard is worth more
                // to the user than "not activated yet".
                let attempt = cli.activate().err().map(|err| err.to_string());
                (attempt, read_licence(&cli))
            },
            move |(attempt, licence)| this.settle_activation(attempt, licence),
        );
    }

    /// Reconcile the page against what `license` now says.
    ///
    /// The same discipline as every write in this app: the command's own output
    /// is not evidence, so the state that decides is read back afterwards.
    fn settle_activation(
        self: &Rc<Self>,
        attempt: Option<String>,
        licence: Result<License, (bool, String)>,
    ) {
        self.busy.set(false);
        self.finish.set_sensitive(true);

        let refusal = match licence {
            Ok(licence) => {
                let active = licence.is_active();
                // Not `licence_read`: this is the one path that is *supposed*
                // to leave `AwaitingLogin`, whichever way the reading went —
                // the toast below has to be able to say "still not active".
                self.set_licence(Licence::Read(licence));
                self.toasts.add_toast(toast(if active {
                    "AdGuard is activated"
                } else {
                    "AdGuard answered, but the licence is still not active"
                }));
                // `status` was refused for want of a licence until a moment
                // ago, so the rows above this group are stale. Only those: the
                // filter catalogue is read from SQLite and was never gated, and
                // the Protection and Advanced pages read `proxy.yaml`.
                self.refresh();
                return;
            }
            Err((unlicensed, message)) => (unlicensed, message),
        };

        self.toasts.add_toast(toast(&match (attempt, refusal) {
            // `activate` itself failed. That names something the user can act
            // on — a network problem — where our own sentence would not.
            (Some(failure), _) => failure,
            (None, (true, _)) => "Not activated yet. Log in with the link below, then \
                                  choose Finish activation again"
                .to_owned(),
            // `license` failed for some reason other than the licence, which is
            // the CLI's to explain.
            (None, (false, message)) => message,
        }));
    }

    /// Render the licence group from whatever this page last learned.
    fn render_licence(&self) {
        let licence = self.licence.borrow();

        let (state, owner, key) = match &*licence {
            Licence::Unknown => ("Checking…".to_owned(), None, None),
            // The status word is AdGuard's, and anything other than active is
            // shown as it came: mapping an unrecognised status to "inactive"
            // would be a claim about the user's licence we cannot support.
            Licence::Read(licence) => (
                if licence.is_active() {
                    "Active".to_owned()
                } else {
                    licence.status.clone()
                },
                Some(licence.owner.clone()),
                Some(licence.masked_key()),
            ),
            Licence::Inactive { message } | Licence::Unreadable { message } => {
                (message.clone(), None, None)
            }
            Licence::AwaitingLogin { .. } => (
                "Waiting for you to log in. Choose Finish activation once you have"
                    .to_owned(),
                None,
                None,
            ),
        };

        self.licence_state.set_subtitle(&state);
        // Absent rather than empty: a licence reading that named no owner would
        // otherwise show a row with nothing in it.
        set_row(&self.licence_owner, owner.as_deref());
        set_row(&self.licence_key, key.as_deref());

        match &*licence {
            Licence::AwaitingLogin { url } => {
                self.licence_link.set_subtitle(url);
                self.licence_link.set_visible(true);
            }
            _ => self.licence_link.set_visible(false),
        }

        // Offered from exactly two states: a licence that says it is not
        // active, and one the CLI refused *because* it is not active. Never
        // from a reading that failed for some other reason — pointing
        // `activate` at an install whose licence was fine all along is the one
        // mistake this flow must not make.
        let inactive = match &*licence {
            Licence::Read(licence) => !licence.is_active(),
            Licence::Inactive { .. } => true,
            Licence::Unknown | Licence::Unreadable { .. } | Licence::AwaitingLogin { .. } => false,
        };
        self.activate.set_visible(inactive);
        self.finish
            .set_visible(matches!(&*licence, Licence::AwaitingLogin { .. }));
    }
}

/// Read the proxy status, keeping the one distinction that matters.
///
/// The variant, not just its wording: a `status` refused for want of a licence
/// is the one status failure that says something about a *different* part of
/// this page — see [`StatusPage::settle_status`].
fn read_status(cli: &Cli) -> Result<ProxyStatus, (bool, String)> {
    cli.status().map_err(classify)
}

/// A log verdict and the proxy run it was read in.
///
/// The pair, never the verdict alone: a verdict outlives the run it describes
/// only as a falsehood, and the start time is what a later reading compares
/// against to notice. `None` is *no cached verdict*, which is what a restart
/// leaves behind and what makes the next reading go to the log immediately.
type Cached = Option<(SystemTime, Filtering)>;

/// What [`read_evidence`] hands back: the bypass to render if there is one, the
/// verdict and run to remember, and whether the log was actually read this time
/// — which is what re-arms [`ACCESS_LOG_EVERY`].
type Evidence = (Option<Bypass>, Cached, bool);

/// Everything this page knows about a bypass, and the log verdict to remember.
///
/// **Both checks hang off one walk of `/proc`.** They ask different questions of
/// the same daemon — is its helper a corpse, and did its run ever get anything
/// through — and finding that daemon twice per tick would double the only part
/// of this that is not free.
///
/// Nothing is walked, read or parsed at all unless `status` claims a proxy to
/// ask about. A stopped proxy has no helper to miss and no run to date, and the
/// walk would be a few hundred file reads every two seconds for an answer
/// nothing would render.
///
/// **Exactly one daemon, or no opinion.** `status` says something is running;
/// with none found, or several, `/proc` cannot say which process it meant, and
/// a verdict read off the wrong one would be a guess. That is the same pairing
/// [`orphan`] documents for the mirror-image bug — the reading and the process
/// tree have to agree before either is acted on.
///
/// Runs on the worker thread beside `status`, because it is a walk of `/proc`
/// and a four-mebibyte read on a two-second timer and the main loop is drawing.
fn read_evidence(
    cli: &Cli,
    status: &Result<ProxyStatus, (bool, String)>,
    log_due: bool,
    cached: Cached,
) -> Evidence {
    if !matches!(status, Ok(status) if status.running) {
        return (None, None, false);
    }
    let found = orphan::daemons(cli.binary());
    let [daemon] = found.as_slice() else {
        return (None, None, false);
    };
    // An undatable run has no window to read and nothing to key a cache on, and
    // reading one unscoped would count a previous run's failures against it.
    let Some(started) = daemon.started_at() else {
        return (None, None, false);
    };
    // **The cache expires on identity, not only on age.** A verdict belongs to
    // the run it was read in, and a restart from anywhere — this page, a
    // terminal, `systemd` — starts a new one. Carrying the old verdict across
    // that boundary would leave a cured install reporting a bypass for up to
    // `ACCESS_LOG_EVERY`, over the very restart that fixed it.
    let carried = cached.filter(|(run, _)| *run == started);

    // The corpse first: it is cheaper, it is immediate, and it names the cause,
    // so a bypass it can explain is never reported as one nothing can — and the
    // log is not read at all, because nothing it could say would change this.
    if helper::process(daemon.pid()) == HelperProcess::Defunct {
        return (Some(Bypass::Helper { redirected: redirects_traffic() }), carried, false);
    }

    // Not due *and* still speaking for this run is the only way to skip the
    // read. A run this page has no verdict for is read now, whatever the
    // interval says, so a restart is answered on the next tick rather than five
    // minutes later.
    if let (false, Some((_, filtering))) = (log_due, carried) {
        return (bypass_of(filtering), carried, false);
    }
    let filtering = access::filtering(started);
    (bypass_of(filtering), Some((started, filtering)), true)
}

/// The one verdict a caller may act on, as the panel's evidence.
fn bypass_of(filtering: Filtering) -> Option<Bypass> {
    (filtering == Filtering::Bypassed).then_some(Bypass::Unreached)
}

/// Does this install redirect traffic into the proxy, rather than wait on its
/// ports?
///
/// Read **only** when the helper is dead, which is the only state whose wording
/// turns on it. `proxy.yaml` is nine kilobytes of YAML and this page's poll is
/// on a two-second timer; parsing it every tick to answer a question nothing
/// asks the rest of the time would be a real cost for no reading.
///
/// That it is read at the moment it is needed, rather than cached from
/// start-up, is what keeps it honest: `proxy_mode` is a key the CLI invites the
/// user to change by hand, and a panel describing the mode they used to be in
/// would be worse than one that said nothing.
fn redirects_traffic() -> bool {
    Config::load().is_ok_and(|config| config.redirects_traffic())
}

/// Read the licence, keeping the one distinction that matters.
///
/// "The licence is not active" and "the licence could not be read" are
/// different facts, and [`Error::Unlicensed`] is what tells them apart. Flatten
/// them both to a string here and the page could no longer tell whether
/// offering activation was safe.
///
/// [`Error::Unlicensed`]: adguard_core::Error::Unlicensed
fn read_licence(cli: &Cli) -> Result<License, (bool, String)> {
    cli.license().map_err(classify)
}

/// Was this failure the licence, in the CLI's own words?
fn classify(err: adguard_core::Error) -> (bool, String) {
    (
        matches!(err, adguard_core::Error::Unlicensed { .. }),
        err.to_string(),
    )
}

#[derive(Copy, Clone)]
enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    fn run(self, cli: &Cli) -> Result<String, adguard_core::Error> {
        match self {
            Action::Start => cli.start(),
            Action::Stop => cli.stop(),
            Action::Restart => cli.restart(),
        }
    }

    /// Does this action expect the proxy to be up when it returns?
    ///
    /// The question recovery hangs on: only a start that was supposed to leave
    /// the proxy running can be said to have *not taken*.
    fn expects_running(self) -> bool {
        matches!(self, Action::Start | Action::Restart)
    }

    /// Run the action, and clear a wedged leftover process if one turns out to
    /// have been in the way.
    ///
    /// Blocking, for a worker thread — worst case a failed start (60 s), a
    /// termination (under a second), and a second start.
    ///
    /// # The order of these steps is the safety argument
    ///
    /// The daemons are listed **before** the command runs. A start forks a
    /// daemon of its own, and one that is still finding its feet looks exactly
    /// like the wedged one — same binary, same command line (see
    /// [`adguard_core::orphan`]). Anything this function signals therefore has
    /// to have existed before we tried, which is what the snapshot establishes
    /// and [`Daemon::alive`] re-checks against pid reuse.
    ///
    /// The `status` re-read between the two is the other half. A leftover
    /// process is only *leftover* if the CLI disagrees that it is there, so a
    /// start that worked — or one whose status we could not read, which is no
    /// basis for killing anything — ends this here.
    ///
    /// Returns what the action said, and separately what was done about it.
    fn perform(self, cli: &Cli) -> (Result<String, String>, Option<String>) {
        let before = if self.expects_running() {
            orphan::daemons(cli.binary())
        } else {
            Vec::new()
        };

        let said = self.run(cli).map_err(|err| err.to_string());

        if !self.expects_running() || cli.status().is_ok_and(|status| status.running) {
            return (said, None);
        }

        // Alive, and alive since before the attempt. In a healthy install that
        // failed to start for some other reason this is empty, and the CLI's own
        // complaint stands as the only explanation — which is right, because
        // there is nothing here to blame.
        let stranded: Vec<_> = before.into_iter().filter(Daemon::alive).collect();
        if stranded.is_empty() {
            return (said, None);
        }

        let Some(cleared) = clear(&stranded) else {
            return (said, Some(couldnt_clear(&stranded)));
        };

        // One retry, not a loop. If a start still fails with the ports free,
        // the leftover was not the reason and trying again would only spend
        // another minute finding that out.
        let retried = self.run(cli).map_err(|err| err.to_string());
        let outcome = if cli.status().is_ok_and(|status| status.running) {
            Outcome::Running
        } else {
            Outcome::StillDown
        };
        (retried, Some(cleared_note(&cleared, outcome)))
    }
}

/// Ask every stranded process to exit. `None` if any of them would not.
///
/// All or nothing: they hold the same ports, so one survivor is enough to keep
/// the next start failing, and reporting a partial success would send the user
/// away believing it was fixed.
fn clear(stranded: &[Daemon]) -> Option<Vec<i32>> {
    // Collected rather than folded through `all`, which stops at the first
    // `false`: one process that will not go would leave the others running and
    // unasked, and they hold the same ports. Every one gets signalled, then the
    // results are judged.
    let ended: Vec<bool> = stranded.iter().map(Daemon::terminate).collect();
    ended
        .iter()
        .all(|ended| *ended)
        .then(|| stranded.iter().map(Daemon::pid).collect())
}

/// Where the proxy stood once the leftovers were cleared.
///
/// Three states, not a `bool`, because the start-up sweep never starts anything
/// and so has no answer to give — and "did not start" is a different sentence
/// from "was not asked to".
#[derive(Copy, Clone, PartialEq, Eq)]
enum Outcome {
    /// Cleared, started, and the proxy is up.
    Running,
    /// Cleared and started, and it still did not come up.
    StillDown,
    /// Cleared, and no start was attempted — the start-up sweep.
    NotAttempted,
}

/// What to say once the leftovers are gone.
///
/// Names the pids. They are meaningless to most users and precisely what the one
/// who reports this needs — and they are also the promise this application is
/// keeping: *these* processes, not every `adguard-cli` on the machine.
fn cleared_note(pids: &[i32], outcome: Outcome) -> String {
    let what = format!(
        "Cleared {} that AdGuard had lost track of ({})",
        plural(pids.len(), "a stopped proxy process", "stopped proxy processes"),
        pid_list(pids),
    );
    match outcome {
        Outcome::Running => format!("{what}. Protection is on."),
        // The leftovers were real and are gone, and the proxy still will not
        // come up. Two separate facts, and flattening them would misreport one.
        Outcome::StillDown => format!("{what}, but the proxy still would not start."),
        Outcome::NotAttempted => format!("{what}. Protection can be started now."),
    }
}

fn couldnt_clear(stranded: &[Daemon]) -> String {
    let pids: Vec<i32> = stranded.iter().map(Daemon::pid).collect();
    format!(
        "{} still holding the proxy ports and did not exit when asked ({})",
        plural(
            pids.len(),
            "A stopped AdGuard process is",
            "Stopped AdGuard processes are"
        ),
        pid_list(&pids),
    )
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 { one } else { many }.to_owned()
}

fn pid_list(pids: &[i32]) -> String {
    let label = plural(pids.len(), "pid", "pids");
    let pids: Vec<String> = pids.iter().map(i32::to_string).collect();
    format!("{label} {}", pids.join(", "))
}

/// One reading of the three figures under the panel.
///
/// `None` per figure rather than per read: the module count comes from
/// `proxy.yaml` and the two filter counts from separate SQLite files, so a
/// missing DNS catalogue must not take the other two figures down with it. It is
/// also the honest answer — a dash says "we could not read this", where a zero
/// would claim the user has nothing enabled.
struct Stats {
    /// Toggles switched on, and how many there are.
    modules: Option<(usize, usize)>,
    web_filters: Option<usize>,
    dns_filters: Option<usize>,
}

impl Stats {
    /// Runs on a worker: two file reads and two `COUNT(*)`s, no CLI.
    fn read() -> Self {
        Self {
            modules: Config::load().ok().as_ref().map(module_count),
            web_filters: count_enabled(FilterSet::Http),
            dns_filters: count_enabled(FilterSet::Dns),
        }
    }
}

/// How many protection modules are switched on, out of how many there are.
///
/// A toggle the config does not carry counts as off rather than as missing — the
/// Protection page is where "unavailable" is said properly, per row; a single
/// figure has nowhere to put the distinction, and "5 of 6" is the useful
/// summary either way.
fn module_count(config: &Config) -> (usize, usize) {
    let on = Toggle::ALL
        .iter()
        .filter(|toggle| config.toggle(**toggle) == Some(true))
        .count();
    (on, Toggle::ALL.len())
}

fn count_enabled(set: FilterSet) -> Option<usize> {
    Catalogue::open_set(set).ok()?.enabled_count().ok()
}

fn enabled_count(count: usize) -> String {
    format!("{count} enabled")
}

/// Show a figure, or a dash when it could not be read.
fn set_stat(label: &gtk::Label, value: Option<String>) {
    label.set_label(value.as_deref().unwrap_or(PLACEHOLDER));
}

fn stat_value() -> gtk::Label {
    let label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .label(PLACEHOLDER)
        .build();
    label.add_css_class(style::STAT_VALUE);
    // Tabular figures, so a count changing from 9 to 10 does not shift the
    // caption under it.
    label.add_css_class("numeric");
    label
}

/// One figure, as the way in to the page that counts it.
///
/// The tile is the button rather than something added beside it, because the
/// number is what the eye lands on and a separate affordance would be asking the
/// user to aim at the smaller of two things. `.flat` and the padding reset in
/// [`style`] leave a button that looks exactly like the tile did and behaves
/// like a button: hover, focus ring, Enter and Space.
fn stat_button(value: &gtk::Label, caption: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&stat_tile(value, caption))
        .tooltip_text(tooltip)
        .build();
    button.add_css_class("flat");
    button.add_css_class(style::STAT_BUTTON);
    // The tile is a figure and a caption, and a screen reader reading them out
    // is told a count and nothing about the button being a way anywhere. The
    // tooltip is the sentence that says so, so it is also the description.
    button.update_property(&[gtk::accessible::Property::Description(tooltip)]);
    button
}

fn stat_tile(value: &gtk::Label, caption: &str) -> gtk::Box {
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    tile.add_css_class(style::STAT);

    let caption = gtk::Label::builder()
        .label(caption)
        .halign(gtk::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .build();
    caption.add_css_class("dim-label");
    caption.add_css_class("caption");

    tile.append(value);
    tile.append(&caption);
    tile
}

/// A proxy endpoint: a name, an icon, an address to copy, and the way to its
/// port.
///
/// The address goes in a suffix rather than a subtitle so it can be selected
/// without the row swallowing the drag, and so it lines up down the right-hand
/// edge with the states in the group below. Selecting it still works now that
/// the row is activatable: the label handles its own clicks, and the arrow after
/// it is what the rest of the row is for.
fn endpoint_row(title: &str, tooltip: &str) -> (adw::ActionRow, gtk::Label) {
    let row = link_row(title);
    row.set_tooltip_text(Some(tooltip));
    row.add_prefix(&gtk::Image::from_icon_name("network-workgroup-symbolic"));

    let value = gtk::Label::builder()
        .label(PLACEHOLDER)
        .selectable(true)
        .valign(gtk::Align::Center)
        .build();
    value.add_css_class("dim-label");
    value.add_css_class("numeric");
    row.add_suffix(&value);
    row.add_suffix(&chevron());

    (row, value)
}

/// Render one endpoint, saying so in the subtitle when there is nothing to show.
///
/// **The row used to go insensitive instead, and that was right until it became
/// a link.** An insensitive row cannot be activated, so dimming it would take
/// the way to the port settings away in precisely the state where a user is
/// likeliest to be going to look at them — a proxy that is stopped, or a port
/// that has been set to -1. The dash on its own does not say which of those it
/// is either, so the subtitle is worth more than the dimming was.
fn set_endpoint(
    row: &adw::ActionRow,
    value: &gtk::Label,
    address: Option<&str>,
    caveat: Option<&str>,
) {
    value.set_label(address.unwrap_or(PLACEHOLDER));
    // Rather than hiding the row: the endpoints are what a user comes to this
    // group to write down, and a group that empties itself while the proxy is
    // stopped reads as "there are none" instead of "not right now".
    //
    // A caveat only qualifies an endpoint there is one to qualify. Nothing is
    // listening on a stopped proxy, so "requests through it fail" would be an
    // odd thing to tell someone whose proxy is not running — and it would bury
    // the reason they are actually looking at a dash.
    row.set_subtitle(match (address, caveat) {
        (Some(_), Some(caveat)) => caveat,
        (Some(_), None) => "",
        (None, _) => "Not listening",
    });
}

/// A row that leads somewhere when clicked.
///
/// `activatable` gives the row the hover, the pointer target and keyboard
/// activation but nothing to look at, so [`chevron`] goes on beside the value —
/// the pair GNOME uses everywhere for a row that opens something else, and the
/// only thing that tells a user this row is not the inert label it was.
///
/// What it leads to is left to the caller, because the two kinds of row here say
/// it differently: an endpoint has its address in the subtitle's place and says
/// it in a tooltip, and a state row has the room to say it outright.
fn link_row(title: &str) -> adw::ActionRow {
    let row = row(title, "");
    row.set_activatable(true);
    row
}

fn chevron() -> gtk::Image {
    let arrow = gtk::Image::from_icon_name("go-next-symbolic");
    arrow.add_css_class("dim-label");
    arrow.set_valign(gtk::Align::Center);
    arrow
}

/// A row whose value is one of two words, shown as a state rather than prose.
///
/// `leads_to` is a permanent subtitle naming the setting that decides the state,
/// and it is the row's whole answer to "so how do I change it?" — the state
/// itself is read from `status` and there is nothing here that could write it.
struct StateRow {
    row: adw::ActionRow,
    value: gtk::Label,
}

impl StateRow {
    fn new(title: &str, leads_to: &str) -> Self {
        let row = link_row(title);
        row.set_subtitle(leads_to);
        let value = gtk::Label::builder().valign(gtk::Align::Center).build();
        row.add_suffix(&value);
        row.add_suffix(&chevron());
        Self { row, value }
    }

    /// Colour carries the same fact as the word, never instead of it: "Enabled"
    /// and "Disabled" are both spelled out, so the row reads the same to someone
    /// who cannot tell the two greens apart.
    fn set(&self, on: bool) {
        self.render(on, true);
    }

    /// The same row, for a setting that reads on and is not doing anything.
    ///
    /// Only for something **measured** to have stopped it — see the call site.
    /// The word "Enabled" stays, because it is what the config says and
    /// reporting the config is this row's whole job; what is added is that it is
    /// not currently in effect.
    ///
    /// Added in words rather than by going grey, and that is the rule above
    /// rather than a preference: dimming alone would leave the fact carried by
    /// colour and nothing else, so a reader who cannot tell the greens apart
    /// would see "Enabled" and learn none of it. Colour still moves, and still
    /// says the same thing the word does.
    fn set_stopped(&self, on: bool) {
        self.render(on, false);
    }

    fn render(&self, on: bool, in_effect: bool) {
        self.value.set_label(state_word(on, in_effect));
        swap_class(
            &self.value,
            &["success", "dim-label"],
            Some(if on && in_effect { "success" } else { "dim-label" }),
        );
    }
}

/// What a state row says about a setting.
///
/// `in_effect` is only ever false for a setting that is on: a disabled setting
/// is not doing anything either way, and "Disabled, not in effect" would be
/// saying the same thing twice.
fn state_word(on: bool, in_effect: bool) -> &'static str {
    match (on, in_effect) {
        (true, true) => "Enabled",
        (true, false) => "Enabled, not in effect",
        (false, _) => "Disabled",
    }
}

/// Put a widget into exactly one of a set of mutually exclusive style classes.
///
/// Every state change on this page is a re-render of the whole state rather
/// than a diff, so the classes that do not apply have to come off — otherwise a
/// panel that has been green and is now amber carries both.
fn swap_class(widget: &impl IsA<gtk::Widget>, candidates: &[&str], wanted: Option<&str>) {
    let widget = widget.as_ref();
    for class in candidates {
        if Some(*class) != wanted {
            widget.remove_css_class(class);
        }
    }
    if let Some(class) = wanted {
        widget.add_css_class(class);
    }
}

/// Show a row's value, or hide the row when there is none.
///
/// An empty subtitle is not the same as no value: a licence reading that named
/// no owner would otherwise leave a labelled row with nothing beside it.
fn set_row(row: &adw::ActionRow, value: Option<&str>) {
    match value.filter(|value| !value.is_empty()) {
        Some(value) => {
            row.set_subtitle(value);
            row.set_visible(true);
        }
        None => row.set_visible(false),
    }
}

/// A row carrying text we did not write.
///
/// `AdwPreferencesRow:use-markup` defaults to true and the label is rendered as
/// the title is assigned, so markup goes off first — the same ordering every
/// other row in this app uses. It is not decoration here: these subtitles carry
/// the CLI's own sentences and the activation link, whose query string
/// (`?action=activate&app=cli&appid=…`) Pango would fail to parse.
fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule `StateRow` is written to: the word carries the fact, so the row
    /// still reads correctly with every style class stripped off it.
    ///
    /// A green "Enabled" beside a panel reporting a bypass is the contradiction
    /// this exists to remove, and removing it by going grey alone would have put
    /// the whole of the correction into a colour.
    #[test]
    fn a_setting_that_is_on_and_doing_nothing_says_both() {
        assert_eq!(state_word(true, true), "Enabled");
        assert_eq!(state_word(true, false), "Enabled, not in effect");

        // Still "Enabled" — the row reports the config, and the config is on.
        assert!(state_word(true, false).starts_with("Enabled"));
        // And the addition is words, not punctuation or a colour.
        assert_ne!(state_word(true, false), state_word(true, true));
    }

    /// "Disabled, not in effect" would be saying the same thing twice, so a
    /// setting that is off reads the same either way.
    #[test]
    fn a_setting_that_is_off_reads_the_same_either_way() {
        assert_eq!(state_word(false, true), "Disabled");
        assert_eq!(state_word(false, false), "Disabled");
    }
}
