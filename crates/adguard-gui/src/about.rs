//! The About page: two version numbers, the one control that reaches
//! AdGuard's servers on purpose, and the one that updates this application.
//!
//! Built for [issue #4](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/4),
//! which asked for a way to check for updates from the main window. An About
//! page is where it belongs rather than a header-bar button, because the action
//! is *about this installation* — and because the app has never had anywhere to
//! show its own version or the CLI's. `Cli::version` has existed since v1.0 and
//! nothing had ever called it.
//!
//! # Everything on this page is shaped by contract §14
//!
//! **The command is misnamed and the page may not repeat the mistake.**
//! `check-update` does not check: it *downloads* filters, DNS filters,
//! userscripts, Safe Browsing and CRLite, and only the application is checked.
//! So the button says *Update now* and the group says what that means. A control
//! labelled "Check for updates" would misdescribe five sixths of what it does.
//!
//! **Nothing is counted.** Safe Browsing and CRLite answer `Updated` on every
//! run of a working install — seven times out of seven, measured — so a summary
//! like "2 of 6 updated" would render identically forever while appearing to
//! report on the user's machine. The six verdicts are shown and never added up.
//!
//! **A component failure is an ordinary outcome, not an error state.** Five of
//! fourteen measured runs carried one and every one cleared on the next run. So
//! a failure is a row with a retry beside it, and the page does not otherwise
//! change character.
//!
//! **The application half is never applied by this application.** `adguard-cli
//! update` re-runs an installer over a suid root helper, and this application
//! performs no privileged operation on AdGuard's behalf (`architecture.md` §1,
//! §6). The command is named, and *Run in Terminal* hands it to a terminal the
//! user drives — the arrangement *Fix certificate* uses for `cert` — so any
//! password it asks for is typed there, and nothing runs it unseen.
//!
//! **The channel is AdGuard's setting, written the ordinary way.** Which of
//! release, beta and nightly AdGuard offers itself from is `update_channel` in
//! `proxy.yaml`, and switching it is one `config set`. It downloads nothing and
//! installs nothing; what it changes is what AdGuard says is available, which
//! the page reads back straight away and shows in the notice above the command.
//!
//! **This application's own update is a different case, and is applied.** It is
//! a package this project builds and signs, installed by the distribution's
//! own package manager behind polkit's own password prompt — not an installer
//! run over a suid helper. `crate::updater` holds the flow and
//! `adguard_core::update` the rules; this page is one of the three places it
//! shows, with the tray and a notification.
//!
//! # What this page deliberately does not do
//!
//! **The Status page's poll is left running.** The activation path stands it
//! down, and this was built expecting to do the same — but measured against a
//! live daemon, eight `status` calls during an in-flight `check-update` came
//! back in 0.03 s each. There is nothing to hold. What *is* guarded is the
//! affordance: the button desensitises itself, which is what makes a duplicate
//! check impossible rather than merely discouraged.
//!
//! **Refreshing the page does not run the command.** [`Self::reload`] is what
//! the header-bar refresh button calls on whichever page is showing, and on
//! every other page that is a cheap re-read. Here the equivalent would be a
//! network operation with side effects on the user's filters, fired by a button
//! that means "show me what is true now" everywhere else in the app.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{Catalogue, Channel, Cli, Config, FilterSet, Standing, UpdateReport, Verdict};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::updater::{Phase, Updater};
use crate::{abbreviate, toast, worker};

/// Shown in a value row before its reading has arrived, as on the Status page.
const PLACEHOLDER: &str = "—";

/// The button, in its two states.
const UPDATE: &str = "Update now";
const WORKING: &str = "Updating…";

/// What the update actually does, in the one place the user meets it.
///
/// Says "downloads" first and "checks" last, in that order, because that is the
/// proportion: five components are fetched and one is asked about.
const WHAT_IT_DOES: &str =
    "Downloads the current filter lists, DNS filter lists, userscripts, Safe Browsing data \
     and certificate revocation data, then asks whether a newer AdGuard CLI has been \
     released. AdGuard does this on its own as well — this is for when you would rather \
     not wait.";

/// Why there is no button to install an AdGuard CLI update, when there is one
/// for AdGuard UI's own.
///
/// `adguard-cli update` re-runs AdGuard's installer over its suid root helper,
/// a file in the user's home directory that AdGuard chose to make root. That is
/// AdGuard's arrangement and the user's to renew, not this application's to
/// renew on their behalf (`architecture.md` §6).
const WHY_NO_BUTTON: &str =
    "Updating AdGuard CLI re-runs its installer over AdGuard's privileged helper, which \
     is AdGuard's to change rather than this application's — so this one is yours to run. \
     Run in Terminal opens it in a terminal, where it asks for anything it needs. AdGuard \
     will not update while protection is on, so the terminal pauses it first and turns it \
     back on afterwards.";

/// Beside the "last changed" figure, and load-bearing rather than decorative:
/// the column behind that figure moves only when data arrives, so a long gap is
/// the *absence of upstream revisions* and not the absence of checking.
const WHAT_A_GAP_MEANS: &str =
    "AdGuard refreshes on its own every few hours. This is when something last \
     changed, so a long gap means the lists have not been revised — not that \
     nothing has looked.";

/// The one disclosure this page owes, next to the switch it is about.
///
/// This application never spoke to anything but `adguard-cli` until the release
/// check, and the check now runs by itself once a day. A user running an ad
/// blocker should meet that fact where it can be switched off rather than in a
/// changelog.
const WHAT_THE_CHECK_SENDS: &str =
    "AdGuard UI asks github.com once a day whether a newer version has been released, \
     and whenever you press Check. It is the only thing this application sends anywhere \
     by itself. The request names the application and its version, and nothing about \
     you or this machine.";

/// The button that asks, in its two states.
const CHECK_RELEASES: &str = "Check";
const CHECKING: &str = "Checking…";

/// The button that installs, in its three states.
const INSTALL: &str = "Update and restart";
const DOWNLOADING: &str = "Downloading…";
const INSTALLING: &str = "Installing…";

/// What *Update and restart* will do, said before it is pressed — the password
/// prompt above all, which would otherwise look like something else asking.
const HOW_A_PACKAGE_UPDATES: &str =
    "Update and restart downloads the release from GitHub, checks that it was signed \
     by this project, and installs it with your system's package manager, which asks \
     for your password. AdGuard UI then restarts.";
const HOW_A_TARBALL_UPDATES: &str =
    "Update and restart downloads the release from GitHub, checks that it was signed \
     by this project, and installs it where this copy is. AdGuard UI then restarts.";

/// Why this copy cannot install the release itself.
const WHY_NOT_THIS_COPY: &str =
    "This copy of AdGuard UI was not installed from a release package — it is a build, \
     or a file copied by hand — so it cannot replace itself. Install the release from \
     its page.";

/// The command that does it. Named, and only ever run in a terminal the user
/// drives (contract §14).
const UPDATE_COMMAND: &str = "adguard-cli update";

/// The button beside it, and what it becomes when the channel cannot deliver
/// what AdGuard offers (see [`short_of`]).
const RUN_UPDATE: &str = "Run in Terminal";
const RUN_UPDATE_ON_RELEASE: &str = "Switch to Release and Update";

/// What *Run in Terminal* runs, given the resolved binary already quoted.
///
/// AdGuard refuses to update under a running proxy — *"You can't update while
/// the proxy server is running, please stop the proxy server first"*, exit 0 —
/// so handing it `update` alone, with protection on, only ever showed that.
/// With the proxy running the script stops it, updates, and starts it again
/// whether or not the update went through: protection was on when the button
/// was pressed and is left that way. With it stopped, nothing is started that
/// the user had not.
///
/// `;` rather than `&&` throughout: `stop` and `update` both exit 0 whatever
/// happened (contract rule 3), so `&&` would decide nothing, and the start must
/// follow a failed update as much as a good one.
///
/// # The start is taken out of the terminal's session
///
/// A plain `start` here left protection **off**, every time. The daemon it
/// forks stays in the terminal's session, and closing the terminal — *Press
/// Enter to close* — hangs it up: three updates on 22 October 2026 each
/// started a proxy that stopped two seconds later, at the Enter, and the same
/// happened to a `start` in a `script` pty the moment it closed. `setsid -w`
/// gives it a session of its own, the way the application's own start has
/// one, and survived the same test. Its output goes nowhere, since the daemon
/// would otherwise hold a terminal that is about to disappear; `status`
/// afterwards says in the terminal whether it took.
fn update_script(binary: &str, running: bool) -> String {
    if running {
        format!(
            "{binary} stop; {binary} update; \
             setsid -w {binary} start </dev/null >/dev/null 2>&1; {binary} status"
        )
    } else {
        format!("{binary} update")
    }
}

/// Why updating may not bring the version AdGuard offers, when the reason is
/// known.
///
/// `update` runs the `install.sh` on the channel's own branch of AdGuard's
/// repository, and that script pins one version. AdGuard's notice, though,
/// offers anything newer, a stable release included. Measured on 22 October
/// 2026: on Beta, with v1.5.0 out the evening before, the notice said
/// *"AdGuard CLI v1.5.0 is now available"*, `update` went from 1.5.0-rc.2 to
/// 1.5.0-rc.3 — the version the `beta` branch's script names — and the notice
/// said the same thing again. Pressing the button again installs rc.3 again,
/// and nothing on the page changes — so the notice says where the stable build
/// is, before the button is pressed rather than after it has failed.
///
/// "May be older", not "is": the branch's script is bumped by AdGuard, and
/// once it names the stable build the advice is merely unnecessary. Only for a
/// stable version offered to a pre-release on another channel, the one case
/// measured; anything else is left as AdGuard's sentence alone.
fn short_of(installed: &str, newer: &str, channel: Channel) -> Option<String> {
    if channel == Channel::Release || newer.contains('-') || !installed.contains('-') {
        return None;
    }
    Some(format!(
        "v{newer} is a stable release. Updating on the {} channel installs that channel's \
         newest build, which may be older, so the button switches the update channel to \
         Release first.",
        channel.label()
    ))
}

/// What each update channel is, as the row under the CLI's version says it.
fn channel_meaning(channel: Channel) -> &'static str {
    match channel {
        Channel::Release => "Stable builds — what AdGuard recommends",
        Channel::Beta => "Release candidates, ahead of the next stable build",
        Channel::Nightly => "Built daily from current development — newest, least tested",
    }
}

/// Said in the notice, in AdGuard's own words less its `>`.
fn available(newer: &str) -> String {
    format!("AdGuard CLI v{newer} is now available")
}

const HOMEPAGE: &str = "https://github.com/dominik-najberg/AdGuard-UI-Linux";
const ISSUES: &str = "https://github.com/dominik-najberg/AdGuard-UI-Linux/issues";

pub struct AboutPage {
    page: adw::PreferencesPage,
    cli: Cli,
    toasts: adw::ToastOverlay,

    /// The CLI's own version banner, read off the main thread like everything
    /// else it says.
    cli_version: adw::ActionRow,
    /// `1.5.0-rc.2 available` beside it, when AdGuard says so.
    cli_standing: gtk::Label,

    /// Release, beta or nightly — the `update_channel` AdGuard offers itself
    /// from. Insensitive until the file has been read, and while a switch is
    /// being written.
    channel: adw::ComboRow,
    /// What the file says now, so a refused switch can be put back.
    channel_now: Cell<Channel>,
    /// Set while the row is being moved to match the file, so that the move is
    /// not taken for the user choosing a channel.
    painting: Cell<bool>,
    /// *Run in Terminal* was pressed and AdGuard still said something newer
    /// was available. The version is read again each time the window comes
    /// back into focus until it says nothing is — see [`Self::recheck_update`].
    updating: Cell<bool>,
    /// The version the last read found, and the one it was when *Run in
    /// Terminal* was pressed. A read that finds them different is the update
    /// landing, which is said in a toast: the version row changing by itself
    /// is easy to miss, and the notice may well stay up (see [`short_of`]).
    version_now: RefCell<Option<String>>,
    version_before: RefCell<Option<String>>,
    /// What AdGuard last said was newer, kept so the notice can be redrawn
    /// when the channel arrives: [`short_of`] depends on both, and the two are
    /// read in parallel.
    newer_now: RefCell<Option<String>>,

    /// When new filter data last arrived, from the catalogues themselves.
    ///
    /// The answer to the question a check-on-launch would have been built to
    /// answer, at the cost of two SQLite reads instead of a network fetch — and
    /// a better answer, because a silent launch check tells the user nothing by
    /// construction. See [`freshness`] for why it says *changed*.
    ///
    /// [`freshness`]: Self::read_freshness
    freshness: gtk::Label,

    /// The verdict beside this application's own version — `Latest`, or the tag
    /// of a release that is newer. Blank until asked, because nothing has been.
    ui_standing: gtk::Label,
    ui_check: gtk::Button,
    /// Shown only when a check found something newer.
    ui_notice: adw::PreferencesGroup,
    ui_release: adw::ActionRow,
    /// *Update and restart*. Absent from a copy that cannot replace itself.
    ui_install: gtk::Button,
    /// The check, its timer and the install. Shared with the tray.
    updater: Rc<Updater>,
    /// Where *Release notes* goes, set by the check that found it.
    ///
    /// Held rather than baked into the handler because the release is not known
    /// until someone asks — and it is built from a constant and a tag rather
    /// than taken from GitHub's answer, so a compromised endpoint cannot choose
    /// what this application opens in a browser.
    ui_release_url: RefCell<Option<String>>,

    check: gtk::Button,
    /// A check is in flight. The button is insensitive for the same span, and
    /// this is the half that cannot be got around by a second code path.
    busy: Cell<bool>,

    /// One row per component, rebuilt from each report. Empty and hidden until
    /// the first run — there is nothing truthful to show before then.
    results: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,

    /// AdGuard's word about the application, shown only when it has one.
    notice: adw::PreferencesGroup,
    notice_row: adw::ActionRow,
    run_update: gtk::Button,
    /// The notice is showing [`short_of`]'s explanation, so the button moves
    /// the channel to Release before it runs the update.
    to_release: Cell<bool>,

    /// Notified after every successful run, so the window can re-read the
    /// catalogues that moved.
    ///
    /// The shape `StatusPage::connect_navigate` uses, and for the same reason:
    /// this page knows what changed and nothing else, while which page renders a
    /// catalogue is `main_view`'s business. It cannot hold those pages itself —
    /// it is built alongside them and the sidebar already holds it.
    checked: RefCell<Option<Box<dyn Fn(&UpdateReport)>>>,
}

impl AboutPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay, updater: Rc<Updater>) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        // ---- versions ----

        // The description is a disclosure, not a caption. Everything else this
        // application causes goes through `adguard-cli`; the button in this
        // group is the one place it speaks to a third party itself, and a user
        // running an ad blocker is entitled to be told that before they press
        // it rather than after.
        let versions = adw::PreferencesGroup::builder()
            .title("Versions")
            .description(WHAT_THE_CHECK_SENDS)
            .build();
        let app_version = row("AdGuard UI", env!("CARGO_PKG_VERSION"));
        let ui_standing = gtk::Label::builder().valign(gtk::Align::Center).build();
        ui_standing.add_css_class("dim-label");
        let ui_check = gtk::Button::builder()
            .label(CHECK_RELEASES)
            .valign(gtk::Align::Center)
            .build();
        app_version.add_suffix(&ui_standing);
        app_version.add_suffix(&ui_check);

        let cli_version = row("AdGuard CLI", PLACEHOLDER);
        // A newer build AdGuard reports, beside the version it would replace,
        // as AdGuard UI's own row shows its newer release. Blank otherwise.
        let cli_standing = gtk::Label::builder().valign(gtk::Align::Center).build();
        cli_standing.add_css_class("accent");
        cli_version.add_suffix(&cli_standing);
        // Under the version, because it decides what the next one will be.
        let channel = adw::ComboRow::builder()
            .title("AdGuard CLI channel")
            .subtitle(channel_meaning(Channel::Release))
            .model(&gtk::StringList::new(&Channel::ALL.map(Channel::label)))
            .sensitive(false)
            .build();
        let binary = row("AdGuard CLI binary", &abbreviate(cli.binary()));
        binary.set_subtitle_lines(2);
        // Under the version it checks, so the switch and the disclosure above
        // it are read together.
        let automatic = adw::SwitchRow::builder()
            .title("Check for updates automatically")
            .subtitle("Once a day")
            .active(updater.automatic())
            .build();
        versions.add(&app_version);
        versions.add(&automatic);
        versions.add(&cli_version);
        versions.add(&channel);
        versions.add(&binary);
        page.add(&versions);

        // Revealed only when a check found something newer. Unlike the AdGuard
        // CLI notice below it carries a button that installs: this is a package
        // this project signed, installed by the system's own package manager.
        let install_kind = updater.install_kind();
        let ui_notice = adw::PreferencesGroup::builder()
            .title("A newer AdGuard UI is available")
            .description(if !install_kind.can_update() {
                WHY_NOT_THIS_COPY
            } else if install_kind.needs_password() {
                HOW_A_PACKAGE_UPDATES
            } else {
                HOW_A_TARBALL_UPDATES
            })
            .build();
        ui_notice.set_visible(false);
        let ui_release = row("Latest release", PLACEHOLDER);
        let open = gtk::Button::builder()
            .label("Release notes")
            .valign(gtk::Align::Center)
            .build();
        let ui_install = gtk::Button::builder()
            .label(INSTALL)
            .valign(gtk::Align::Center)
            .build();
        ui_install.add_css_class("suggested-action");
        ui_install.set_visible(install_kind.can_update());
        // The notes are the main action only where there is nothing to install.
        if !install_kind.can_update() {
            open.add_css_class("suggested-action");
        }
        ui_release.add_suffix(&open);
        ui_release.add_suffix(&ui_install);
        ui_notice.add(&ui_release);
        page.add(&ui_notice);

        // ---- AdGuard CLI's own update, if AdGuard mentions one ----

        // Directly under the versions, beside AdGuard UI's own notice, so the
        // answer to a channel switch lands where the switch is. It used to sit
        // below the update results, out of sight of the row that asks.

        let notice = adw::PreferencesGroup::builder()
            .title("AdGuard CLI update")
            .description(WHY_NO_BUTTON)
            .build();
        notice.set_visible(false);
        let notice_row = row("AdGuard reports", PLACEHOLDER);
        notice_row.set_subtitle_lines(6);
        notice_row.set_subtitle_selectable(true);
        let command = row("To update, run", UPDATE_COMMAND);
        command.set_subtitle_selectable(true);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy the command"));
        copy.set_valign(gtk::Align::Center);
        copy.add_css_class("flat");
        copy.connect_clicked({
            let toasts = toasts.clone();
            move |button| {
                button.clipboard().set_text(UPDATE_COMMAND);
                toasts.add_toast(toast("Command copied"));
            }
        });
        command.add_suffix(&copy);
        let run_update = gtk::Button::builder()
            .label(RUN_UPDATE)
            .valign(gtk::Align::Center)
            .build();
        command.add_suffix(&run_update);
        notice.add(&notice_row);
        notice.add(&command);
        page.add(&notice);

        // ---- the update control ----

        let updates = adw::PreferencesGroup::builder()
            .title("Updates")
            .description(WHAT_IT_DOES)
            .build();

        // Above the button, because it is the fact you read in order to decide
        // whether to press it.
        //
        // The subtitle is not filler. This column moves only when data actually
        // arrives (contract §14), so a gap of days means *nothing has changed
        // upstream* — and without the sentence saying so, "8 days ago" reads as
        // an install that has stopped working and sends the user to press a
        // button that will report `Up to date`.
        let freshness_row = row("Filter data last changed", WHAT_A_GAP_MEANS);
        freshness_row.set_subtitle_lines(3);
        let freshness = gtk::Label::builder()
            .label(PLACEHOLDER)
            .valign(gtk::Align::Center)
            .build();
        freshness.add_css_class("dim-label");
        freshness_row.add_suffix(&freshness);
        updates.add(&freshness_row);

        let action = adw::ActionRow::builder()
            .title("Filters and protection data")
            .subtitle("Filters, DNS filters, userscripts, Safe Browsing, certificate revocation")
            .build();
        action.set_use_markup(false);
        action.set_subtitle_lines(2);
        let check = gtk::Button::builder()
            .label(UPDATE)
            .valign(gtk::Align::Center)
            .build();
        check.add_css_class("suggested-action");
        action.add_suffix(&check);
        action.set_activatable_widget(Some(&check));
        updates.add(&action);
        page.add(&updates);

        // Built empty and hidden: before a run there is no honest thing for it
        // to say, and a group of six "unknown" rows would be six claims.
        let results = adw::PreferencesGroup::new();
        results.set_visible(false);
        page.add(&results);

        // ---- the project ----

        // "Project", not "AdGuard UI": the Versions group above already has a
        // row by that name, and two headings apart they would read as the same
        // thing said twice.
        let project = adw::PreferencesGroup::builder().title("Project").build();
        project.add(&link_row("Project page", HOMEPAGE));
        project.add(&link_row("Report an issue", ISSUES));
        project.add(&row("Licence", "GPL-3.0-or-later"));
        page.add(&project);

        let this = Rc::new(Self {
            page,
            cli,
            toasts,
            cli_version,
            cli_standing,
            channel: channel.clone(),
            channel_now: Cell::new(Channel::Release),
            painting: Cell::new(false),
            updating: Cell::new(false),
            version_now: RefCell::new(None),
            version_before: RefCell::new(None),
            newer_now: RefCell::new(None),
            ui_standing: ui_standing.clone(),
            ui_check: ui_check.clone(),
            ui_notice,
            ui_release,
            ui_install: ui_install.clone(),
            updater: updater.clone(),
            ui_release_url: RefCell::new(None),
            freshness,
            check: check.clone(),
            busy: Cell::new(false),
            results,
            rows: RefCell::new(Vec::new()),
            notice,
            notice_row,
            run_update: run_update.clone(),
            to_release: Cell::new(false),
            checked: RefCell::new(None),
        });

        check.connect_clicked({
            let this = this.clone();
            move |_| this.check_now()
        });

        ui_check.connect_clicked({
            let updater = updater.clone();
            move |_| updater.check(true)
        });

        ui_install.connect_clicked({
            let updater = updater.clone();
            move |_| updater.install()
        });

        channel.connect_selected_notify({
            let this = Rc::downgrade(&this);
            move |row| {
                let Some(this) = this.upgrade() else { return };
                if this.painting.get() {
                    return;
                }
                if let Some(&channel) = Channel::ALL.get(row.selected() as usize) {
                    this.switch_channel(channel);
                }
            }
        });

        run_update.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.run_update();
                }
            }
        });

        automatic.connect_active_notify({
            let updater = updater.clone();
            move |row| updater.set_automatic(row.is_active())
        });

        // Weak: the updater outlives every page, and a strong capture would
        // keep this one alive through it.
        updater.connect({
            let this = Rc::downgrade(&this);
            move |phase| {
                if let Some(this) = this.upgrade() {
                    this.render_update(phase);
                }
            }
        });

        open.connect_clicked({
            let this = this.clone();
            move |button| {
                let Some(url) = this.ui_release_url.borrow().clone() else {
                    return;
                };
                let launcher = gtk::UriLauncher::new(&url);
                let window = button.root().and_downcast::<gtk::Window>();
                glib::spawn_future_local(async move {
                    // The address is also on the row, selectable, so a browser
                    // that will not open is not a dead end.
                    let _ = launcher.launch_future(window.as_ref()).await;
                });
            }
        });

        this.read_version();
        this.read_channel();
        this.read_freshness();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    /// Re-read what this page can read cheaply — which is the version, and not
    /// the update state.
    ///
    /// The header-bar refresh button lands here. It means "show me what is true
    /// now" on every page, and on this one the update rows are a record of a
    /// command that ran at a particular moment rather than a reading of anything
    /// — so refreshing leaves them alone. Re-running the command would be a
    /// network fetch with side effects on the user's filters, fired by a button
    /// that is a no-op everywhere else.
    pub fn reload(self: &Rc<Self>) {
        self.read_version();
        self.read_channel();
        self.read_freshness();
    }

    /// The window came back into focus: if an AdGuard CLI update was handed to
    /// a terminal and has not visibly landed yet, read the version again.
    ///
    /// Two quick CLI calls, and only while one is outstanding — the focus
    /// re-check is otherwise a handful of `stat`s, and stays that way.
    pub fn recheck_update(self: &Rc<Self>) {
        if self.updating.get() {
            self.read_version();
        }
    }

    /// Called after a run, with what AdGuard said.
    pub fn connect_checked(&self, checked: impl Fn(&UpdateReport) + 'static) {
        self.checked.replace(Some(Box::new(checked)));
    }

    fn read_version(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.installed(),
            move |installed: Result<adguard_core::Installed, adguard_core::Error>| match installed {
                Ok(installed) => {
                    let version = short_version(&installed.banner).to_owned();
                    this.cli_version.set_subtitle(&version);
                    let before = this.version_before.borrow().clone();
                    if this.updating.get() && before.is_some_and(|before| before != version) {
                        this.updating.set(false);
                        this.toasts.add_toast(toast(&format!("AdGuard CLI updated to v{version}")));
                    }
                    this.version_now.replace(Some(version));
                    // `--version` carries AdGuard's word on a newer build for
                    // free, so the notice is current without `check-update`.
                    this.render_newer(installed.newer.as_deref());
                }
                // The row says what it could not do rather than sitting on a
                // placeholder that reads as "not installed".
                Err(err) => this.cli_version.set_subtitle(&err.to_string()),
            },
        );
    }

    /// Show AdGuard's word on a newer build of itself, or take it away.
    fn render_newer(&self, newer: Option<&str>) {
        self.newer_now.replace(newer.map(str::to_owned));
        match newer {
            Some(newer) => {
                self.cli_standing.set_label(&format!("{newer} available"));
                let installed = self.version_now.borrow();
                let why = installed
                    .as_deref()
                    .and_then(|installed| short_of(installed, newer, self.channel_now.get()));
                self.to_release.set(why.is_some());
                let label = if why.is_some() { RUN_UPDATE_ON_RELEASE } else { RUN_UPDATE };
                self.run_update.set_label(label);
                self.notice_row.set_subtitle(&match why {
                    Some(why) => format!("{}\n{why}", available(newer)),
                    None => available(newer),
                });
                self.notice.set_visible(true);
            }
            None => {
                self.cli_standing.set_label("");
                self.to_release.set(false);
                self.run_update.set_label(RUN_UPDATE);
                self.notice.set_visible(false);
                self.updating.set(false);
            }
        }
    }

    /// Read the channel from `proxy.yaml`, as every other setting is read —
    /// and from AdGuard when the file holds something it will not keep, since
    /// what it becomes depends on the build (contract §14).
    fn read_channel(self: &Rc<Self>) {
        let this = self.clone();
        let cli = self.cli.clone();
        worker::run(
            move || {
                let from_file = Config::load().map_err(|err| err.to_string())?.update_channel();
                match from_file {
                    Some(channel) => Ok(channel),
                    None => cli
                        .update_channel()
                        .map_err(|err| err.to_string())?
                        .ok_or_else(|| "AdGuard named no channel".to_owned()),
                }
            },
            move |channel: Result<Channel, String>| match channel {
                Ok(channel) => {
                    this.paint_channel(channel);
                    this.channel.set_sensitive(true);
                    let newer = this.newer_now.borrow().clone();
                    if newer.is_some() {
                        this.render_newer(newer.as_deref());
                    }
                }
                Err(err) => {
                    this.channel.set_sensitive(false);
                    this.channel.set_subtitle(&format!("Unavailable — {err}"));
                }
            },
        );
    }

    /// Move the row to `channel` without that counting as a choice.
    fn paint_channel(&self, channel: Channel) {
        self.channel_now.set(channel);
        self.painting.set(true);
        let index = Channel::ALL.iter().position(|c| *c == channel).unwrap_or(0);
        self.channel.set_selected(index as u32);
        self.painting.set(false);
        self.channel.set_subtitle(channel_meaning(channel));
    }

    /// The user picked a channel: write it, then ask what is available on it.
    ///
    /// Act, re-read, reconcile, like every write here (`architecture.md` §3).
    /// The re-read is the second command rather than the `config set` itself,
    /// because that one reports what AdGuard had cached for the channel it is
    /// leaving (contract §14).
    fn switch_channel(self: &Rc<Self>, channel: Channel) {
        if channel == self.channel_now.get() {
            return;
        }
        self.channel.set_sensitive(false);
        self.channel.set_subtitle("Switching…");

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                cli.set_update_channel(channel)?;
                cli.newer_release()
            },
            move |result: Result<Option<String>, adguard_core::Error>| {
                this.channel.set_sensitive(true);
                match result {
                    Ok(newer) => {
                        this.paint_channel(channel);
                        this.render_newer(newer.as_deref());
                        this.toasts.add_toast(toast(&match newer {
                            Some(newer) => format!(
                                "Switched to {} — AdGuard CLI v{newer} is available",
                                channel.label()
                            ),
                            None => format!("Switched to {} — nothing newer there", channel.label()),
                        }));
                    }
                    Err(err) => {
                        // Put the row back where the file is, rather than
                        // leave it showing a channel AdGuard is not on.
                        this.paint_channel(this.channel_now.get());
                        this.toasts.add_toast(toast(&format!("Channel not changed: {err}")));
                    }
                }
            },
        );
    }

    /// Hand `adguard-cli update` to a terminal, as *Fix certificate* does
    /// `cert`. The binary is the resolved one, quoted, so the terminal runs the
    /// copy this page describes rather than whichever `$PATH` finds first.
    ///
    /// `status` is asked first, because what the terminal runs depends on it —
    /// see [`update_script`]. A status that cannot be read counts as stopped:
    /// starting a proxy the user may have turned off is the worse mistake, and
    /// if it was running AdGuard says so in the terminal.
    ///
    /// When the notice says the channel cannot deliver what AdGuard offers, the
    /// channel is moved to Release first, as the button then says. Otherwise
    /// the button runs an update that installs the same build again and leaves
    /// the page exactly as it was — which is what it did, twice, on 22 October
    /// 2026, with the explanation in plain view above it.
    fn run_update(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        let to_release = self.to_release.get();
        self.run_update.set_sensitive(false);
        worker::run(
            move || {
                if to_release {
                    cli.set_update_channel(Channel::Release)?;
                }
                Ok(cli.status().is_ok_and(|status| status.running))
            },
            move |running: Result<bool, adguard_core::Error>| {
                this.run_update.set_sensitive(true);
                match running {
                    Ok(running) => {
                        if to_release {
                            this.paint_channel(Channel::Release);
                        }
                        this.open_update(running);
                    }
                    Err(err) => this
                        .toasts
                        .add_toast(toast(&format!("Channel not changed, nothing run: {err}"))),
                }
            },
        );
    }

    fn open_update(&self, running: bool) {
        let binary = glib::shell_quote(self.cli.binary()).to_string_lossy().into_owned();
        let command = update_script(&binary, running);
        if crate::terminal::open(&command) {
            self.updating.set(true);
            self.version_before.replace(self.version_now.borrow().clone());
            self.toasts.add_toast(toast(if running {
                "Protection is paused while AdGuard updates in the terminal"
            } else {
                "Follow AdGuard's update in the terminal"
            }));
        } else {
            // No terminal we know how to drive: the command is still the way,
            // so hand it over to be pasted into one.
            self.notice_row.clipboard().set_text(&command);
            self.toasts.add_toast(toast("No terminal found — command copied, run it yourself"));
        }
    }

    /// Draw where this application's own update stands.
    ///
    /// The check itself is [`Updater::check`]'s, and so are the toasts that
    /// answer a press of *Check* — a check run by the daily timer has no
    /// button to answer and must not leave toasts on a page nobody opened.
    fn render_update(&self, phase: &Phase) {
        let checking = matches!(phase, Phase::Checking);
        self.ui_check.set_sensitive(!phase.busy());
        self.ui_check.set_label(if checking { CHECKING } else { CHECK_RELEASES });

        self.ui_standing.set_label(&match phase {
            Phase::Unchecked | Phase::Checking => String::new(),
            Phase::Checked(Standing::Current) => "Latest".to_owned(),
            // A local build between releases. Saying "up to date" here would
            // claim this binary matches a release it does not.
            Phase::Checked(Standing::Ahead(_)) => "Unreleased build".to_owned(),
            Phase::Checked(Standing::Behind(release) | Standing::Unknown(release))
            | Phase::Downloading(release)
            | Phase::Installing(release) => release.tag.clone(),
        });

        // A release that is newer, or one this cannot place — a pre-release,
        // most likely, which is named so the user can judge. Only the first is
        // offered for installing.
        let shown = match phase {
            Phase::Checked(Standing::Unknown(release)) => Some(release),
            _ => phase.available(),
        };
        let Some(release) = shown else {
            self.ui_notice.set_visible(false);
            return;
        };
        let url = release.url();
        self.ui_release.set_subtitle(&url);
        self.ui_release.set_subtitle_selectable(true);
        self.ui_release_url.replace(Some(url));
        self.ui_notice.set_visible(true);

        let installable = self.updater.install_kind().can_update() && phase.available().is_some();
        self.ui_install.set_visible(installable);
        self.ui_install.set_sensitive(!phase.busy());
        self.ui_install.set_label(match phase {
            Phase::Downloading(_) => DOWNLOADING,
            Phase::Installing(_) => INSTALLING,
            _ => INSTALL,
        });
    }

    /// Read when new filter data last arrived, from the two catalogues.
    ///
    /// Two SQLite reads and no network — which is the whole argument for this
    /// row existing instead of a check on launch. A launch check would spend a
    /// download to learn something AdGuard's own daemon learned within the last
    /// few hours, and, being silent, would tell the user none of it.
    ///
    /// The newer of the two sets. They are separate catalogues on separate
    /// schedules, but the question this answers — *is AdGuard still getting
    /// data?* — is about the install rather than about either database, and two
    /// rows would ask the user to do the comparison themselves.
    fn read_freshness(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(
            || {
                [FilterSet::Http, FilterSet::Dns]
                    .into_iter()
                    // A catalogue that will not open is not an error here: this
                    // row is a nicety beside a button that works without it, and
                    // the Filters page already reports an unreadable database.
                    .filter_map(|set| Catalogue::open_set(set).ok())
                    .filter_map(|catalogue| catalogue.last_downloaded().ok().flatten())
                    .max()
            },
            move |newest: Option<i64>| {
                let now = glib::DateTime::now_utc().map(|now| now.to_unix()).unwrap_or(0);
                this.freshness.set_label(&match newest {
                    Some(when) => ago(now - when),
                    // No installed list carries one. Not "never" — an empty or
                    // unreadable catalogue looks the same from here.
                    None => PLACEHOLDER.to_owned(),
                });
            },
        );
    }

    /// Run `check-update`, off the main thread.
    ///
    /// The guard is the button's own sensitivity plus [`Self::busy`]: the
    /// measured range is 1.8–7.3 s, which is comfortably long enough for a
    /// second click, and two of these in flight would put two writers into the
    /// same data directory.
    fn check_now(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.check.set_sensitive(false);
        self.check.set_label(WORKING);

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.check_update(),
            move |result: Result<UpdateReport, adguard_core::Error>| {
                this.busy.set(false);
                this.check.set_sensitive(true);
                this.check.set_label(UPDATE);
                match result {
                    Ok(report) => this.render(&report),
                    Err(err) => this.render_error(&err.to_string()),
                }
            },
        );
    }

    /// Draw what AdGuard said, one row per component, in its order.
    fn render(self: &Rc<Self>, report: &UpdateReport) {
        self.clear();

        for component in &report.components {
            let row = adw::ActionRow::builder().title(component.part.title()).build();
            row.set_use_markup(false);

            // AdGuard's own sentence is the value. An unanswered header has no
            // sentence, and saying so is better than an empty cell.
            let said = if component.said.is_empty() {
                "No answer"
            } else {
                &component.said
            };
            // Not wrapped. AdGuard's longest sentence here is *Failed to update
            // filters*, which fits on one line beside a title, and letting it
            // wrap breaks even `Up to date` across two — so the column of
            // verdicts stops being scannable in order to accommodate a case
            // that does not need it. A narrow window shortens the title, which
            // is our text, rather than AdGuard's.
            let value = gtk::Label::builder()
                .label(said)
                .valign(gtk::Align::Center)
                .xalign(1.0)
                .build();
            // Colour never carries a fact on its own here — the sentence beside
            // it already says the same thing — which is the rule the Status
            // page's state rows follow.
            value.add_css_class(match component.verdict {
                Verdict::Changed => "success",
                Verdict::Failed => "error",
                Verdict::Available => "accent",
                Verdict::Unrecognised => "warning",
                Verdict::UpToDate => "dim-label",
            });
            row.add_suffix(&value);

            if component.verdict == Verdict::Failed {
                // Measured: every failure cleared on the next run. Saying so is
                // what keeps this from reading as a broken install.
                row.set_subtitle("Temporary — try again");
                row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            }

            self.results.add(&row);
            self.rows.borrow_mut().push(row);
        }

        self.results.set_title(&format!("Last updated {}", now()));
        self.results.set_visible(true);

        // AdGuard's word about itself, when it has one. `app_notice` withholds a
        // failed *check* — that is already a row above, and repeating it here
        // would recommend a command on the strength of a check that did not
        // finish (contract §14).
        match report.app_notice() {
            Some(said) => {
                self.notice_row.set_subtitle(said);
                self.notice.set_visible(true);
            }
            None => self.render_newer(None),
        }

        let failed = report.failures().count();
        self.toasts.add_toast(toast(if failed == 0 {
            "Update finished"
        } else {
            // Deliberately not a count: what failed is on the page, named, and a
            // number here would be arithmetic about rows rather than about the
            // install.
            "Update finished — some parts did not update"
        }));

        // The run may have brought data down, so the figure above the button is
        // now stale — and it is read from the catalogues rather than from the
        // report, because `1 filter(s) updated` says something happened and the
        // database says when.
        self.read_freshness();

        if let Some(checked) = self.checked.borrow().as_ref() {
            checked(report);
        }
    }

    /// The command ran and its answer could not be read, or it never ran.
    ///
    /// Shown on the page rather than only in a toast: `Error::Unparseable`
    /// carries AdGuard's actual output, and that output is the only evidence
    /// anyone has of what the CLI now says. A toast would take it away again
    /// after a few seconds.
    fn render_error(self: &Rc<Self>, message: &str) {
        self.clear();

        let row = adw::ActionRow::builder()
            .title("AdGuard could not be asked")
            .subtitle(message)
            .build();
        row.set_use_markup(false);
        row.set_subtitle_lines(6);
        row.set_subtitle_selectable(true);
        row.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));

        self.results.add(&row);
        self.rows.borrow_mut().push(row);
        self.results.set_title(&format!("Last tried {}", now()));
        self.results.set_visible(true);
        self.notice.set_visible(false);

        self.toasts.add_toast(toast(message));
    }

    fn clear(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.results.remove(&row);
        }
    }
}

/// `AdGuard CLI v1.4.13` -> `1.4.13`, and anything else through unchanged.
///
/// The row is already titled *AdGuard CLI*, so the banner repeats it, and the
/// number beside it should read like the application's own version above it
/// rather than like a sentence. A banner in any other shape is shown whole:
/// this is a presentation trim, not a parse, and losing an unfamiliar version
/// string would be worse than showing a familiar prefix.
fn short_version(banner: &str) -> &str {
    let banner = banner.trim();
    banner
        .strip_prefix("AdGuard CLI ")
        .map_or(banner, |rest| rest.strip_prefix('v').unwrap_or(rest))
}

/// A gap in seconds, as a phrase a person would use.
///
/// Coarse on purpose. The figure answers "is AdGuard still getting data", and
/// that question is settled by *minutes / hours / days* — a precise
/// "3 hours 14 minutes" would imply the number matters, and it does not.
///
/// A negative gap is a clock that has moved backwards, or a timestamp written
/// by a machine whose clock disagrees with this one. It reads as `just now`
/// rather than as a time in the future, which is the harmless direction: the
/// alternative is a row announcing that filters will be updated tomorrow.
fn ago(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    fn plural(count: i64, unit: &str) -> String {
        if count == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{count} {unit}s ago")
        }
    }

    match seconds {
        s if s < MINUTE => "just now".to_owned(),
        s if s < HOUR => plural(s / MINUTE, "minute"),
        s if s < DAY => plural(s / HOUR, "hour"),
        s => plural(s / DAY, "day"),
    }
}

/// The local time, for "last updated". Falls back to nothing readable rather
/// than to a wrong time.
fn now() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|when| when.format("at %H:%M").ok())
        .map_or_else(|| "just now".to_owned(), |when| when.to_string())
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    // Before the strings, which are consumed as they are set: filter names and
    // CLI messages contain `&`, and markup is on by default.
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

/// A row that opens a URL, with the URL as its own subtitle so it can be read
/// and copied by someone who would rather not be sent anywhere.
fn link_row(title: &str, uri: &'static str) -> adw::ActionRow {
    let row = row(title, uri);
    row.set_subtitle_selectable(true);
    row.set_activatable(true);
    row.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    row.connect_activated(move |row| {
        let launcher = gtk::UriLauncher::new(uri);
        let window = row.root().and_downcast::<gtk::Window>();
        glib::spawn_future_local(async move {
            // Nothing to report on failure: the address is in the row, in full,
            // and selectable.
            let _ = launcher.launch_future(window.as_ref()).await;
        });
    });
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_reads_as_a_phrase() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3_599), "59 minutes ago");
        assert_eq!(ago(3_600), "1 hour ago");
        assert_eq!(ago(86_399), "23 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(8 * 86_400), "8 days ago");
    }

    /// A clock that moved backwards, or a timestamp from a machine that
    /// disagrees with this one. The row must not announce a future update.
    #[test]
    fn a_negative_gap_is_not_a_time_in_the_future() {
        assert_eq!(ago(-1), "just now");
        assert_eq!(ago(-86_400), "just now");
    }

    /// The figure says when data last *changed*, and the column behind it does
    /// not move on an up-to-date run (contract §14) — so the sentence beside it
    /// has to stop "8 days ago" from reading as an install that has stopped
    /// checking. Without this the row would send users to a button that will
    /// answer `Up to date`.
    #[test]
    fn the_freshness_row_says_a_gap_is_not_a_failure_to_check() {
        let said = WHAT_A_GAP_MEANS.to_lowercase();
        assert!(said.contains("on its own"), "it has to say something else is checking");
        assert!(said.contains("not that nothing has looked"), "and deny the wrong reading");
    }

    #[test]
    fn the_cli_banner_is_trimmed_to_a_version() {
        assert_eq!(short_version("AdGuard CLI v1.4.13"), "1.4.13");
        assert_eq!(short_version("AdGuard CLI v1.4.13\n"), "1.4.13");
    }

    /// A reworded banner is shown whole rather than mangled or dropped. The trim
    /// is cosmetic, and a cosmetic rule must not be able to lose information.
    #[test]
    fn an_unfamiliar_banner_survives_whole() {
        assert_eq!(short_version("adguard-cli 2.0"), "adguard-cli 2.0");
        assert_eq!(short_version("AdGuard CLI 1.4.13"), "1.4.13");
        assert_eq!(short_version(""), "");
    }

    /// The two links here and the ones in the AppStream metadata are the same
    /// project, and nothing but this test connects them — the file is data, and
    /// a rename would leave these two pointing at a repository that had moved.
    #[test]
    fn the_project_links_match_the_appstream_metadata() {
        const METAINFO: &str =
            include_str!("../../../data/io.github.dominik-najberg.AdGuardUI.metainfo.xml");
        assert!(METAINFO.contains(HOMEPAGE), "homepage is not the one in the metainfo");
        assert!(METAINFO.contains(ISSUES), "issue tracker is not the one in the metainfo");
    }

    /// The measured trap: `check-update` downloads five of its six components
    /// and only checks the sixth, so a control described as a check would
    /// misdescribe nearly all of what it does (contract §14).
    #[test]
    fn the_control_does_not_describe_itself_as_only_a_check() {
        assert_eq!(UPDATE, "Update now");
        let described = WHAT_IT_DOES.to_lowercase();
        assert!(described.starts_with("downloads"), "the download half comes first");
        assert!(described.contains("asks whether"), "and the check half is still named");
    }

    /// The one request this application makes of its own has to be disclosed
    /// where it is made, not in a changelog — a user running an ad blocker is
    /// entitled to know before pressing, and to know it carries nothing about
    /// them.
    #[test]
    fn the_release_check_discloses_what_it_sends() {
        let said = WHAT_THE_CHECK_SENDS.to_lowercase();
        assert!(said.contains("github.com"), "it has to name who is asked");
        assert!(said.contains("only thing this application sends"), "and that it is the only one");
        assert!(said.contains("nothing about you"), "and what it does not carry");
    }

    /// The check now runs by itself, so the disclosure has to say how often —
    /// and the switch beside it is what makes that acceptable.
    #[test]
    fn the_disclosure_says_the_check_runs_by_itself() {
        assert!(WHAT_THE_CHECK_SENDS.contains("once a day"));
    }

    /// The password prompt is said before the button is pressed, so polkit's
    /// dialog is not mistaken for something else asking. Both routes say the
    /// release is checked before it is installed.
    #[test]
    fn the_install_button_says_what_it_will_do() {
        assert!(HOW_A_PACKAGE_UPDATES.contains("password"));
        assert!(!HOW_A_TARBALL_UPDATES.contains("password"), "a tarball install needs none");
        for said in [HOW_A_PACKAGE_UPDATES, HOW_A_TARBALL_UPDATES] {
            assert!(said.contains("signed by this project"), "{said}");
            assert!(said.contains("restarts"), "{said}");
        }
    }

    /// The AdGuard CLI half is never run by this application, and the group
    /// that mentions it has to say why — now beside a page that does install
    /// its own update, and with a button that hands it to a terminal.
    #[test]
    fn the_update_notice_explains_why_it_is_not_a_button() {
        let why = WHY_NO_BUTTON.to_lowercase();
        assert!(why.contains("privileged"));
        assert!(why.contains("yours to run"));
        assert!(why.contains("terminal"));
        assert_eq!(UPDATE_COMMAND, "adguard-cli update");
    }

    /// AdGuard will not update under a running proxy, so the script stops it,
    /// and starts it again even after an update that failed. A stopped proxy
    /// is left stopped.
    #[test]
    fn the_update_steps_around_a_running_proxy() {
        assert_eq!(update_script("'/a b/adguard-cli'", false), "'/a b/adguard-cli' update");
        assert_eq!(
            update_script("cli", true),
            "cli stop; cli update; setsid -w cli start </dev/null >/dev/null 2>&1; cli status"
        );
        assert!(WHY_NO_BUTTON.contains("pauses it first"));
    }

    /// A stable release offered to a beta build is explained; a newer
    /// pre-release, or anything on Release, is left to AdGuard's sentence.
    #[test]
    fn a_stable_release_offered_to_a_beta_says_where_it_is() {
        let why = short_of("1.5.0-rc.3", "1.5.0", Channel::Beta).unwrap();
        assert!(why.contains("Beta channel"), "{why}");
        assert!(why.contains("switches the update channel to Release"), "{why}");
        assert!(short_of("1.5.0-nightly.10", "1.5.0", Channel::Nightly).is_some());
        assert_eq!(short_of("1.5.0-rc.3", "1.5.0-rc.4", Channel::Beta), None);
        assert_eq!(short_of("1.5.0-rc.3", "1.5.0", Channel::Release), None);
        assert_eq!(short_of("1.4.13", "1.5.0", Channel::Beta), None);
    }

    /// The notice repeats AdGuard's sentence, so it reads the same whether it
    /// came from `--version`, a channel switch or `check-update`.
    #[test]
    fn the_notice_is_adguards_sentence() {
        assert_eq!(available("1.5.0-nightly.8"), "AdGuard CLI v1.5.0-nightly.8 is now available");
    }
}
