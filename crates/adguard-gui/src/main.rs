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
mod worker;

use adguard_core::{Cli, FilterSet};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

/// Must match the `.desktop` filename and its `StartupWMClass`, or GNOME
/// shows a second unbranded icon instead of grouping with the launcher.
const APP_ID: &str = "io.github.dominik-najberg.AdGuardUI";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("AdGuard UI")
        .default_width(820)
        .default_height(680)
        .build();

    match Cli::discover() {
        Ok(cli) => window.set_content(Some(&main_view(&cli))),
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

fn main_view(cli: &Cli) -> adw::NavigationSplitView {
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

    split
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
