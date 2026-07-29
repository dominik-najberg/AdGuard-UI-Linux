//! AdGuard UI — GTK4/libadwaita front-end for `adguard-cli`.
//!
//! Scaffold scope: one vertical slice through every layer — locate the CLI,
//! read real status off the main thread, render it, and control the proxy.
//! Feature pages (filters, protection toggles, advanced settings) come next;
//! see `docs/architecture.md`.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use adguard_core::{Cli, ProxyStatus};
use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use adw::prelude::*;

/// Must match the `.desktop` filename and its `StartupWMClass`, or GNOME
/// shows a second unbranded icon instead of grouping with the launcher.
const APP_ID: &str = "io.github.dominik-najberg.AdGuardUI";

const POLL_INTERVAL: Duration = Duration::from_secs(2);

const PLACEHOLDER: &str = "—";

/// Messages from worker threads back to the UI.
enum Msg {
    Status(Result<ProxyStatus, String>),
    /// Outcome of a start/stop/restart. The text is informational only —
    /// success is confirmed by re-reading status, never by exit code.
    Action(Result<String, String>),
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("AdGuard UI")
        .default_width(560)
        .default_height(620)
        .build();

    match Cli::discover() {
        Ok(cli) => window.set_content(Some(&main_view(cli))),
        Err(err) => window.set_content(Some(&missing_cli_view(&err.to_string()))),
    }

    window.present();
}

/// Shown when `adguard-cli` is not installed. Failing with an explanation
/// beats crashing, since the GUI is useless without the CLI.
fn missing_cli_view(message: &str) -> adw::ToolbarView {
    let status = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("AdGuard CLI not found")
        .description(message)
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&status));
    view
}

/// Widgets that reflect proxy state, grouped so refresh logic stays readable.
/// GTK widgets are reference-counted, so `Clone` is cheap.
#[derive(Clone)]
struct StatusView {
    state: adw::ActionRow,
    http: adw::ActionRow,
    socks5: adw::ActionRow,
    manual_dns: adw::ActionRow,
    system_filtering: adw::ActionRow,
    system_dns: adw::ActionRow,
    start: gtk::Button,
    stop: gtk::Button,
    restart: gtk::Button,
}

impl StatusView {
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
        self.system_dns.set_subtitle(on_off(status.system_dns_filtering));

        self.start.set_sensitive(!status.running);
        self.stop.set_sensitive(status.running);
        self.restart.set_sensitive(status.running);
    }

    /// Disable the controls while a command is in flight.
    fn set_busy(&self, busy: bool) {
        if busy {
            self.start.set_sensitive(false);
            self.stop.set_sensitive(false);
            self.restart.set_sensitive(false);
        }
    }

    fn show_error(&self, message: &str) {
        self.state.set_subtitle(message);
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "Enabled"
    } else {
        "Disabled"
    }
}

fn main_view(cli: Cli) -> adw::ToolbarView {
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

    let view = StatusView {
        state,
        http,
        socks5,
        manual_dns,
        system_filtering,
        system_dns,
        start: start.clone(),
        stop: stop.clone(),
        restart: restart.clone(),
    };

    let (tx, rx) = async_channel::unbounded::<Msg>();

    // Poll `status`; each call costs ~10 ms, so a 2 s timer is free.
    // There is no push/event mechanism in the CLI, so polling is the only option.
    let refresh: Rc<dyn Fn()> = {
        let cli = cli.clone();
        let tx = tx.clone();
        Rc::new(move || {
            let cli = cli.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = cli.status().map_err(|e| e.to_string());
                let _ = tx.send_blocking(Msg::Status(result));
            });
        })
    };

    // Suppress polling while a command runs, so a stale poll cannot overwrite
    // the busy state and re-enable the buttons mid-flight.
    let busy = Rc::new(Cell::new(false));

    for (button, action) in [
        (&start, Action::Start),
        (&stop, Action::Stop),
        (&restart, Action::Restart),
    ] {
        let cli = cli.clone();
        let tx = tx.clone();
        let view = view.clone();
        let busy = busy.clone();
        button.connect_clicked(move |_| {
            busy.set(true);
            view.set_busy(true);
            let cli = cli.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = action.run(&cli).map_err(|e| e.to_string());
                let _ = tx.send_blocking(Msg::Action(result));
            });
        });
    }

    glib::timeout_add_local(POLL_INTERVAL, {
        let refresh = refresh.clone();
        let busy = busy.clone();
        move || {
            if !busy.get() {
                refresh();
            }
            glib::ControlFlow::Continue
        }
    });

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&page));

    glib::spawn_future_local({
        let view = view.clone();
        let refresh = refresh.clone();
        let overlay = toast_overlay.clone();
        let busy = busy.clone();
        async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Msg::Status(Ok(status)) => view.apply(&status),
                    Msg::Status(Err(err)) => view.show_error(&err),
                    Msg::Action(result) => {
                        // act -> re-read -> reconcile: the command's own output
                        // is not evidence that it worked (see cli-contract.md).
                        busy.set(false);
                        if let Err(err) = result {
                            overlay.add_toast(adw::Toast::new(&err));
                        }
                        refresh();
                    }
                }
            }
        }
    });

    refresh();

    let refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_button.set_tooltip_text(Some("Refresh"));
    refresh_button.connect_clicked({
        let refresh = refresh.clone();
        move |_| refresh()
    });

    let header = adw::HeaderBar::new();
    header.pack_end(&refresh_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toast_overlay));
    toolbar
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

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build()
}
