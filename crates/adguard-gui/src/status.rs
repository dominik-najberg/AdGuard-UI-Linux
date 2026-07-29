//! The Status page: runtime state and lifecycle control.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use adguard_core::{Cli, ProxyStatus};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{toast, worker};

/// `status` costs ~10 ms and there is no event mechanism to subscribe to, so
/// polling is the only way to notice the proxy going down underneath us.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

const PLACEHOLDER: &str = "—";

pub struct StatusPage {
    page: adw::PreferencesPage,
    cli: Cli,
    toasts: adw::ToastOverlay,

    state: adw::ActionRow,
    http: adw::ActionRow,
    socks5: adw::ActionRow,
    manual_dns: adw::ActionRow,
    system_filtering: adw::ActionRow,
    system_dns: adw::ActionRow,
    start: gtk::Button,
    stop: gtk::Button,
    restart: gtk::Button,

    /// Set while a lifecycle command is in flight. Polling pauses so a reply
    /// that predates the command cannot re-enable the buttons mid-flight.
    busy: Cell<bool>,
}

impl StatusPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        let proxy_group = adw::PreferencesGroup::builder().title("Proxy").build();
        let state = row("Status", "Checking…");
        let http = row("HTTP proxy", PLACEHOLDER);
        let socks5 = row("SOCKS5 proxy", PLACEHOLDER);
        for r in [&state, &http, &socks5] {
            proxy_group.add(r);
        }

        let filtering_group = adw::PreferencesGroup::builder().title("Filtering").build();
        let manual_dns = row("Manual DNS proxy", PLACEHOLDER);
        let system_filtering = row("System-wide filtering", PLACEHOLDER);
        let system_dns = row("System-wide DNS filtering", PLACEHOLDER);
        for r in [&manual_dns, &system_filtering, &system_dns] {
            filtering_group.add(r);
        }

        let controls_group = adw::PreferencesGroup::new();
        let start = gtk::Button::with_label("Start");
        start.add_css_class("suggested-action");
        let stop = gtk::Button::with_label("Stop");
        let restart = gtk::Button::with_label("Restart");
        let button_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        for b in [&start, &stop, &restart] {
            b.set_sensitive(false);
            button_box.append(b);
        }
        controls_group.add(&button_box);

        for g in [&proxy_group, &filtering_group, &controls_group] {
            page.add(g);
        }

        let this = Rc::new(Self {
            page,
            cli,
            toasts,
            state,
            http,
            socks5,
            manual_dns,
            system_filtering,
            system_dns,
            start: start.clone(),
            stop: stop.clone(),
            restart: restart.clone(),
            busy: Cell::new(false),
        });

        for (button, action) in [
            (&start, Action::Start),
            (&stop, Action::Stop),
            (&restart, Action::Restart),
        ] {
            let this = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.act(action);
                }
            });
        }

        this.start_polling();
        this.refresh();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    pub fn refresh(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.status().map_err(|err| err.to_string()),
            move |result| match result {
                Ok(status) => this.apply(&status),
                // Keep it in the row rather than a toast: a failing `status`
                // repeats every two seconds and would bury the UI in toasts.
                Err(err) => this.state.set_subtitle(&err),
            },
        );
    }

    fn start_polling(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(this) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !this.busy.get() {
                this.refresh();
            }
            glib::ControlFlow::Continue
        });
    }

    fn act(self: &Rc<Self>, action: Action) {
        self.busy.set(true);
        for b in [&self.start, &self.stop, &self.restart] {
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
        self.state
            .set_subtitle(if status.running { "Running" } else { "Stopped" });
        self.http
            .set_subtitle(status.http_proxy.as_deref().unwrap_or(PLACEHOLDER));
        self.socks5
            .set_subtitle(status.socks5_proxy.as_deref().unwrap_or(PLACEHOLDER));
        self.manual_dns.set_subtitle(on_off(status.manual_dns_proxy));
        self.system_filtering
            .set_subtitle(on_off(status.system_wide_filtering));
        self.system_dns
            .set_subtitle(on_off(status.system_dns_filtering));

        self.start.set_sensitive(!status.running);
        self.stop.set_sensitive(status.running);
        self.restart.set_sensitive(status.running);
    }
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

fn on_off(value: bool) -> &'static str {
    if value {
        "Enabled"
    } else {
        "Disabled"
    }
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build()
}
