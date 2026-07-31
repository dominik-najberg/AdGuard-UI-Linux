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
use std::rc::Rc;
use std::time::Duration;

use adguard_core::{Activation, Catalogue, Cli, Config, FilterSet, License, ProxyStatus, Toggle};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{style, toast, worker};

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
    manual_dns: StateRow,
    system_filtering: StateRow,
    system_dns: StateRow,

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

    /// Whether the main window is on screen. False while only the tray is.
    window_visible: Cell<bool>,
    /// Poll ticks since start, for the hidden-window rate.
    ticks: Cell<u32>,

    /// Notified after every successful `status` read.
    ///
    /// The tray renders the same runtime state this page does, and this is how
    /// it gets it — rather than polling `status` itself, which is what a second
    /// process had to do.
    observer: RefCell<Option<Box<dyn Fn(&ProxyStatus)>>>,
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
        for (value, caption) in [
            (&modules, "Protection modules"),
            (&web_filters, "Web filters"),
            (&dns_filters, "DNS filters"),
        ] {
            stats.append(&stat_tile(value, caption));
        }

        let stats_group = adw::PreferencesGroup::new();
        stats_group.add(&stats);

        // ---- the detail behind the answer ----

        let endpoint_group = adw::PreferencesGroup::builder()
            .title("Proxy endpoints")
            .description("Point applications at these local addresses to filter their traffic.")
            .build();
        let (http, http_value) = endpoint_row("HTTP");
        let (socks5, socks5_value) = endpoint_row("SOCKS5");
        for r in [&http, &socks5] {
            endpoint_group.add(r);
        }

        let filtering_group = adw::PreferencesGroup::builder().title("Filtering").build();
        let manual_dns = StateRow::new("Manual DNS proxy");
        let system_filtering = StateRow::new("System-wide filtering");
        let system_dns = StateRow::new("System-wide DNS filtering");
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

        for g in [
            &hero_group,
            &stats_group,
            &endpoint_group,
            &filtering_group,
            &licence_group,
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
            manual_dns,
            system_filtering,
            system_dns,
            licence_state,
            licence_owner,
            licence_key,
            licence_link,
            activate: activate.clone(),
            finish: finish.clone(),
            licence: RefCell::new(Licence::Unknown),
            busy: Cell::new(false),
            window_visible: Cell::new(true),
            ticks: Cell::new(0),
            observer: RefCell::new(None),
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
        let this = self.clone();
        worker::run(
            move || {
                let status = read_status(&cli);
                let licence = read_licence(&cli);
                (status, licence)
            },
            move |(status, licence)| {
                this.settle_status(status);
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
        let this = self.clone();
        worker::run(
            move || read_status(&cli),
            move |result| this.settle_status(result),
        );
    }

    /// Render one `status` reading, and notice when it contradicts the licence.
    fn settle_status(self: &Rc<Self>, result: Result<ProxyStatus, (bool, String)>) {
        match result {
            Ok(status) => self.apply(&status),
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
    pub fn connect_status(&self, observer: impl Fn(&ProxyStatus) + 'static) {
        self.observer.replace(Some(Box::new(observer)));
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
        for b in [&self.primary, &self.restart] {
            b.set_sensitive(false);
        }

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || action.run(&cli).map_err(|err| err.to_string()),
            move |result| {
                this.busy.set(false);
                if let Err(err) = result {
                    this.toasts.add_toast(toast(&err));
                }
                // act -> re-read -> reconcile: the command's own output is not
                // evidence that it worked (see docs/cli-contract.md §3).
                this.refresh();
            },
        );
    }

    fn apply(&self, status: &ProxyStatus) {
        self.set_runtime(if status.running {
            Runtime::Up
        } else {
            Runtime::Down
        });

        set_endpoint(&self.http, &self.http_value, status.http_proxy.as_deref());
        set_endpoint(
            &self.socks5,
            &self.socks5_value,
            status.socks5_proxy.as_deref(),
        );

        self.manual_dns.set(status.manual_dns_proxy);
        self.system_filtering.set(status.system_wide_filtering);
        self.system_dns.set(status.system_dns_filtering);

        if let Some(observer) = self.observer.borrow().as_ref() {
            observer(status);
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
            Runtime::Up => Some(Action::Stop),
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

        self.restart
            .set_visible(matches!(&*runtime, Runtime::Up));
        self.restart.set_sensitive(matches!(&*runtime, Runtime::Up));
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

/// A proxy endpoint: a name, an icon, and an address to copy.
///
/// The address goes in a suffix rather than a subtitle so it can be selected
/// without the row swallowing the drag, and so it lines up down the right-hand
/// edge with the states in the group below.
fn endpoint_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = row(title, "");
    row.add_prefix(&gtk::Image::from_icon_name("network-workgroup-symbolic"));

    let value = gtk::Label::builder()
        .label(PLACEHOLDER)
        .selectable(true)
        .valign(gtk::Align::Center)
        .build();
    value.add_css_class("dim-label");
    value.add_css_class("numeric");
    row.add_suffix(&value);

    (row, value)
}

/// Render one endpoint, dimming the row while the proxy is not listening.
fn set_endpoint(row: &adw::ActionRow, value: &gtk::Label, address: Option<&str>) {
    value.set_label(address.unwrap_or(PLACEHOLDER));
    // Rather than hiding the row: the endpoints are what a user comes to this
    // group to write down, and a group that empties itself while the proxy is
    // stopped reads as "there are none" instead of "not right now".
    row.set_sensitive(address.is_some());
}

/// A row whose value is one of two words, shown as a state rather than prose.
struct StateRow {
    row: adw::ActionRow,
    value: gtk::Label,
}

impl StateRow {
    fn new(title: &str) -> Self {
        let row = row(title, "");
        let value = gtk::Label::builder().valign(gtk::Align::Center).build();
        row.add_suffix(&value);
        Self { row, value }
    }

    /// Colour carries the same fact as the word, never instead of it: "Enabled"
    /// and "Disabled" are both spelled out, so the row reads the same to someone
    /// who cannot tell the two greens apart.
    fn set(&self, on: bool) {
        self.value.set_label(on_off(on));
        swap_class(
            &self.value,
            &["success", "dim-label"],
            Some(if on { "success" } else { "dim-label" }),
        );
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "Enabled"
    } else {
        "Disabled"
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
