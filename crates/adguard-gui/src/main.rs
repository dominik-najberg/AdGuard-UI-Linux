//! AdGuard UI — GTK4/libadwaita front-end for `adguard-cli`.
//!
//! Reads state from AdGuard's own files (`proxy.yaml`, `agflm_*.db`) and the
//! `status` command; writes only ever go through `adguard-cli`. See
//! `docs/architecture.md` for the split and `docs/cli-contract.md` for the
//! measured CLI behaviour the wrapper encodes.

mod about;
mod advanced;
mod autostart;
mod backup;
mod browser_integration;
mod certificate;
mod dns;
mod filter_settings;
mod filters;
mod protection;
mod root_helper;
mod setup;
mod status;
mod style;
mod watch;
mod worker;

use std::cell::RefCell;
use std::rc::Rc;

use adguard_core::{Cli, Toggle, ADVANCED, STEALTH};
use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

/// Must match the `.desktop` filename and its `StartupWMClass`, or GNOME
/// shows a second unbranded icon instead of grouping with the launcher.
///
/// It is also the tray's `id`, and it is what gives the process its
/// single-instance behaviour: launching `adguard-ui` a second time activates
/// this one instead of starting a rival that would write the same config.
const APP_ID: &str = "io.github.dominik-najberg.AdGuardUI";

/// `--background`: register the tray and leave the window closed.
///
/// What the autostart entry in `data/autostart/` runs, so the tray is there
/// from login without a window opening in the user's face. The launcher entry
/// in `data/` keeps its plain `Exec`, because clicking a dock icon means the
/// opposite.
///
/// It is also what the *Start at login* switch on the Advanced page writes into
/// the entry it creates (see [`autostart`]) — one flag, spelled one way, for
/// what is one behaviour.
const BACKGROUND: &str = "background";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        // `--background` has to reach whichever instance acts on it. Without
        // this the flag is parsed and dropped by the launching process, so a
        // second `adguard-ui --background` — autostart racing a manual launch,
        // or a session restoring both — arrives at the running one as a bare
        // "activate" and pulls the window on screen, which is the single thing
        // the flag asks us not to do. It is also the only place GApplication
        // offers to set an exit status.
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.add_main_option(
        BACKGROUND,
        glib::Char::from(b'b'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start with only the tray icon, leaving the window closed",
        None,
    );

    // Built by the first launch that gets this far, and kept. Activation used
    // to build the whole UI unconditionally, which was invisible while the only
    // way in was a launcher click on a process that was not running yet;
    // `--background` makes a second activation the normal case, and each one
    // would otherwise raise a rival window with its own poll timer and its own
    // tray registration.
    let ui: Rc<RefCell<Option<Instance>>> = Rc::new(RefCell::new(None));

    // Activation with no command line behind it: `gapplication launch`, or the
    // shell calling org.freedesktop.Application.Activate on a running process.
    // Always means "show me".
    app.connect_activate({
        let ui = ui.clone();
        move |app| {
            if let Err(reason) = start(app, &ui, false) {
                eprintln!("adguard-ui: {reason}");
            }
        }
    });

    app.connect_command_line(move |app, cmdline| {
        let background = cmdline.options_dict().contains(BACKGROUND);
        match start(app, &ui, background) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(reason) => {
                // Our own stderr, not the caller's: this handler runs in the
                // running process when there is one, but the only failure it
                // can report comes from building the UI, which only ever
                // happens in the process that is starting up.
                eprintln!("adguard-ui: {reason}");
                glib::ExitCode::FAILURE
            }
        }
    });

    app.run()
}

/// What the first launch built, so a later one has something to present.
struct Instance {
    window: adw::ApplicationWindow,
    /// `None` when `adguard-cli` is missing: the window explains why, and there
    /// are no pages behind it. Also `None` while the first-run assistant has the
    /// window, since the pages it would hold cannot be built yet.
    view: Option<MainView>,
    /// The first-run assistant, for as long as it owns the window.
    ///
    /// Held here because the buttons inside it deliberately do **not** hold it
    /// — that would be a reference cycle through its own widget tree — which
    /// leaves this the only strong reference. Without it the assistant would be
    /// freed the moment [`start`] returned and every button in it would be
    /// inert. Dropped by [`install_main_view`], which is what replaces it.
    setup: Option<Rc<setup::SetupAssistant>>,
}

impl Instance {
    /// Put the window on screen, and take the Status page back to the 2 s poll
    /// rate — it drops to 10 s while only the tray is showing.
    fn present(&self) {
        self.window.present();
        if let Some(view) = &self.view {
            view.status.set_window_visible(true);
        }
    }
}

/// Bring the application up, or bring it forward if it is already up.
///
/// `background` is the autostart case: build everything, register the tray, and
/// present nothing. Because the window is what a user would otherwise reach the
/// app by, that is the one situation where a tray icon failing to register is
/// fatal — the inverse of the rule everywhere else, so it is spelled out below
/// rather than left to be inferred.
fn start(
    app: &adw::Application,
    ui: &Rc<RefCell<Option<Instance>>>,
    background: bool,
) -> Result<(), String> {
    if let Some(instance) = ui.borrow().as_ref() {
        // Launched again. Under `--background` that is the autostart entry
        // arriving second and it must not drag the window on screen; anything
        // else is someone asking for exactly that.
        if !background {
            instance.present();
        }
        return Ok(());
    }

    // Before any widget exists, so nothing is ever drawn once unstyled and then
    // restyled a frame later.
    style::install();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("AdGuard UI")
        .default_width(880)
        .default_height(720)
        .build();

    let cli = Cli::discover();

    // A machine where `adguard-cli configure` has never run has no
    // `proxy.yaml`, and until it does, `config set` refuses every real key
    // (contract §5) — so every page behind this would render "unavailable" and
    // nothing the user touched would stick. The assistant is the only thing
    // that can move that state, so it takes the window until it has.
    let needs_setup = cli
        .as_ref()
        .ok()
        .and_then(Cli::config_path)
        .is_some_and(|path| !path.is_file());

    if needs_setup && background {
        // There is no window to run the assistant in, and a tray built over a
        // configuration that does not exist would offer six toggles that
        // cannot be read and a proxy that cannot start. The same judgement as
        // the no-tray case below, for the same reason: better to say why than
        // to sit in the background being useless.
        window.destroy();
        return Err(format!(
            "AdGuard CLI has not been configured yet, so there is nothing for the tray to \
             show. Start without --{BACKGROUND} to finish setting it up."
        ));
    }

    if needs_setup {
        let cli = cli.expect("needs_setup is only true when the CLI was found");
        let toasts = adw::ToastOverlay::new();
        let assistant = setup::SetupAssistant::new(cli.clone(), toasts.clone());
        toasts.set_child(Some(assistant.widget()));
        window.set_content(Some(&toasts));
        window.present();

        // The instance exists from here so a second activation presents this
        // window rather than building a rival one — the assistant is as
        // single-instance as the pages are, and two of them would race to run
        // `configure` against the same directory.
        ui.replace(Some(Instance {
            window: window.clone(),
            view: None,
            setup: Some(assistant.clone()),
        }));

        assistant.connect_finished({
            let app = app.clone();
            let window = window.clone();
            let ui = ui.clone();
            move || install_main_view(&app, &window, &ui, &cli)
        });
        return Ok(());
    }

    let view = match cli {
        Ok(cli) => {
            let view = main_view(&cli);
            window.set_content(Some(&view.root));
            connect_focus_rechecks(&window, &view);
            Ok(view)
        }
        Err(err) => {
            window.set_content(Some(&missing_cli_view(&err.to_string())));
            Err(err.to_string())
        }
    };

    if background {
        // Nothing will be presented, so the Status page starts at the
        // tray-only poll rate instead of arriving there after a first hide.
        if let Ok(view) = &view {
            view.status.set_window_visible(false);
        }
    } else {
        // Before the tray, so a slow or absent StatusNotifierItem host cannot
        // delay the window appearing.
        window.present();
    }

    let tray = match &view {
        Ok(view) => connect_tray(app, &window, view),
        // No tray without a working CLI: every menu item would be inert, and
        // the window is already explaining why.
        Err(reason) => Err(reason.clone()),
    };

    // The *Start at login* switch offers a windowless start, which needs
    // somewhere for the application to appear. Whether there is one is settled
    // here and nowhere else, so the page is told rather than left to guess.
    if let Ok(view) = &view {
        view.advanced.set_tray_available(tray.is_ok());
    }

    if let Err(reason) = tray {
        if background {
            // Nothing on screen and nothing on the bus: the process could not
            // be reached, quit, or even noticed. Say so and stop, rather than
            // linger where only a process list would find us.
            window.destroy();
            return Err(format!(
                "--{BACKGROUND} leaves the window closed and there is no tray either \
                 ({reason}). Start it without --{BACKGROUND} to use the app in a window."
            ));
        }
        // A normal outcome, not a failure: GNOME has no native tray, so this is
        // what a missing or disabled AppIndicator extension looks like. Carry
        // on windowed — quitting because an icon could not be drawn would be
        // far worse than going without it.
        eprintln!("adguard-ui: continuing without a tray icon ({reason})");
    }

    ui.replace(Some(Instance {
        window,
        view: view.ok(),
        setup: None,
    }));
    Ok(())
}

/// Put the real UI in the window once the first-run assistant is done with it.
///
/// The tail of [`start`] that could not run at launch: with no `proxy.yaml`
/// there were no pages to build and nothing for a tray to show. Everything here
/// happens in the order it does at a normal launch — pages, then tray — and the
/// tray's registration failure stays the ordinary non-fatal outcome, because by
/// definition there is a window on screen at this point.
fn install_main_view(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    ui: &Rc<RefCell<Option<Instance>>>,
    cli: &Cli,
) {
    let view = main_view(cli);
    window.set_content(Some(&view.root));
    connect_focus_rechecks(window, &view);

    let tray = connect_tray(app, window, &view);
    view.advanced.set_tray_available(tray.is_ok());
    if let Err(reason) = tray {
        eprintln!("adguard-ui: continuing without a tray icon ({reason})");
    }

    if let Some(instance) = ui.borrow_mut().as_mut() {
        instance.view = Some(view);
        // The assistant's last strong reference. Dropping it here rather than
        // leaving it parked is what keeps a finished wizard from holding its
        // whole widget tree for the life of the process.
        instance.setup = None;
    }
}

/// Register the tray icon and connect it to the pages.
///
/// The tray is a view onto this process rather than a process of its own; see
/// `adguard_tray` for why. Three things are wired here:
///
/// - **state out** — the Status page's existing 2 s `status` poll and the
///   Protection page's config reads are forwarded to the tray, so it adds no
///   polling of its own.
/// - **commands in** — menu activations arrive on a channel and are dispatched
///   to the same page methods a click uses, so a tray toggle and the switch on
///   the page cannot disagree.
/// - **lifetime** — with a tray, closing the window hides it and the app keeps
///   running; without one, closing quits as usual.
///
/// Returns why there is no tray, rather than deciding what that means: with a
/// window on screen it is a line on stderr, and under `--background` it is the
/// end of the process. [`start`] holds that judgement.
fn connect_tray(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    view: &MainView,
) -> Result<(), String> {
    let tray = match adguard_tray::spawn(adguard_tray::State::default()) {
        Ok(tray) => tray,
        Err(err) => return Err(err.to_string()),
    };
    let tray = Rc::new(tray);

    // The two halves of the tray's state arrive from different pages, so they
    // are accumulated here and pushed as a whole. `set_state` drops an
    // unchanged state, which matters because the status poll delivers an
    // identical one every 2 s.
    let state = Rc::new(RefCell::new(adguard_tray::State::default()));

    view.status.connect_status({
        let tray = tray.clone();
        let state = state.clone();
        move |status| {
            state.borrow_mut().running = status.running;
            let snapshot = state.borrow().clone();
            tray.set_state(snapshot);
        }
    });

    view.protection.connect_config({
        let tray = tray.clone();
        let state = state.clone();
        move |config| {
            // Positional against `Toggle::ALL`, and `None` for a key that could
            // not be read — the tray shows those items insensitive rather than
            // unchecked, as the page shows them "unavailable".
            state.borrow_mut().toggles = Toggle::ALL.iter().map(|t| config.toggle(*t)).collect();
            let snapshot = state.borrow().clone();
            tray.set_state(snapshot);
        }
    });

    // Keep the application alive with no window on screen. Held only when a
    // tray exists; otherwise there would be no way left to reach or quit it.
    let hold = app.hold();

    window.connect_close_request({
        let status = view.status.clone();
        move |window| {
            window.set_visible(false);
            status.set_window_visible(false);
            glib::Propagation::Stop
        }
    });

    glib::spawn_future_local({
        let commands = tray.commands().clone();
        let app = app.clone();
        let window = window.clone();
        let status = view.status.clone();
        let protection = view.protection.clone();
        async move {
            // Owned by this future, so the hold lasts exactly as long as the
            // tray is being served.
            let _hold = hold;

            while let Ok(command) = commands.recv().await {
                use adguard_tray::Command;
                match command {
                    Command::ShowWindow => {
                        window.present();
                        status.set_window_visible(true);
                    }
                    Command::StartProxy => status.start_proxy(),
                    Command::StopProxy => status.stop_proxy(),
                    Command::SetToggle { toggle, on } => protection.request(toggle, on),
                    Command::Quit => {
                        app.quit();
                        return;
                    }
                }
            }
        }
    });

    Ok(())
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

/// Sidebar entries, in order. The id doubles as the stack child name.
const PAGES: [Page; 7] = [
    Page {
        id: "status",
        title: "Status",
        icon: "network-transmit-receive-symbolic",
    },
    Page {
        id: "protection",
        title: "Protection",
        icon: "security-high-symbolic",
    },
    Page {
        id: "filters",
        title: "Filters",
        icon: "view-list-symbolic",
    },
    Page {
        id: "dns",
        title: "DNS",
        icon: "network-server-symbolic",
    },
    Page {
        id: "stealth",
        title: "Stealth",
        icon: "view-conceal-symbolic",
    },
    Page {
        id: "advanced",
        title: "Advanced",
        icon: "emblem-system-symbolic",
    },
    // Last, as #4 asked. It is the only page that is about the installation
    // rather than about what the installation is doing, so it sorts below every
    // page that answers a question about protection.
    Page {
        id: "about",
        title: "About",
        icon: "help-about-symbolic",
    },
];

struct Page {
    id: &'static str,
    title: &'static str,
    icon: &'static str,
}

/// Where a click on the Status page asks to be taken.
///
/// That page answers one question — *am I protected?* — and every figure and row
/// on it is the readable end of a setting that is written somewhere else. A
/// number that counts something is a question about it ("four enabled, out of
/// what?") and a row that reads "Disabled" is a question about how to change it;
/// both answers are a page away, and until now the user had to know which page.
///
/// These name the other end. The Status page picks one and knows nothing more —
/// which page holds which setting, how the sidebar and the stack are kept in
/// step, and what "arriving" means once there, are all [`main_view`]'s business.
#[derive(Clone, Copy)]
pub enum Destination {
    /// The six protection modules.
    Protection,
    /// The HTTP/HTTPS filter catalogue.
    WebFilters,
    /// The DNS page, for its catalogue.
    DnsFilters,
    /// The DNS page, at the local DNS proxy's own controls.
    DnsProxy,
    /// The Advanced page, at the group holding this setting.
    Advanced(&'static str),
    /// The Advanced page, at the *Start at login* switch.
    ///
    /// Its own variant rather than an [`Self::Advanced`] carrying a key,
    /// because that group is not in the settings table at all — it writes a
    /// desktop entry, not `proxy.yaml`, so there is no key to name it by and
    /// `AdvancedPage::reveal` could not find it.
    Autostart,
}

impl Destination {
    /// Which of [`PAGES`] this leads to.
    fn page(self) -> &'static str {
        match self {
            Self::Protection => "protection",
            Self::WebFilters => "filters",
            Self::DnsFilters | Self::DnsProxy => "dns",
            Self::Advanced(_) | Self::Autostart => "advanced",
        }
    }

    /// The sidebar row to select, which is how every other part of the window
    /// finds out where we went.
    ///
    /// `None` is unreachable — [`Self::page`] returns ids from [`PAGES`] and a
    /// test pins that — and is an `Option` rather than an `expect` because the
    /// cost of being wrong should be a link that does nothing, not a window that
    /// disappears mid-click.
    fn page_index(self) -> Option<usize> {
        PAGES.iter().position(|page| page.id == self.page())
    }
}

/// The window's content, plus the pages the tray needs to reach.
///
/// The pages were local to `main_view` while the only thing that drove them was
/// a click inside the window. The tray dispatches to the same methods, so they
/// have to be reachable from where it is wired up.
struct MainView {
    root: adw::NavigationSplitView,
    status: Rc<status::StatusPage>,
    protection: Rc<protection::ProtectionPage>,
    /// Held for the root-helper re-check, which needs the window and so cannot
    /// be wired up in `main_view` itself.
    advanced: Rc<advanced::AdvancedPage>,
    /// The `proxy.yaml` subscription. Dropping it ends the subscription, so it
    /// lives exactly as long as the pages it reconciles — and it is what keeps
    /// the Advanced page reachable, since nothing else here holds one.
    _watch: Option<watch::ConfigWatch>,
}

fn main_view(cli: &Cli) -> MainView {
    let toasts = adw::ToastOverlay::new();

    let status = status::StatusPage::new(cli.clone(), toasts.clone());
    // Before anything else asks the CLI for anything: an install can be left
    // holding a proxy process the CLI has lost track of, and in that state
    // `start` fails for a minute and `stop` does nothing at all. Clearing it is
    // this application's to do — the process belongs to the user running it and
    // needs no privilege to end. See `adguard_core::orphan`.
    //
    // Costs one `/proc` scan on a healthy machine, and stops there without
    // running the CLI at all.
    status.sweep();
    let protection = protection::ProtectionPage::new(cli.clone(), toasts.clone());
    // The HTTP catalogue plus the one `proxy.yaml` setting that writes it:
    // `auto_enable_language_filters` adds and enables language filters on its
    // own, so the page it changes is where its brake belongs (contract §6).
    let filters = filter_settings::FilterSettingsPage::new(cli.clone(), toasts.clone());
    // The DNS catalogue plus the `dns_filtering` settings around it, on one
    // page: its user-rules row cannot go through `dns filters enable`
    // (contract §6), so that page owns the row and writes it with the list
    // commands instead.
    let dns = dns::DnsPage::new(cli.clone(), toasts.clone());
    let advanced = advanced::AdvancedPage::new(cli.clone(), toasts.clone(), &ADVANCED);
    // The same page against a different table: 26 settings behind the one
    // stealth switch Protection shows (handoff §3 gap 4). The master switch
    // stays on Protection, so there is still exactly one writer for that key.
    let stealth = advanced::AdvancedPage::new(cli.clone(), toasts.clone(), &STEALTH);
    // The two version numbers, and the one control that reaches AdGuard's
    // servers because the user asked it to (#4).
    let about = about::AboutPage::new(cli.clone(), toasts.clone());

    let stack = gtk::Stack::new();
    stack.add_named(status.widget(), Some(PAGES[0].id));
    stack.add_named(protection.widget(), Some(PAGES[1].id));
    stack.add_named(&filters.widget(), Some(PAGES[2].id));
    stack.add_named(&dns.widget(), Some(PAGES[3].id));
    stack.add_named(stealth.widget(), Some(PAGES[4].id));
    stack.add_named(advanced.widget(), Some(PAGES[5].id));
    stack.add_named(about.widget(), Some(PAGES[6].id));
    toasts.set_child(Some(&stack));

    // `check-update` rewrites the filter databases, and **nothing watches
    // them**: `watch.rs` monitors `proxy.yaml` alone, so an update that moved a
    // catalogue would otherwise sit behind a page still showing the old one
    // until something else rebuilt it. (`architecture.md` §3 claimed those files
    // were watched; they never have been, and this is the change that had to
    // find out.)
    //
    // The About page reports what moved and reaches into nothing — the same
    // division as `StatusPage::connect_navigate`, where the page names what it
    // wants and the window knows which page holds it. Weak, because the sidebar
    // holds a strong `about` and a strong capture here would close the loop.
    about.connect_checked({
        let filters = Rc::downgrade(&filters);
        let dns = Rc::downgrade(&dns);
        move |report: &adguard_core::UpdateReport| {
            // Only on a reported change. A component the CLI did not mention, or
            // one that failed, has moved nothing worth re-reading — and a
            // rebuild would discard the catalogue's scroll position for nothing.
            if report.changed(&adguard_core::UpdatePart::Filters) {
                if let Some(filters) = filters.upgrade() {
                    filters.reload();
                }
            }
            if report.changed(&adguard_core::UpdatePart::DnsFilters) {
                if let Some(dns) = dns.upgrade() {
                    dns.reload();
                }
            }
        }
    });

    let content_header = adw::HeaderBar::new();
    let content_title = adw::WindowTitle::new(PAGES[0].title, "");
    content_header.set_title_widget(Some(&content_title));

    // Refreshes whichever page is showing: status re-runs `status` and re-reads
    // the licence, filters re-reads the catalogue. Every page also refreshes
    // itself after any change it makes; this is for changes made behind our
    // back, from a terminal.
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh"));
    refresh.connect_clicked({
        let stack = stack.clone();
        let status = status.clone();
        let protection = protection.clone();
        let filters = filters.clone();
        let advanced = advanced.clone();
        let stealth = stealth.clone();
        let dns = dns.clone();
        let about = about.clone();
        move |_| match stack.visible_child_name().as_deref() {
            Some("protection") => protection.reload(),
            Some("filters") => filters.reload(),
            Some("dns") => dns.reload(),
            Some("stealth") => stealth.reload(),
            Some("advanced") => advanced.reload(),
            // Re-reads the CLI's version and nothing else. Deliberately does
            // **not** run `check-update`: on every other page this button is a
            // cheap re-read, and here that would make it a network fetch with
            // side effects on the user's filters.
            Some("about") => about.reload(),
            _ => status.reload(),
        }
    });
    content_header.pack_end(&refresh);

    let content = adw::ToolbarView::new();
    content.add_top_bar(&content_header);
    content.set_content(Some(&toasts));

    let split = adw::NavigationSplitView::builder()
        .min_sidebar_width(180.0)
        .max_sidebar_width(240.0)
        .build();
    split.set_content(Some(&adw::NavigationPage::new(&content, "AdGuard UI")));

    let sidebar = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    sidebar.add_css_class("navigation-sidebar");
    for page in &PAGES {
        sidebar.append(&sidebar_row(page));
    }
    sidebar.connect_row_selected({
        let stack = stack.clone();
        let split = split.clone();
        let status = status.clone();
        move |_, row| {
            let Some(row) = row else { return };
            let page = &PAGES[row.index().max(0) as usize];
            stack.set_visible_child_name(page.id);
            content_title.set_title(page.title);
            // The Status page's three figures count things the *other* pages
            // change, and nothing signals a switch flip across pages. Coming
            // back to it is therefore the moment to recount — cheap, because
            // those figures are read from `proxy.yaml` and the two catalogues
            // rather than from `adguard-cli`, so this cannot race the 2 s poll.
            if page.id == PAGES[0].id {
                status.refresh_stats();
            }
            // On a narrow window the sidebar and content are separate views;
            // choosing a page should move to it.
            split.set_show_content(true);
        }
    });
    sidebar.select_row(sidebar.row_at_index(0).as_ref());

    // The Status page's links, resolved. Routed through the sidebar rather than
    // straight at the stack so that everything hanging off `row-selected`
    // follows — the sidebar highlight, the header title, the recount of the
    // figures, and the narrow-window move to the content pane. Switching the
    // stack directly would leave the sidebar pointing at Status while the
    // Advanced page was showing.
    //
    // Every capture here is weak, and it has to be: the sidebar's own
    // `row-selected` handler holds a strong `status`, so a strong sidebar here
    // would close the loop and neither would ever be freed.
    status.connect_navigate({
        let sidebar = sidebar.downgrade();
        let advanced = Rc::downgrade(&advanced);
        let dns = Rc::downgrade(&dns);
        let filters = Rc::downgrade(&filters);
        move |destination: Destination| {
            let Some(sidebar) = sidebar.upgrade() else { return };
            if let Some(index) = destination.page_index() {
                sidebar.select_row(sidebar.row_at_index(index as i32).as_ref());
            }
            // Being on the page is not the same as being at the setting: on
            // Advanced the ports are four groups down, and a link that lands
            // above the fold has answered a different question than the one
            // asked. The pages that can say where to stop, do.
            match destination {
                Destination::Advanced(setting) => {
                    if let Some(advanced) = advanced.upgrade() {
                        advanced.reveal(setting);
                    }
                }
                // The one group on that page a setting key cannot address, so
                // it is asked for by name.
                Destination::Autostart => {
                    if let Some(advanced) = advanced.upgrade() {
                        advanced.reveal_autostart();
                    }
                }
                Destination::DnsProxy => {
                    if let Some(dns) = dns.upgrade() {
                        dns.reveal();
                    }
                }
                // A count of filter lists means the lists, which is the top of
                // the catalogue and not necessarily the top of the page — the
                // DNS page keeps three groups of settings above its own. Both
                // pages also hold their scroll position between visits, and a
                // link that says "show me the filters" should not arrive two
                // thirds of the way down where it was left.
                Destination::WebFilters => {
                    if let Some(filters) = filters.upgrade() {
                        filters.scroll_to_lists();
                    }
                }
                Destination::DnsFilters => {
                    if let Some(dns) = dns.upgrade() {
                        dns.scroll_to_lists();
                    }
                }
                // The whole page is the answer: the six modules are all of it.
                Destination::Protection => {}
            }
        }
    });

    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&adw::HeaderBar::new());
    sidebar_view.set_content(Some(&sidebar));
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar_view, "AdGuard UI")));

    // After the pages are built, so priming the snapshot cannot race the first
    // render, and so a repaint always has rows to patch rather than a spinner.
    let watch = watch::install(
        &status,
        &protection,
        &[advanced.clone(), stealth],
        &dns,
        &filters,
        &toasts,
    );

    MainView {
        root: split,
        status,
        protection,
        advanced,
        _watch: watch,
    }
}

/// Re-read the four checks that live outside `proxy.yaml` whenever the window
/// regains focus: AdGuard's root helper, whether its certificate is trusted,
/// whether its browser integration is installed, and whether the login entry is
/// in place.
///
/// The user's way out of the first three unmet states is a command they run in a
/// terminal, so the moment they come back to this window is exactly the moment
/// the answer has changed — and hunting for a refresh button to be told so
/// would make the instruction feel like it had not worked
/// (`architecture.md` §6). The fourth is the same shape with a different other
/// window: a startup-applications editor writes the file this application's
/// login switch reads, and the two disagreeing would be worse than either.
///
/// **The browser check has a second trigger the other two do not**, and it is
/// the reason it is here rather than read once when the page is built:
/// installing a browser invalidates it. `install-browser-integration` writes
/// only where it already sees a browser, so a browser installed after that
/// command was last run has no manifest and nothing anywhere says so
/// (`adguard_core::browser`). Coming back to this window after installing one
/// is a plausible way to find out, and the only one this application can offer.
///
/// `is-active` rather than a focus event on the widget: the check is about the
/// window as a whole, and the row the user needs to see may not be the one with
/// the keyboard focus. It notifies on *losing* focus too, which the guard below
/// makes free — the cost is one `stat`, three small reads and at most six more,
/// and it is paid only on the way back in. All three are re-read rather than
/// cached, and all three are cheap enough for the main loop for that reason.
fn connect_focus_rechecks(window: &adw::ApplicationWindow, view: &MainView) {
    let advanced = view.advanced.clone();
    let protection = view.protection.clone();
    let status = view.status.clone();
    window.connect_is_active_notify(move |window| {
        if window.is_active() {
            advanced.recheck_helper();
            // Both ends of the login entry: the switch that writes it and the
            // row on Status that reports it. Whichever page is showing, one of
            // the two is the one the user is looking at.
            advanced.recheck_autostart();
            status.recheck_autostart();
            protection.recheck_certificate();
            protection.recheck_browser_integration();
        }
    });
}

fn sidebar_row(page: &Page) -> gtk::ListBoxRow {
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    box_.append(&gtk::Image::from_icon_name(page.icon));
    box_.append(&gtk::Label::new(Some(page.title)));

    let row = gtk::ListBoxRow::builder().child(&box_).build();
    // Without this the row reaches the accessibility tree unnamed: it is a bare
    // `GtkListBoxRow` wrapping an icon and a label, and neither is promoted to
    // be the row's name. A screen reader then announces five anonymous list
    // items where the whole navigation is — and, less importantly but usefully,
    // nothing outside the process can tell the pages apart either, which is
    // what stops a headless test from opening any page but Status.
    row.update_property(&[gtk::accessible::Property::Label(page.title)]);
    row
}

/// A toast carrying text we did not write.
///
/// CLI messages and filter names contain `&` ("uBlock Origin & AdGuard"), and
/// `AdwToast:use-markup` defaults to true — left on, Pango fails to parse the
/// string and the message renders mangled. `use_markup` comes first for the
/// same reason it does on the filter rows: the title is consumed as it is set.
pub fn toast(message: &str) -> adw::Toast {
    adw::Toast::builder().use_markup(false).title(message).build()
}

/// Scroll a widget to the top of whatever scrolls it, and tint it for a moment.
///
/// The second half of a link from the Status page. Switching pages is not the
/// same as arriving: on the Advanced page the ports the user clicked are four
/// groups below the fold, and a page that opens at the top has put them back
/// where they were before the link existed.
///
/// The tint is what makes the scroll readable. A page that has jumped to the
/// middle of itself looks, without it, like a page that opened somewhere
/// arbitrary — the user has to re-find the thing they just clicked in order to
/// recognise that they arrived. Use [`scroll_to`] instead when the answer is
/// everything from the widget downwards rather than the widget itself; marking
/// one group there would point at the wrong thing.
pub fn reveal(widget: &impl IsA<gtk::Widget>) {
    once_allocated(widget, |widget| {
        scroll_into_view(widget);
        mark(widget);
    });
}

/// [`reveal`] without the tint: bring a widget to the top of the view and leave
/// it looking like itself.
pub fn scroll_to(widget: &impl IsA<gtk::Widget>) {
    once_allocated(widget, scroll_into_view);
}

/// Run `then` against `widget` once it has a size, or give up.
///
/// # Why this waits for a frame
///
/// A `GtkStack` does not allocate the children that are not showing, so at the
/// instant a page is switched to, the group on it has no position on screen to
/// scroll to — and asking for one yields zero, which reads as "the top of the
/// page" and would silently do nothing at all.
///
/// So the work is deferred to an idle, which runs *after* the frame clock has
/// laid the page out: GDK draws at a higher priority than an idle callback, so
/// one pass round the main loop is enough in the ordinary case. It re-arms while
/// the width is still zero rather than assuming that, and gives up after a
/// handful of frames — losing the scroll, never the navigation, which has
/// already happened by then.
fn once_allocated(widget: &impl IsA<gtk::Widget>, then: impl FnOnce(&gtk::Widget) + 'static) {
    /// Frames to wait for an allocation before giving up.
    const FRAMES: u32 = 8;

    let widget = widget.as_ref().clone();
    let mut then = Some(then);
    let mut waited = 0;
    glib::idle_add_local(move || {
        waited += 1;
        if widget.width() == 0 {
            return if waited < FRAMES {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            };
        }
        if let Some(then) = then.take() {
            then(&widget);
        }
        glib::ControlFlow::Break
    });
}

/// Tint a widget, and take the tint off again a moment later.
fn mark(widget: &gtk::Widget) {
    /// How long the tint stays at full strength before it starts fading.
    const HOLD: std::time::Duration = std::time::Duration::from_millis(1200);

    widget.add_css_class(style::REVEAL_TARGET);
    widget.add_css_class(style::REVEALED);
    glib::timeout_add_local_once(HOLD, {
        let widget = widget.clone();
        // Only the tint comes off. `REVEAL_TARGET` carries the transition that
        // fades it, so removing that too would make it vanish instead.
        move || widget.remove_css_class(style::REVEALED)
    });
}

/// Put `widget` at the top of the nearest scrolling ancestor.
///
/// Translating into the scrolled window rather than into its child: the child is
/// a `GtkViewport` on some libadwaita versions and the page's clamp on others,
/// and the arithmetic only needs a widget whose origin is the top of the visible
/// area — which the scrolled window itself always is. The offset that comes back
/// is therefore the widget's distance from the top of the *view*, and the scroll
/// position already in effect is what turns it back into a distance from the top
/// of the content.
fn scroll_into_view(widget: &gtk::Widget) {
    let Some(scroller) = widget
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
    else {
        return;
    };
    let Some((_, offset)) = widget.translate_coordinates(&scroller, 0.0, 0.0) else {
        return;
    };
    let adjustment = scroller.vadjustment();
    // A group near the foot of a short page cannot reach the top, and asking for
    // it would leave the adjustment holding a value it will silently clamp
    // anyway — with the group off screen if anything ever read the value back.
    let furthest = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    adjustment.set_value((adjustment.value() + offset).clamp(adjustment.lower(), furthest));
}

/// `/home/you/.local/share/...` -> `~/.local/share/...`, so an AdGuard path
/// fits in a subtitle without wrapping.
pub fn abbreviate(path: &std::path::Path) -> String {
    let display = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = std::path::Path::new(&home).display().to_string();
            display
                .strip_prefix(&home)
                .map_or(display.clone(), |rest| format!("~{rest}"))
        }
        _ => display,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every link on the Status page has to resolve to a sidebar row.
    ///
    /// The two halves are strings that meet nowhere else: `Destination::page`
    /// spells the id and `PAGES` defines it, so renaming a page id — or adding a
    /// destination for a page that does not exist — would otherwise turn a link
    /// into a click that quietly does nothing. Nothing on screen would say so.
    #[test]
    fn every_destination_names_a_page() {
        for destination in [
            Destination::Protection,
            Destination::WebFilters,
            Destination::DnsFilters,
            Destination::DnsProxy,
            Destination::Advanced(adguard_core::config::key::PROXY_MODE),
            Destination::Autostart,
        ] {
            assert!(
                destination.page_index().is_some(),
                "{} is not one of PAGES",
                destination.page()
            );
        }
    }

    /// The Status page is index 0, which [`main_view`] relies on when it
    /// recounts the figures on the way back to it.
    #[test]
    fn status_is_the_first_page() {
        assert_eq!(PAGES[0].id, "status");
    }

    /// About is last, which is where [#4] asked for it and where it belongs:
    /// every page above it answers a question about protection, and this one is
    /// about the installation. A page inserted after it would put a version
    /// number between two settings pages.
    ///
    /// [#4]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/4
    #[test]
    fn about_is_the_last_page() {
        assert_eq!(PAGES.last().expect("PAGES is never empty").id, "about");
    }

    /// Each id names exactly one page. They are stack child names, sidebar
    /// lookups and the refresh button's match arms all at once, so a duplicate
    /// would be a page that cannot be shown rather than a compile error.
    #[test]
    fn every_page_id_is_distinct() {
        let mut ids: Vec<_> = PAGES.iter().map(|page| page.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two pages share an id");
    }
}
