//! The window shown when `adguard-cli` is not installed, and the way out of it
//! (#51).
//!
//! It used to be a dead end: a status page saying the CLI was not found, and
//! nothing to do about it here. Now it offers AdGuard's own installer —
//! [`adguard_core::install::command`], the line AdGuard's knowledge base gives,
//! unchanged — in a terminal, which is the same arrangement *Fix certificate*
//! uses for `cert`: AdGuard's command, a terminal the user drives, and a
//! password typed into `sudo` there and never seen by this application
//! (`architecture.md` §6). With no terminal to drive, the command is copied.
//!
//! **Nothing waits on the terminal.** The page looks for the binary every
//! [`POLL`] for as long as it is on screen, whoever put it there — the button,
//! the same line typed into a terminal of the user's own, a package — and hands
//! over as soon as one turns up. A file monitor would need one per directory
//! the binary could land in: every `$PATH` entry, `/opt`, `~/.local`, several
//! of which do not exist until the installer creates them. A handful of `stat`s
//! every two seconds, on a page whose only job is to wait, costs less than
//! that bookkeeping and cannot miss a directory.
//!
//! **A binary that has just appeared may not be finished.** `install.sh`
//! unpacks a tarball, and `is_file` is true from the first byte of the
//! extraction, well before the root helper and the certificate installer beside
//! it exist. So the binary has to be executable and read the same — path, size
//! and modification time — on two polls in a row before it is handed over.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use adguard_core::install::{self, Channel};
use adguard_core::Cli;
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::toast;

/// How often the page looks for the binary while it is on screen.
const POLL: Duration = Duration::from_secs(2);

const LEAD: &str = "AdGuard UI is a window onto AdGuard CLI, and does nothing without it.";

const WHAT_HAPPENS: &str = "The button opens a terminal running AdGuard's own installer — the \
    command below, as AdGuard's knowledge base gives it. It downloads AdGuard CLI from GitHub, \
    asks for your password to create /opt/adguard-cli, and asks whether to link it into \
    /usr/local/bin; either answer works. The password goes to the terminal, never to AdGuard UI.";

const ELSEWHERE: &str = "Installed somewhere this application does not look? Set $ADGUARD_CLI \
    to its path and start AdGuard UI again.";

const WAITING: &str = "This window carries on by itself once AdGuard CLI is installed, \
    starting with activation and setup.";

/// Build the page. `found` runs once, on the main loop, when a finished
/// `adguard-cli` turns up; the caller replaces this page from there.
pub fn view(found: impl Fn(Cli) + 'static) -> adw::ToolbarView {
    let toasts = adw::ToastOverlay::new();
    let status = adw::StatusPage::builder()
        .icon_name("system-software-install-symbolic")
        .title("AdGuard CLI is not installed")
        .build();

    if install::supported_arch() {
        status.set_description(Some(&format!("{LEAD}\n\n{WHAT_HAPPENS}")));
        status.set_child(Some(&installer(&toasts)));
    } else {
        // `install.sh` stops with "Unsupported CPU type" here, so a button
        // would only open a terminal to say so.
        status.set_icon_name(Some("dialog-error-symbolic"));
        status.set_description(Some(&format!(
            "{LEAD}\n\nAdGuard publishes AdGuard CLI for x86_64 and aarch64 computers only, \
             and this one is {}, so there is nothing to install it from.\n\n{ELSEWHERE}",
            install::arch()
        )));
    }
    toasts.set_child(Some(&status));

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));

    watch(&view, found);
    view
}

/// The channel picker, the command it selects, and the button that runs it.
fn installer(toasts: &adw::ToastOverlay) -> gtk::Widget {
    let channel = adw::ComboRow::builder()
        .title("Version")
        .subtitle("Beta and nightly are AdGuard's previews of what comes next")
        .model(&gtk::StringList::new(&Channel::ALL.map(Channel::label)))
        .build();

    let command = adw::ActionRow::builder()
        .title("Command")
        .subtitle(install::command(Channel::Release))
        .subtitle_lines(3)
        .subtitle_selectable(true)
        .use_markup(false)
        .build();
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy the command"));
    copy.set_valign(gtk::Align::Center);
    copy.add_css_class("flat");
    copy.connect_clicked({
        let toasts = toasts.clone();
        let command = command.clone();
        move |_| {
            let text = command.subtitle().unwrap_or_default();
            command.clipboard().set_text(&text);
            toasts.add_toast(toast("Command copied"));
        }
    });
    command.add_suffix(&copy);

    channel.connect_selected_notify({
        let command = command.clone();
        move |row| command.set_subtitle(&install::command(selected(row)))
    });

    let group = adw::PreferencesGroup::new();
    group.add(&channel);
    group.add(&command);

    let button = gtk::Button::with_label("Install AdGuard CLI…");
    button.add_css_class("pill");
    button.add_css_class("suggested-action");
    button.set_halign(gtk::Align::Center);
    button.connect_clicked({
        let toasts = toasts.clone();
        let channel = channel.clone();
        move |button| {
            let command = install::command(selected(&channel));
            if crate::terminal::open(&command) {
                toasts.add_toast(toast("Follow AdGuard's installer in the terminal"));
            } else {
                // No terminal we know how to drive: the command is still the
                // way in, so hand it over to be pasted into one.
                button.clipboard().set_text(&command);
                toasts.add_toast(toast("No terminal found — command copied, run it yourself"));
            }
        }
    });

    let note = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build()
    };

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    column.append(&group);
    column.append(&button);
    column.append(&note(WAITING));
    column.append(&note(ELSEWHERE));

    adw::Clamp::builder()
        .maximum_size(560)
        .child(&column)
        .build()
        .upcast()
}

fn selected(row: &adw::ComboRow) -> Channel {
    Channel::ALL
        .get(row.selected() as usize)
        .copied()
        .unwrap_or(Channel::Release)
}

/// What one look at the binary saw: enough to tell a file still being written
/// from one that has settled.
type Sighting = (PathBuf, u64, SystemTime);

fn sight() -> Option<Sighting> {
    let path = adguard_core::paths::cli_binary()?;
    let meta = std::fs::metadata(&path).ok()?;
    if meta.permissions().mode() & 0o111 == 0 {
        return None;
    }
    Some((path, meta.len(), meta.modified().ok()?))
}

/// Look for the binary every [`POLL`] until it has settled, then hand over.
///
/// The timer holds the page weakly and stops once the page is gone, so a
/// window replaced some other way does not leave it polling for nothing.
fn watch(view: &adw::ToolbarView, found: impl Fn(Cli) + 'static) {
    let page = view.downgrade();
    let last: RefCell<Option<Sighting>> = RefCell::new(None);
    glib::timeout_add_local(POLL, move || {
        if page.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let now = sight();
        let settled = now.is_some() && *last.borrow() == now;
        if !settled {
            last.replace(now);
            return glib::ControlFlow::Continue;
        }
        match Cli::discover() {
            Ok(cli) => {
                found(cli);
                glib::ControlFlow::Break
            }
            Err(_) => glib::ControlFlow::Continue,
        }
    });
}
