//! Looking for a newer AdGuard UI, and installing it with one click.
//!
//! One of these per process, built in `main` before anything else and shared by
//! the three places that show its state: the About page, the tray, and a
//! desktop notification. The rules live in `adguard_core::release` and
//! `adguard_core::update`; this is the timing and the plumbing.
//!
//! # Once a day, unless switched off
//!
//! The first check runs a minute after start-up, so it never competes with the
//! window's own first reads, and then whenever a day has passed since the last
//! one. The day is wall-clock time, polled hourly, rather than a 24-hour glib
//! timer: those run on the monotonic clock, which stops while a laptop is
//! suspended, so a machine that sleeps every night would otherwise never reach
//! the end of one.
//!
//! A check that finds a release raises one notification per release per
//! session, with *Update and restart* on it. One that fails says nothing — it
//! is not something the user asked for, and the next one is a day away.
//!
//! # Restarting is the last thing the process does
//!
//! After a successful install the application quits, and `main` replaces the
//! process with the new binary once the main loop has returned (`exec`, not a
//! child: the D-Bus name has to be released before the new copy asks for it,
//! and closing this process's connection is what releases it). The window
//! comes back if it was on screen, and only the tray if it was not.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use adguard_core::release::{self, Release, Standing};
use adguard_core::update::{self, Install, Preferences};
use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::{toast, worker};

/// After start-up, before the first check.
const FIRST_CHECK: Duration = Duration::from_secs(60);

/// How often the schedule asks whether a day has passed.
const POLL: Duration = Duration::from_secs(60 * 60);

/// Between automatic checks.
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The app actions a notification's buttons name.
pub const INSTALL_ACTION: &str = "update-install";
pub const SHOW_ACTION: &str = "update-show";

/// The notification's id, so a second one replaces the first.
const NOTIFICATION: &str = "update";

/// Where the update stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been asked yet.
    Unchecked,
    Checking,
    Checked(Standing),
    Downloading(Release),
    Installing(Release),
}

impl Phase {
    /// The release that is newer than this build, while there is one.
    pub fn available(&self) -> Option<&Release> {
        match self {
            Self::Checked(Standing::Behind(release))
            | Self::Downloading(release)
            | Self::Installing(release) => Some(release),
            _ => None,
        }
    }

    /// Something is in flight and a second request would race it.
    pub fn busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading(_) | Self::Installing(_))
    }
}

/// How the process should come back after an update.
pub struct Restart {
    pub exe: PathBuf,
    pub background: bool,
}

pub struct Updater {
    app: adw::Application,
    install: Install,
    preferences: Option<Preferences>,
    phase: RefCell<Phase>,
    listeners: RefCell<Vec<Box<dyn Fn(&Phase)>>>,
    /// Where messages go while the window is on screen.
    toasts: RefCell<Option<adw::ToastOverlay>>,
    /// When the last check — automatic or not — got an answer.
    last_check: Cell<Option<SystemTime>>,
    /// The release a notification has already been raised for this session.
    notified: RefCell<Option<String>>,
    /// The hourly poll, while automatic checking is on.
    schedule: RefCell<Option<glib::SourceId>>,
    restart: RefCell<Option<Restart>>,
}

impl Updater {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let this = Rc::new(Self {
            app: app.clone(),
            // Read once, now: after an install replaces the binary the running
            // process sees `… (deleted)` (see `Install::detect`).
            install: Install::detect(),
            preferences: Preferences::locate(),
            phase: RefCell::new(Phase::Unchecked),
            listeners: RefCell::new(Vec::new()),
            toasts: RefCell::new(None),
            last_check: Cell::new(None),
            notified: RefCell::new(None),
            schedule: RefCell::new(None),
            restart: RefCell::new(None),
        });

        let install = gio::SimpleAction::new(INSTALL_ACTION, None);
        install.connect_activate({
            let this = Rc::downgrade(&this);
            move |_, _| {
                if let Some(this) = this.upgrade() {
                    this.install();
                }
            }
        });
        app.add_action(&install);
        this
    }

    pub fn install_kind(&self) -> &Install {
        &self.install
    }

    pub fn phase(&self) -> Phase {
        self.phase.borrow().clone()
    }

    /// Called with every change of [`Phase`], and once straight away.
    pub fn connect(&self, listener: impl Fn(&Phase) + 'static) {
        listener(&self.phase.borrow());
        self.listeners.borrow_mut().push(Box::new(listener));
    }

    pub fn set_toasts(&self, toasts: &adw::ToastOverlay) {
        self.toasts.replace(Some(toasts.clone()));
    }

    pub fn automatic(&self) -> bool {
        self.preferences.as_ref().is_some_and(Preferences::automatic)
    }

    /// The switch on the About page.
    pub fn set_automatic(self: &Rc<Self>, on: bool) {
        if let Some(preferences) = &self.preferences {
            if let Err(err) = preferences.set_automatic(on) {
                self.report(&format!("Could not save the setting: {err}"));
            }
        }
        if on {
            self.start_schedule();
        } else if let Some(source) = self.schedule.take() {
            source.remove();
        }
    }

    /// Start the daily check, if it is switched on. Called once the
    /// application is the primary instance — a second launch that only hands
    /// its command line over must not check as well.
    pub fn start_schedule(self: &Rc<Self>) {
        if !self.automatic() || self.schedule.borrow().is_some() {
            return;
        }
        glib::timeout_add_local_once(FIRST_CHECK, {
            let this = Rc::downgrade(self);
            move || {
                if let Some(this) = this.upgrade() {
                    this.check_if_due();
                }
            }
        });
        let source = glib::timeout_add_local(POLL, {
            let this = Rc::downgrade(self);
            move || match this.upgrade() {
                Some(this) => {
                    this.check_if_due();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        self.schedule.replace(Some(source));
    }

    fn check_if_due(self: &Rc<Self>) {
        // Switched off after the first-check timer was already set.
        if self.schedule.borrow().is_none() {
            return;
        }
        let due = match self.last_check.get() {
            None => true,
            // A clock set backwards reads as an error here, and counts as due.
            Some(last) => last.elapsed().map_or(true, |since| since >= INTERVAL),
        };
        if due {
            self.check(false);
        }
    }

    /// Ask GitHub for the newest release. `manual` is a button press, and is
    /// answered whatever the answer; an automatic check speaks only when there
    /// is something to install.
    pub fn check(self: &Rc<Self>, manual: bool) {
        if self.phase.borrow().busy() {
            return;
        }
        let before = self.phase();
        self.set_phase(Phase::Checking);

        let this = self.clone();
        worker::run(release::latest, move |found| {
            let release = match found {
                Ok(release) => release,
                Err(err) => {
                    this.set_phase(before);
                    if manual {
                        this.report(&err.to_string());
                    } else {
                        eprintln!("adguard-ui: automatic update check failed: {err}");
                    }
                    return;
                }
            };
            this.last_check.set(Some(SystemTime::now()));

            let standing = release::standing(env!("CARGO_PKG_VERSION"), release);
            // Before the notification, whose button acts on this phase.
            this.set_phase(Phase::Checked(standing.clone()));
            if manual {
                this.report(&match &standing {
                    Standing::Current => "You have the latest AdGuard UI release".to_owned(),
                    Standing::Behind(release) => format!("{} is available", release.tag),
                    // A local build between releases. Saying "up to date" here
                    // would claim this binary matches a release it does not.
                    Standing::Ahead(release) => format!(
                        "This build is newer than the latest release ({})",
                        release.tag
                    ),
                    Standing::Unknown(release) => format!("The latest release is {}", release.tag),
                });
            } else if let Standing::Behind(release) = &standing {
                this.notify_available(release);
            }
        });
    }

    /// Download, verify, install, restart. What *Update and restart* does,
    /// wherever it was pressed.
    pub fn install(self: &Rc<Self>) {
        let release = match &*self.phase.borrow() {
            Phase::Checked(Standing::Behind(release)) => release.clone(),
            _ => return,
        };
        if !self.install.can_update() {
            self.report(&update::Error::Unmanaged.to_string());
            return;
        }
        self.app.withdraw_notification(NOTIFICATION);
        self.set_phase(Phase::Downloading(release.clone()));

        let install = self.install.clone();
        let this = self.clone();
        worker::run(
            {
                let release = release.clone();
                move || update::download(&install, &release)
            },
            move |downloaded| match downloaded {
                Ok(downloaded) => this.apply(release, downloaded),
                Err(err) => this.failed(release, err),
            },
        );
    }

    fn apply(self: &Rc<Self>, release: Release, downloaded: update::Downloaded) {
        self.set_phase(Phase::Installing(release.clone()));
        let install = self.install.clone();
        let this = self.clone();
        worker::run(
            move || update::apply(&install, &downloaded),
            move |applied| match applied {
                Ok(()) => this.restart(),
                Err(err) => this.failed(release, err),
            },
        );
    }

    fn failed(&self, release: Release, err: update::Error) {
        self.set_phase(Phase::Checked(Standing::Behind(release)));
        match err {
            update::Error::Cancelled => self.report("Update cancelled"),
            err => self.report(&err.to_string()),
        }
    }

    /// Quit, and leave `main` the instruction to start the new binary.
    fn restart(&self) {
        let background = !self.app.windows().iter().any(|window| window.is_visible());
        self.restart.replace(Some(Restart {
            exe: self.install.exe().to_owned(),
            background,
        }));
        self.app.quit();
    }

    /// What `main` runs once the main loop has returned, if an update went in.
    pub fn take_restart(&self) -> Option<Restart> {
        self.restart.take()
    }

    fn set_phase(&self, phase: Phase) {
        if *self.phase.borrow() == phase {
            return;
        }
        self.phase.replace(phase.clone());
        for listener in self.listeners.borrow().iter() {
            listener(&phase);
        }
    }

    /// A toast while the window is on screen, and a notification while only
    /// the tray is — an update started from the tray has nowhere else to say
    /// how it went.
    fn report(&self, message: &str) {
        let visible = self.app.windows().iter().any(|window| window.is_visible());
        if visible {
            if let Some(toasts) = &*self.toasts.borrow() {
                toasts.add_toast(toast(message));
                return;
            }
        }
        let notification = gio::Notification::new("AdGuard UI");
        notification.set_body(Some(message));
        self.app.send_notification(Some(NOTIFICATION), &notification);
    }

    /// Once per release per session, so a daily check on a machine that has
    /// not restarted does not repeat itself.
    fn notify_available(&self, release: &Release) {
        if self.notified.borrow().as_deref() == Some(release.tag.as_str()) {
            return;
        }
        self.notified.replace(Some(release.tag.clone()));

        let notification = gio::Notification::new(&format!("AdGuard UI {} is available", release.tag));
        if self.install.can_update() {
            notification.set_body(Some(if self.install.needs_password() {
                "Update now: it is downloaded from GitHub, its signature is checked, and your \
                 password is asked for to install it."
            } else {
                "Update now: it is downloaded from GitHub, its signature is checked, and AdGuard UI \
                 restarts."
            }));
            notification.add_button("Update and restart", &format!("app.{INSTALL_ACTION}"));
        } else {
            notification.set_body(Some("See the About page for how to install it."));
        }
        notification.set_default_action(&format!("app.{SHOW_ACTION}"));
        self.app.send_notification(Some(NOTIFICATION), &notification);
    }
}
