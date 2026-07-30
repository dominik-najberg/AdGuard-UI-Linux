//! AdGuard UI — GTK4/libadwaita front-end for `adguard-cli`.
//!
//! Reads state from AdGuard's own files (`proxy.yaml`, `agflm_*.db`) and the
//! `status` command; writes only ever go through `adguard-cli`. See
//! `docs/architecture.md` for the split and `docs/cli-contract.md` for the
//! measured CLI behaviour the wrapper encodes.

mod advanced;
mod filters;
mod protection;
mod status;
mod watch;
mod worker;

use std::cell::RefCell;
use std::rc::Rc;

use adguard_core::{Cli, FilterSet, Toggle};
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
    /// are no pages behind it.
    view: Option<MainView>,
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

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("AdGuard UI")
        .default_width(820)
        .default_height(680)
        .build();

    let view = match Cli::discover() {
        Ok(cli) => {
            let view = main_view(&cli);
            window.set_content(Some(&view.root));
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
    }));
    Ok(())
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
const PAGES: [Page; 4] = [
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
        id: "advanced",
        title: "Advanced",
        icon: "emblem-system-symbolic",
    },
];

struct Page {
    id: &'static str,
    title: &'static str,
    icon: &'static str,
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
    /// The `proxy.yaml` subscription. Dropping it ends the subscription, so it
    /// lives exactly as long as the pages it reconciles — and it is what keeps
    /// the Advanced page reachable, since nothing else here holds one.
    _watch: Option<watch::ConfigWatch>,
}

fn main_view(cli: &Cli) -> MainView {
    let toasts = adw::ToastOverlay::new();

    let status = status::StatusPage::new(cli.clone(), toasts.clone());
    let protection = protection::ProtectionPage::new(cli.clone(), toasts.clone());
    // The DNS catalogue gets its own page later: its user-rules row cannot be
    // enabled through `dns filters enable` (see docs/cli-contract.md §6).
    let filters = filters::FiltersPage::new(cli.clone(), toasts.clone(), FilterSet::Http);
    let advanced = advanced::AdvancedPage::new(cli.clone(), toasts.clone());

    let stack = gtk::Stack::new();
    stack.add_named(status.widget(), Some(PAGES[0].id));
    stack.add_named(protection.widget(), Some(PAGES[1].id));
    stack.add_named(filters.widget(), Some(PAGES[2].id));
    stack.add_named(advanced.widget(), Some(PAGES[3].id));
    toasts.set_child(Some(&stack));

    let content_header = adw::HeaderBar::new();
    let content_title = adw::WindowTitle::new(PAGES[0].title, "");
    content_header.set_title_widget(Some(&content_title));

    // Refreshes whichever page is showing: status re-runs `status`, filters
    // re-reads the catalogue. Both also refresh themselves after any change
    // they make; this is for changes made behind our back, from a terminal.
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh"));
    refresh.connect_clicked({
        let stack = stack.clone();
        let status = status.clone();
        let protection = protection.clone();
        let filters = filters.clone();
        let advanced = advanced.clone();
        move |_| match stack.visible_child_name().as_deref() {
            Some("protection") => protection.reload(),
            Some("filters") => filters.reload(),
            Some("advanced") => advanced.reload(),
            _ => status.refresh(),
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
        move |_, row| {
            let Some(row) = row else { return };
            let page = &PAGES[row.index().max(0) as usize];
            stack.set_visible_child_name(page.id);
            content_title.set_title(page.title);
            // On a narrow window the sidebar and content are separate views;
            // choosing a page should move to it.
            split.set_show_content(true);
        }
    });
    sidebar.select_row(sidebar.row_at_index(0).as_ref());

    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&adw::HeaderBar::new());
    sidebar_view.set_content(Some(&sidebar));
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar_view, "AdGuard UI")));

    // After the pages are built, so priming the snapshot cannot race the first
    // render, and so a repaint always has rows to patch rather than a spinner.
    let watch = watch::install(&protection, &advanced);

    MainView {
        root: split,
        status,
        protection,
        _watch: watch,
    }
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

    gtk::ListBoxRow::builder().child(&box_).build()
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
