//! The rows that report whether AdGuard's certificate is trusted, and carry
//! AdGuard's own command for installing it (`docs/architecture.md` §6).
//!
//! The shape is the root helper's, because the problem is the same one: a
//! privileged step this application will not take, an upstream command that
//! takes it, and a check that has to render the unmet state rather than merely
//! prevent it. What differs is that the certificate matters on two screens —
//! the Protection page, below the switch it qualifies, and the first-run
//! assistant, which is where the state is *created*, because `configure`
//! generates the CA and silently skips installing it (contract §7). Hence a
//! module of its own rather than a second copy of `AdvancedPage::paint_helper`.
//!
//! [`adguard_core::CaTrust`] and [`adguard_core::BrowserStores`] do the looking;
//! everything here is wording. The browsers' stores share this group rather
//! than taking one of their own because they share its remedy: the command that
//! installs the certificate into the system store is the same one that adds it
//! to Firefox and Chrome, and two groups offering one command would read as two
//! things to do.
//!
//! **The one thing this file runs is AdGuard's own browser step**, since the
//! last item of [issue #21]: *Add to Browsers* runs `certutil -A` against each
//! browser store that lacks the certificate, exactly as `install_cert.sh` does
//! for each store it finds (`adguard_core::nss::add`). It touches only the
//! user's own files and needs no password, so it can be a button. The system
//! store's half needs `sudo` and stays a command with a copy button, exactly as
//! with the `sudo` command on the Advanced page.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adguard_core::nss::{self, BrowserStores, Store, StoreState};
use adguard_core::trust::{self, CaTrust};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::root_helper::join_with_and;
use crate::{abbreviate, toast, worker};

/// A group of up to three rows: what the system store says, what the browsers'
/// own stores say, and what to run about it.
///
/// Hidden entirely when the system and every browser store trust the
/// certificate, or when HTTPS filtering is off and so nothing depends on it. A requirement that is met is not worth a
/// standing row, and a permanent one would invite a user to run a `sudo`
/// command they do not need.
pub struct CertificateView {
    group: adw::PreferencesGroup,
    status: adw::ActionRow,
    /// The browsers whose own stores lack the certificate. Hidden when none do.
    browsers: adw::ActionRow,
    /// On the browsers row: adds the certificate to every store that lacks it.
    /// Shown only with a `certutil` to run.
    add: gtk::Button,
    command: adw::ActionRow,
    /// What the last paint was given, so the view can paint itself again once
    /// the button's work is done.
    last: RefCell<Option<(Option<bool>, String)>>,
    /// The stores the button would change, and the certificate, as last read.
    pending: RefCell<Option<(Vec<Store>, PathBuf)>>,
    /// The button's work is running.
    adding: Cell<bool>,
    /// The last reading rendered, so a re-check that found nothing new does not
    /// rebuild rows under the user's pointer.
    painted: RefCell<Option<String>>,
    /// AdGuard's installer, resolved once at construction.
    ///
    /// A field rather than a call per paint, for the same reason
    /// `AdvancedPage::helper_path` is one: an override that changed underneath
    /// a running window would make the row's history impossible to follow.
    /// `$ADGUARD_CERT_INSTALLER` overrides it, which is what makes the
    /// installer-missing branch reachable on a machine that has one.
    installer: Option<PathBuf>,
}

impl CertificateView {
    pub fn new(toasts: &adw::ToastOverlay) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("AdGuard's certificate")
            .build();

        let status = adw::ActionRow::new();
        status.set_use_markup(false);
        status.set_title("Certificate");
        status.set_subtitle_lines(4);
        status.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        group.add(&status);

        let browsers = adw::ActionRow::new();
        browsers.set_use_markup(false);
        browsers.set_title("Browsers");
        // Up to three clauses, each naming stores, and one naming a path: more
        // room than the other rows, which carry one fact each.
        browsers.set_subtitle_lines(8);
        browsers.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let add = gtk::Button::builder()
            .label("Add to Browsers")
            .valign(gtk::Align::Center)
            .tooltip_text(
                "Add AdGuard's certificate to these browsers' own stores, as AdGuard's installer \
                 does. No password is needed: only your own files change.",
            )
            .build();
        add.add_css_class("suggested-action");
        browsers.add_suffix(&add);
        group.add(&browsers);

        let command = adw::ActionRow::new();
        command.set_use_markup(false);
        command.set_title("Run this in a terminal");
        // Six, not three: with a second Firefox profile the installer is on the
        // line twice, and a command cut off mid-path cannot be checked before
        // it is pasted.
        command.set_subtitle_lines(6);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy the command"));
        copy.set_valign(gtk::Align::Center);
        copy.add_css_class("flat");
        // **Weak, both of them.** The row owns the button, the button owns this
        // closure, and a strong `command` in it would close a GObject cycle
        // that nothing breaks — the row, its subtitle and this button would
        // outlive every rebuild of the page. `toasts` is worse: the overlay is
        // an ancestor of the whole view, so holding it strongly from a leaked
        // row keeps the entire widget tree alive, including the first-run
        // assistant's after it has handed the window over.
        copy.connect_clicked({
            let toasts = toasts.downgrade();
            let command = command.downgrade();
            move |_| {
                let (Some(command), Some(toasts)) = (command.upgrade(), toasts.upgrade()) else {
                    return;
                };
                let text = command.subtitle().unwrap_or_default();
                command.clipboard().set_text(&text);
                toasts.add_toast(toast("Command copied"));
            }
        });
        command.add_suffix(&copy);
        group.add(&command);

        let this = Rc::new(Self {
            group,
            status,
            browsers,
            add,
            command,
            last: RefCell::new(None),
            pending: RefCell::new(None),
            adding: Cell::new(false),
            painted: RefCell::new(None),
            installer: std::env::var_os("ADGUARD_CERT_INSTALLER")
                .map(PathBuf::from)
                .or_else(adguard_core::paths::cert_installer),
        });
        this.add.connect_clicked({
            // Weak, for the reason the copy button's closure is.
            let this = Rc::downgrade(&this);
            let toasts = toasts.downgrade();
            move |_| {
                if let (Some(this), Some(toasts)) = (this.upgrade(), toasts.upgrade()) {
                    this.add_to_browsers(&toasts);
                }
            }
        });
        this
    }

    /// Run the installer's browser step on every store that lacks the
    /// certificate, then read everything again.
    ///
    /// Act, re-read, reconcile: `nss::add` already checks each store it wrote,
    /// and the repaint afterwards reads them all a second time, so the rows say
    /// what the stores hold rather than what was attempted.
    fn add_to_browsers(self: &Rc<Self>, toasts: &adw::ToastOverlay) {
        let Some((stores, certificate)) = self.pending.borrow().clone() else {
            return;
        };
        let Some(certutil) = nss::certutil() else {
            toasts.add_toast(toast("certutil could not be found"));
            return;
        };
        if self.adding.replace(true) {
            return;
        }
        self.add.set_sensitive(false);
        self.add.set_label("Adding…");

        let this = self.clone();
        let toasts = toasts.clone();
        worker::run(
            move || {
                stores
                    .iter()
                    .map(|store| (store.name(), nss::add(store, &certutil, &certificate)))
                    .collect::<Vec<_>>()
            },
            move |results: Vec<(String, Result<(), String>)>| {
                this.adding.set(false);
                this.add.set_sensitive(true);
                this.add.set_label("Add to Browsers");
                let failed: Vec<_> = results.iter().filter(|(_, result)| result.is_err()).collect();
                let message = match (failed.first(), results.len()) {
                    (None, 1) => "Added to 1 browser store. An open browser may need restarting"
                        .to_owned(),
                    (None, n) => {
                        format!("Added to {n} browser stores. An open browser may need restarting")
                    }
                    (Some((name, Err(why))), _) => format!("Could not add it to {name}: {why}"),
                    (Some(_), _) => unreachable!("only failures are collected"),
                };
                toasts.add_toast(toast(&message));
                this.painted.replace(None);
                let last = this.last.borrow().clone();
                if let Some((filtering, name)) = last {
                    this.paint(filtering, &name);
                }
            },
        );
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Re-read the check and render it.
    ///
    /// `filtering` is whether HTTPS filtering is switched on — `None` when that
    /// could not be read. An untrusted certificate is only a problem for the
    /// traffic AdGuard decrypts, so with the switch off there is nothing to
    /// report and the group goes away; an unreadable switch is treated as on,
    /// because the two mistakes are not symmetric and the row is only ever an
    /// explanation with a command beside it.
    ///
    /// Cheap enough for the main loop, which is a measurement rather than an
    /// assumption: three file reads, the largest of them the ~200 KB system
    /// bundle, at **0.52 ms** a call in a debug build on the reference machine
    /// — a thirtieth of a frame, and pinned by a test with a 50 ms bound. The
    /// browser stores add two read-only SQLite queries on that machine,
    /// **0.39 ms** together, under a bound of their own. It is
    /// re-read every time rather than cached, for the reason the helper check
    /// is: the user's way out is a command they run elsewhere, so a cache would
    /// be stale at exactly the moment that matters.
    pub fn paint(&self, filtering: Option<bool>, certificate_name: &str) {
        self.last.replace(Some((filtering, certificate_name.to_owned())));
        let check = (filtering != Some(false))
            .then(|| CaTrust::detect(certificate_name))
            .flatten();
        // Only once there is a certificate to compare: with none, every store
        // would read as missing, and the one thing to do is generate it.
        let stores = check
            .as_ref()
            .filter(|trust| trust.generated)
            .and_then(|trust| BrowserStores::detect(&trust.certificate));
        let unmet = stores.as_ref().map(BrowserStores::unmet).unwrap_or_default();

        // The remedy is computed before the snapshot, and goes into it, because
        // it does not follow from the check alone: it stats the installer and
        // may look up the CLI binary, and either can appear or vanish while the
        // window is open — a reinstall of AdGuard CLI is exactly the case, and
        // it is one this row would otherwise keep denying long after it stopped
        // being true. Snapshotting the check alone would have made the guard
        // suppress precisely the repaint worth making.
        let remedy = check.as_ref().and_then(|trust| self.remedy(trust, &unmet));
        let certutil = nss::certutil();
        let snapshot = format!("{check:?} {stores:?} {remedy:?} {certutil:?}");
        if self.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return;
        }
        self.painted.replace(Some(snapshot));

        // Nothing to say: filtering is off, or AdGuard's data directory could
        // not be located — which the window is already explaining elsewhere.
        let Some(trust) = check else {
            self.group.set_visible(false);
            return;
        };

        if trust.is_trusted() && unmet.is_empty() {
            self.group.set_visible(false);
            return;
        }

        self.group.set_visible(true);
        // A system store that trusts the certificate is not worth a warning
        // row of its own; the browsers' row says it in passing.
        self.status.set_visible(!trust.is_trusted());
        self.status.set_subtitle(&self.explain(&trust, filtering));
        self.browsers.set_visible(!unmet.is_empty());
        if let Some(stores) = &stores {
            self.browsers.set_subtitle(&explain_stores(stores, trust.is_trusted()));
        }
        let button = !unmet.is_empty() && certutil.is_some();
        self.add.set_visible(button);
        self.pending.replace(button.then(|| {
            (unmet.iter().map(|store| (*store).clone()).collect(), trust.certificate.clone())
        }));

        match remedy {
            Some(remedy) => {
                self.command.set_visible(!remedy.command.is_empty());
                self.command.set_subtitle(&remedy.command);
                let mut description = remedy.description;
                if button {
                    description.push(' ');
                    description.push_str(BUTTON);
                }
                self.group.set_description(Some(&description));
            }
            // No command to show. The state is still worth showing — it is the
            // reason HTTPS pages will fail — but the two reasons are not the
            // same fact and the group must not assert the wrong one: an
            // installer that is missing, or a path that cannot be put in a
            // shell command without changing what it would do.
            None => {
                self.command.set_visible(false);
                let unshowable = !trust::quotable(&trust.certificate)
                    || self
                        .installer
                        .as_ref()
                        .is_some_and(|path| !trust::quotable(path))
                    || unmet
                        .iter()
                        .filter_map(|store| store.profile_flag())
                        .any(|dir| !trust::quotable(dir));
                self.group
                    .set_description(Some(if unshowable { UNSHOWABLE } else { NO_INSTALLER }));
            }
        }
    }

    /// What the check found, in the order the machine applies it.
    ///
    /// The opening clause is not decoration: it says *why this row is here at
    /// all*, and it must not assert a switch state that was never read. An
    /// unreadable `https_filtering.enabled` is why these rows are shown at all
    /// in that case — the same rule as everywhere else in this app, that a fact
    /// we could not read is never rendered as the reassuring answer — but the
    /// row says so rather than claiming filtering is on. The Protection switch
    /// immediately above is already reading *unavailable*, and two rows
    /// disagreeing about the same key would be worse than either.
    fn explain(&self, trust: &CaTrust, filtering: Option<bool>) -> String {
        let missing = trust
            .unmet()
            .first()
            .copied()
            .unwrap_or("it is not trusted");
        let why = match filtering {
            Some(_) => "HTTPS filtering is on, but",
            None => "Whether HTTPS filtering is on could not be read, and",
        };

        if !trust.generated {
            return format!(
                "{why} {missing} — nothing is at {}. \
                 Filtered pages will fail to load until there is.",
                abbreviate(&trust.certificate)
            );
        }

        // Name the place the answer came from, so the row can be checked rather
        // than believed. Which place that is depends on which question failed.
        let where_ = match (trust.stale, trust.anchored) {
            (true, _) => match &trust.anchor {
                Some(anchor) => format!(" {} holds another one.", anchor.display()),
                None => String::new(),
            },
            (_, true) => match &trust.anchor {
                Some(anchor) => format!(" It is at {}.", anchor.display()),
                None => String::new(),
            },
            // Not installed at all. The bundle is what decides, so it is named
            // first; with no bundle, the directory the installer would have
            // written to is the next most useful thing to have looked at.
            _ => match (&trust.bundle, &trust.anchor) {
                (Some(bundle), _) => format!(" Checked {}.", bundle.display()),
                (None, Some(anchor)) => format!(" Checked {}.", anchor.display()),
                // Neither location exists: an unrecognised distribution rather
                // than an untrusted certificate, and worth saying so rather
                // than implying the user forgot a step.
                (None, None) => String::from(
                    " This machine has none of the trust-store locations AdGuard's \
                     installer knows about.",
                ),
            },
        };

        format!("{why} {missing}.{where_}")
    }

    /// The command that moves this machine to the next state, or `None` when
    /// there is nothing honest to name.
    ///
    /// Each carries its own explanation, because they are different programs
    /// doing different things and one description covering all of them would
    /// be wrong about most — the group used to say "the command below is
    /// AdGuard's own installer" over `adguard-cli cert`, which generates rather
    /// than installs.
    ///
    /// `unmet` is the browser stores that lack the certificate. They change the
    /// command only by what AdGuard's installer has to be run for: once more
    /// after the system store is done, and once per Firefox profile it would
    /// not find by itself.
    fn remedy(&self, trust: &CaTrust, unmet: &[&Store]) -> Option<Remedy> {
        remedy(self.installer.as_deref(), trust, unmet)
    }
}

/// [`CertificateView::remedy`], with the installer as a parameter so every
/// branch can be tested without a window.
fn remedy(installer: Option<&Path>, trust: &CaTrust, unmet: &[&Store]) -> Option<Remedy> {
    if trust.is_trusted() && unmet.is_empty() {
        return None;
    }
    if !trust.generated {
        // A different program: `install_cert.sh` installs a certificate, it
        // does not make one. AdGuard's own help for this is `cert`
        // ("Generate a certificate for HTTPS filtering"), which generates
        // and then offers to install in the same run.
        let cli = adguard_core::paths::cli_binary().filter(|path| trust::quotable(path))?;
        return Some(Remedy {
            command: format!("\"{}\" cert", cli.display()),
            description: GENERATE.to_owned(),
        });
    }

    // Installed already, just not in the bundle, and no browser waiting on
    // the installer: the step AdGuard's script takes after copying the
    // file, and the only one still outstanding. No installer needed, so
    // this is answered before one is required.
    let rebuild = trust.anchored && !trust.bundled;
    if rebuild && unmet.is_empty() {
        return Some(Remedy {
            command: trust::refresh_command(),
            description: REBUILD.to_owned(),
        });
    }

    let installer = installer.filter(|path| path.is_file())?;
    let install = trust::install_command(installer, &trust.certificate)?;
    let mut steps = Vec::new();
    let mut description = String::from(match (trust.stale, &trust.anchor) {
        // The one state AdGuard's installer cannot repair: it tests whether
        // the anchor path exists and stops if it does, so the old
        // certificate has to go first. One line, because two rows would
        // leave the user holding half a fix — and the `rm` gets the same
        // quoting check as everything else on the line, since this is the
        // one command here that destroys something.
        (true, Some(anchor)) => {
            if !trust::quotable(anchor) {
                return None;
            }
            steps.push(format!("sudo rm \"{}\"", anchor.display()));
            steps.push(install.clone());
            REPLACE
        }
        // The installer finds the anchor and skips the rebuild, so the
        // rebuild goes first and the installer after it, for the browsers.
        _ if rebuild => {
            steps.push(trust::refresh_command());
            steps.push(install.clone());
            REBUILD_AND_BROWSERS
        }
        _ if !trust.is_trusted() => {
            steps.push(install.clone());
            INSTALL
        }
        // The system is done. The installer is still the command — it
        // finds the certificate installed, says so, and goes on to the
        // browsers — but only if a store it reaches by itself is waiting.
        _ => {
            if unmet
                .iter()
                .any(|store| store.profile_flag().is_none() && store.installer_reaches())
            {
                steps.push(install.clone());
            }
            if unmet.iter().all(|store| !store.installer_reaches()) {
                // Only Flatpak Chromium stores are waiting, and the installer
                // cannot reach any of them: there is no command to show, and
                // the button is the whole remedy.
                return Some(Remedy {
                    command: String::new(),
                    description: FLATPAK_ONLY.to_owned(),
                });
            }
            BROWSERS
        }
    });

    let mut named = unmet.iter().filter_map(|store| store.profile_flag()).peekable();
    if named.peek().is_some() {
        for dir in named {
            steps.push(trust::install_command_for_profile(
                installer,
                &trust.certificate,
                dir,
            )?);
        }
        description.push(' ');
        description.push_str(PROFILES);
    }

    if unmet.iter().any(|store| !store.installer_reaches()) {
        description.push(' ');
        description.push_str(FLATPAK);
    }

    Some(Remedy {
        command: steps.join(" && "),
        description,
    })
}

/// A command to show, and the sentence that says what it does.
///
/// `Debug` because it goes into the repaint snapshot: what the rows will say
/// is what has to be compared, not what the check found.
#[derive(Debug)]
struct Remedy {
    command: String,
    description: String,
}

/// What the browsers' own stores say, grouped by what is wrong.
///
/// Grouped for the reason the browser-integration row groups: the ordinary
/// case is every store in the same state, and a sentence per store would be a
/// wall. `system_trusted` opens the row with the one fact the status row is
/// hidden for — that the machine itself is fine — so the user is not left
/// wondering why a trusted certificate is being talked about at all.
fn explain_stores(stores: &BrowserStores, system_trusted: bool) -> String {
    let named = |wanted: fn(&StoreState) -> bool| -> Vec<String> {
        stores
            .stores
            .iter()
            .filter(|store| wanted(&store.state))
            .map(Store::name)
            .collect()
    };
    let join = |names: &[String]| {
        join_with_and(&names.iter().map(String::as_str).collect::<Vec<_>>())
    };

    let mut clauses: Vec<String> = Vec::new();
    if system_trusted {
        clauses.push(String::from(
            "This machine trusts AdGuard's certificate, but browsers keep certificate stores \
             of their own.",
        ));
    } else {
        clauses.push(String::from("Browsers keep certificate stores of their own too."));
    }

    let missing = named(|state| *state == StoreState::Missing);
    if !missing.is_empty() {
        // Name one database that was read, so the row can be checked rather
        // than believed.
        let where_ = stores
            .stores
            .iter()
            .find(|store| store.state == StoreState::Missing)
            .map(|store| abbreviate(&store.database))
            .unwrap_or_default();
        clauses.push(format!(
            "AdGuard's certificate is missing from the {} of {}, so filtered pages will fail \
             to load there. Checked {where_}.",
            store_or_stores(missing.len()),
            join(&missing)
        ));
    }

    let untrusted = named(|state| *state == StoreState::Untrusted);
    if !untrusted.is_empty() {
        clauses.push(format!(
            "The {} of {} {} it, but not trusted to identify websites — its trust was removed \
             in the browser's settings, or it was added without any.",
            store_or_stores(untrusted.len()),
            join(&untrusted),
            if untrusted.len() == 1 { "holds" } else { "hold" }
        ));
    }

    for store in &stores.stores {
        if let StoreState::Unreadable(why) = &store.state {
            clauses.push(format!("The store of {} could not be read — {why}.", store.name()));
        }
    }

    clauses.join(" ")
}

/// The ordinary case: a certificate that exists and has never been installed.
///
/// The last sentence is the root-helper group's, for the same reason: the step
/// needs a password, and a GUI that collects one to run a shell script as root
/// is a different proposition from a user typing `sudo` at their own prompt
/// (`architecture.md` §6). The browser note is not padding: the script writes
/// to the browsers' stores in the same run, and a user who has been told only
/// about the system would not expect the browsers row to clear too.
const INSTALL: &str = "Filtered connections are signed by a certificate this machine has to \
                       trust. The command below is AdGuard's own installer; it asks for your \
                       password itself, and it adds the certificate to Firefox and Chrome as \
                       well as to the system store. This application never runs it for you.";

/// The same, plus the removal AdGuard's installer will not do for itself.
const REPLACE: &str = "A certificate of this name is already installed, but it is a different \
                       one — from an earlier AdGuard install, or from before this certificate \
                       was regenerated. AdGuard's installer stops when it finds a file of that \
                       name and leaves the old one in place, so the command below removes it \
                       first and then runs the installer. This application never runs it for you.";

/// Nothing to install yet. A different program, so a different sentence.
const GENERATE: &str = "AdGuard generates this certificate itself, and there is none here. The \
                        command below is AdGuard's own; it generates one and then offers to \
                        install it, asking for your password. This application never runs it \
                        for you.";

/// "store" for one, "stores" for several. Every store is somebody's single
/// store, so the noun counts and the browser names do not — "Chromium-based
/// browsers" is one store.
fn store_or_stores(count: usize) -> &'static str {
    if count == 1 {
        "store"
    } else {
        "stores"
    }
}

/// The system trusts the certificate and only the browsers are outstanding.
///
/// "No password" is measured, not hoped: with the anchor already in place the
/// script prints "Certificate already exists in system trust store" and never
/// reaches its `sudo`, and `certutil` writes to the user's own stores.
const BROWSERS: &str = "Filtered connections are signed by a certificate every browser has to \
                        trust, and Firefox and Chrome keep their own list rather than reading \
                        the system's. The command below is AdGuard's own installer: it finds the \
                        certificate already installed for the system and adds it to the \
                        browsers, without asking for a password. A browser that is open may need \
                        restarting to notice. This application never runs it for you.";

/// The rebuild is outstanding and the browsers are too. The installer skips
/// the rebuild when the anchor is in place, so both are on the line.
const REBUILD_AND_BROWSERS: &str = "The certificate is already in the system's certificate \
                                    directory, but the trust store has not been rebuilt from \
                                    it, and the browsers listed above do not have it either. \
                                    The command below rebuilds the trust store, which asks for \
                                    your password, and then runs AdGuard's own installer, which \
                                    adds the certificate to the browsers. This application never \
                                    runs it for you.";

/// Appended when the button is shown: what it does, in the terms the command's
/// sentence has just used.
const BUTTON: &str = "Or press Add to Browsers, which runs the installer's own step for each \
                      browser here: no password, and only your own files change.";

/// Appended when a Flatpak Chromium store is waiting, which the installer's
/// command cannot reach.
const FLATPAK: &str = "AdGuard's installer does not know about Flatpak browsers, so only Add to \
                       Browsers reaches those.";

/// Only Flatpak Chromium stores are waiting, and the system is done.
const FLATPAK_ONLY: &str = "Filtered connections are signed by a certificate every browser has to \
                            trust, and Flatpak browsers keep their own list. AdGuard's own \
                            installer does not know about them, so there is no command for this; \
                            Add to Browsers adds it.";

/// Appended when a Firefox profile other than the default one needs it.
const PROFILES: &str = "AdGuard's installer finds only the profile Firefox starts by default, so \
                        it is run once more for each other profile, named with its -f option.";

/// The file is in place and only the rebuild is outstanding.
const REBUILD: &str = "The certificate is already in the system's certificate directory, but the \
                       trust store has not been rebuilt from it — so nothing is reading it yet. \
                       The command below is the step AdGuard's installer runs last. This \
                       application never runs it for you.";

/// The state is real but the fix cannot be named.
const NO_INSTALLER: &str = "Filtered connections are signed by a certificate this machine has to \
                            trust. AdGuard's own installer, install_cert.sh, is not beside the \
                            adguard-cli binary on this machine, so there is no command to show \
                            you — reinstalling AdGuard CLI restores it.";

/// The fix exists, but writing it down would be unsafe.
///
/// Never seen on an ordinary install: the seeded certificate name is `AdGuard
/// CLI CA` and even spaces, brackets and accents are fine. It takes a name
/// deliberately built to break out of AdGuard's quoting, which `config set`
/// will accept like any other string — and the row this application offers to
/// the clipboard is one a user may well paste behind a `sudo`.
const UNSHOWABLE: &str = "Filtered connections are signed by a certificate this machine has to \
                          trust, and the installer for it is here — but a path the command would \
                          carry contains characters that cannot be written into a shell command \
                          safely, such as a quotation mark, a backtick, a dollar sign or a line \
                          break. Rather than show you a command that might not do what it says, \
                          this application shows none. The certificate's name comes from \
                          https_filtering.root_certificate_name, and a Firefox profile's from \
                          Firefox's profile manager.";

#[cfg(test)]
mod tests {
    use super::*;
    use adguard_core::nss::Profile;

    /// A certificate that exists, in whichever of the system's states the test
    /// names. Built rather than read, because the reference machine is in only
    /// one of them.
    fn trust(anchored: bool, bundled: bool) -> CaTrust {
        CaTrust {
            certificate: PathBuf::from("/data/AdGuard CLI CA.pem"),
            generated: true,
            anchor: Some(PathBuf::from("/usr/local/share/ca-certificates/AdGuard CLI CA.crt")),
            anchored,
            stale: false,
            bundle: Some(PathBuf::from("/etc/ssl/certs/ca-certificates.crt")),
            bundled,
        }
    }

    fn chromium(state: StoreState) -> Store {
        Store {
            browser: "Chromium-based browsers",
            scanned: true,
            profile: None,
            database: PathBuf::from("/h/.pki/nssdb/cert9.db"),
            state,
        }
    }

    fn firefox(name: &str, default: bool, state: StoreState) -> Store {
        Store {
            browser: "Firefox",
            scanned: true,
            profile: Some(Profile {
                name: name.to_owned(),
                number: 1,
                default,
                dir: PathBuf::from(format!("/h/.mozilla/firefox/abcd.{name}")),
            }),
            database: PathBuf::from(format!("/h/.mozilla/firefox/abcd.{name}/cert9.db")),
            state,
        }
    }

    /// An installer that exists, because the remedy refuses to name one that
    /// does not.
    fn installer() -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("adguard-ui-certificate-{}", std::process::id()))
            .join("install_cert.sh");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        path
    }

    /// The case this change is for: the system trusts the CA and a browser
    /// does not. The command is the installer alone — it finds the system done
    /// and goes on to the browsers — and the sentence says no password.
    #[test]
    fn a_trusted_system_with_a_browser_missing_gets_the_installer() {
        let installer = installer();
        let store = chromium(StoreState::Missing);
        let remedy = remedy(Some(&installer), &trust(true, true), &[&store]).unwrap();
        assert_eq!(
            remedy.command,
            trust::install_command(&installer, Path::new("/data/AdGuard CLI CA.pem")).unwrap()
        );
        assert!(remedy.description.contains("without asking for a password"), "{remedy:?}");
    }

    /// A second Firefox profile is named with `-f`, and the plain run is left
    /// out when nothing it reaches by itself is waiting.
    #[test]
    fn a_second_profile_alone_is_named_and_nothing_else_runs() {
        let installer = installer();
        let store = firefox("work", false, StoreState::Untrusted);
        let remedy = remedy(Some(&installer), &trust(true, true), &[&store]).unwrap();
        assert_eq!(remedy.command.matches("install_cert.sh").count(), 1, "{remedy:?}");
        assert!(remedy.command.ends_with(" -f \"/h/.mozilla/firefox/abcd.work\""), "{remedy:?}");
        assert!(remedy.description.ends_with(PROFILES), "{remedy:?}");
    }

    /// Untrusted by the system as well: the ordinary install, then the named
    /// profile after it.
    #[test]
    fn an_untrusted_system_installs_first_and_names_the_profile_after() {
        let installer = installer();
        let (default, work) = (
            firefox("default", true, StoreState::Missing),
            firefox("work", false, StoreState::Missing),
        );
        let remedy = remedy(Some(&installer), &trust(false, false), &[&default, &work]).unwrap();
        let steps: Vec<_> = remedy.command.split(" && ").collect();
        assert_eq!(steps.len(), 2, "{remedy:?}");
        assert!(!steps[0].contains(" -f "), "{remedy:?}");
        assert!(steps[1].contains(" -f "), "{remedy:?}");
        assert!(remedy.description.starts_with(INSTALL), "{remedy:?}");
    }

    /// The installer skips its rebuild when the anchor is in place, so a
    /// browser waiting behind an unrebuilt store needs both on the line — and
    /// with no browser waiting, the rebuild alone, as before.
    #[test]
    fn a_pending_rebuild_runs_before_the_installer_only_when_a_browser_needs_it() {
        let installer = installer();
        let store = chromium(StoreState::Missing);
        let both = remedy(Some(&installer), &trust(true, false), &[&store]).unwrap();
        assert!(both.command.starts_with(&trust::refresh_command()), "{both:?}");
        assert!(both.command.contains("install_cert.sh"), "{both:?}");
        assert_eq!(both.description, REBUILD_AND_BROWSERS);

        let alone = remedy(Some(&installer), &trust(true, false), &[]).unwrap();
        assert_eq!(alone.command, trust::refresh_command());
    }

    /// A Flatpak Chromium store is out of the installer's reach: alone, there is
    /// no command; beside a store it reaches, the command stays and says so.
    #[test]
    fn a_flatpak_store_has_no_command_of_its_own() {
        let installer = installer();
        let mut flatpak = chromium(StoreState::Missing);
        flatpak.browser = "Chrome (Flatpak)";
        flatpak.scanned = false;
        let alone = remedy(Some(&installer), &trust(true, true), &[&flatpak]).unwrap();
        assert!(alone.command.is_empty(), "{alone:?}");
        assert_eq!(alone.description, FLATPAK_ONLY);

        let native = chromium(StoreState::Missing);
        let both = remedy(Some(&installer), &trust(true, true), &[&flatpak, &native]).unwrap();
        assert!(both.command.contains("install_cert.sh"), "{both:?}");
        assert!(both.description.ends_with(FLATPAK), "{both:?}");
    }

    /// Nothing to do, nothing named.
    #[test]
    fn a_machine_with_nothing_unmet_has_no_remedy() {
        assert!(remedy(Some(&installer()), &trust(true, true), &[]).is_none());
    }

    /// A profile directory that cannot be quoted withholds the whole command,
    /// rather than showing one that quietly leaves that profile out.
    #[test]
    fn an_unquotable_profile_withholds_the_command() {
        let mut store = firefox("work", false, StoreState::Missing);
        store.profile.as_mut().unwrap().dir = PathBuf::from("/h/.mozilla/firefox/$(id)");
        assert!(remedy(Some(&installer()), &trust(true, true), &[&store]).is_none());
    }

    /// The sentence, in both numbers, naming the file it read.
    #[test]
    fn the_browsers_row_reads_as_sentences() {
        let one = BrowserStores {
            stores: vec![chromium(StoreState::Missing), firefox("default", true, StoreState::Trusted)],
        };
        let text = explain_stores(&one, true);
        assert!(text.starts_with("This machine trusts AdGuard's certificate"), "{text}");
        assert!(
            text.contains("certificate is missing from the store of Chromium-based browsers, so"),
            "{text}"
        );
        assert!(text.contains("Checked /h/.pki/nssdb/cert9.db."), "{text}");
        assert!(!text.contains("default"), "{text}");

        let several = BrowserStores {
            stores: vec![
                firefox("default", true, StoreState::Untrusted),
                firefox("work", false, StoreState::Untrusted),
                chromium(StoreState::Unreadable(String::from("database is locked"))),
            ],
        };
        let text = explain_stores(&several, false);
        assert!(
            text.contains(
                "The stores of Firefox profile “default” and Firefox profile “work” hold it, but \
                 not trusted"
            ),
            "{text}"
        );
        assert!(
            text.ends_with(
                "The store of Chromium-based browsers could not be read — database is locked."
            ),
            "{text}"
        );
    }
}
