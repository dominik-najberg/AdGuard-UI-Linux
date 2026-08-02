//! The first-run assistant — what replaces the TTY-only `configure` wizard.
//!
//! # Why this is not simply "discrete `config set` calls"
//!
//! `architecture.md` §5 described this page as an `AdwNavigationView` issuing
//! discrete `config set` calls, and contract §10 forbade this codebase from
//! ever invoking `configure`. Measured on v1.4.13, those two cannot both hold.
//! A data directory that has never been configured has no `proxy.yaml`, and
//! until it does, `config set` refuses **every** real key:
//!
//! ```text
//! No configuration YAML file
//! You can only configure the 'log_level' and 'update_channel'
//! ```
//!
//! Nothing else creates that file — `config get`, `config set` and `activate`
//! were each run against a virgin directory and none of them produced one. So
//! the assistant is two movements, not one: **seed, then set**. The seed is a
//! single [`Cli::configure`] call, which is guarded so it can only ever run
//! when the file is absent; everything after it is the ordinary write path
//! every other page uses.
//!
//! # The order the pages are in is the order the CLI forces
//!
//! - **Licence first.** `configure` is licence-gated, so an unlicensed install
//!   cannot be set up at all. The welcome page says so and sends the user to
//!   the app proper, where the Status page already carries activation.
//! - **Seed on an explicit press**, never on page load. It writes a real file,
//!   and a user who quits immediately afterwards is left with exactly the
//!   configuration `adguard-cli configure` would have given them — defaults,
//!   intact, nothing half-applied.
//! - **Ask afterwards, pre-filled from the seeded file.** The defaults shown
//!   are read back from `proxy.yaml` rather than hardcoded here, so they stay
//!   AdGuard's defaults rather than becoming a copy of them that can drift.
//!   Only settings the user actually moved are written, so answering nothing
//!   issues no calls at all.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::config::key;
use adguard_core::{cli, zip, Cli, Config, Kind, Setting, Toggle, SETUP};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::certificate::CertificateView;
use crate::root_helper::RootHelperView;
use crate::{abbreviate, toast, worker};

/// Wire a button to the assistant without the closure keeping it alive.
///
/// Every button here is a descendant of `SetupAssistant::view`, which the
/// assistant owns — so a strong `Rc` in the handler would be a reference cycle,
/// and the assistant plus its whole widget tree would outlive the window
/// content it was swapped out of. The same reason `protection.rs` reaches for
/// `Rc::downgrade` on its switch handler.
fn on_click(
    button: &gtk::Button,
    assistant: &Rc<SetupAssistant>,
    action: impl Fn(&Rc<SetupAssistant>) + 'static,
) {
    let weak = Rc::downgrade(assistant);
    button.connect_clicked(move |_| {
        if let Some(assistant) = weak.upgrade() {
            action(&assistant);
        }
    });
}

/// One question, and the answer the user has given it so far.
struct Answer {
    setting: Setting,
    control: Control,
    /// What the *seeded* file said for this key, rendered the same way
    /// [`Control::chosen`] renders the answer.
    ///
    /// The delta is what gets written. `None` means the key could not be read
    /// from the seeded file at all, which is the one case where a write is
    /// issued unconditionally: there is nothing to compare against, and leaving
    /// a question the user answered unwritten would be worse than a redundant
    /// `config set`.
    ///
    /// Fixed once, when the page is built from the seeded file. Nothing
    /// re-reads it: the assistant runs before [`crate::watch`] is installed, so
    /// there is no external reconcile to race with, which is why this needs
    /// none of the `painted`/`pending` machinery the Advanced page carries.
    seeded: Option<String>,
}

impl Answer {
    /// The `config set` value this row would write, or `None` if the user has
    /// left it where the seed put it.
    fn delta(&self) -> Option<String> {
        let chosen = self.control.chosen();
        match self.seeded.as_deref() {
            Some(seeded) if seeded == chosen => None,
            _ => Some(chosen),
        }
    }
}

enum Control {
    Switch(adw::SwitchRow),
    Number(adw::SpinRow),
}

impl Control {
    /// The answer, spelled exactly as `config set` must receive it.
    ///
    /// Booleans are lowercase `true`/`false` and nothing else: the CLI also
    /// accepts `1`/`0`, but that writes a literal integer where a bool belongs
    /// (contract §5).
    fn chosen(&self) -> String {
        match self {
            Self::Switch(row) => if row.is_active() { "true" } else { "false" }.to_owned(),
            Self::Number(row) => (row.value() as i64).to_string(),
        }
    }

    fn set(&self, value: &str) {
        match self {
            Self::Switch(row) => row.set_active(value == "true"),
            Self::Number(row) => {
                if let Ok(number) = value.parse::<i64>() {
                    row.set_value(number as f64);
                }
            }
        }
    }

    fn widget(&self) -> &adw::PreferencesRow {
        match self {
            Self::Switch(row) => row.upcast_ref(),
            Self::Number(row) => row.upcast_ref(),
        }
    }
}

pub struct SetupAssistant {
    view: adw::NavigationView,
    cli: Cli,
    toasts: adw::ToastOverlay,
    answers: RefCell<Vec<Rc<Answer>>>,
    /// Called once, when the user leaves the assistant for the app proper.
    finished: RefCell<Option<Box<dyn Fn()>>>,
}

impl SetupAssistant {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            view: adw::NavigationView::new(),
            cli,
            toasts,
            answers: RefCell::new(Vec::new()),
            finished: RefCell::new(None),
        });
        this.show_welcome();
        this
    }

    pub fn widget(&self) -> &adw::NavigationView {
        &self.view
    }

    /// Called when setup is done — or deliberately skipped — and the main UI
    /// should take over the window.
    pub fn connect_finished(&self, callback: impl Fn() + 'static) {
        self.finished.replace(Some(Box::new(callback)));
    }

    /// Hand the window over to the main UI.
    ///
    /// Deferred to the next main-loop iteration rather than run inline. Every
    /// caller is a button *inside* the widget tree this hands away, and the
    /// callback's first act is to replace that tree — so running it here would
    /// be destroying the widget whose signal handler is still on the stack.
    fn finish(self: &Rc<Self>) {
        let this = self.clone();
        glib::idle_add_local_once(move || {
            if let Some(callback) = this.finished.borrow().as_ref() {
                callback();
            }
        });
    }

    // --- page 1: welcome, and the licence gate -----------------------------

    fn show_welcome(self: &Rc<Self>) {
        let status = adw::StatusPage::builder()
            .icon_name("preferences-system-symbolic")
            .title("Set up AdGuard")
            .description("Checking the licence…")
            .build();

        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        status.set_child(Some(&spinner));

        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);

        // `configure` is licence-gated — measured, it exits 1 with AdGuard's
        // own complaint on stderr — so there is no point offering a button that
        // cannot work. Reading the licence first turns that into a sentence the
        // user can act on instead of a failure half way through setup.
        let this = self.clone();
        let cli = self.cli.clone();
        worker::run(
            move || cli.license().map(|_| ()).map_err(|err| match err {
                cli::Error::Unlicensed { message } => Some(message),
                // Anything else — the initialisation race, a missing binary —
                // is not a licence answer, and pretending it is would send the
                // user off to activate something that is already active.
                _ => None,
            }),
            move |result| this.welcome_settled(result),
        );
    }

    /// `Ok` — licensed, setup can proceed. `Err(Some(complaint))` — AdGuard says
    /// the licence is not active, in its own words. `Err(None)` — the licence
    /// could not be read for some *other* reason, which is not the same fact and
    /// must not be reported as one.
    fn welcome_settled(self: &Rc<Self>, result: Result<(), Option<String>>) {
        let where_ = self
            .cli
            .config_path()
            .map(|path| abbreviate(&path))
            .unwrap_or_else(|| "AdGuard's data directory".to_owned());

        let (icon, title, description, primary) = match &result {
            Ok(()) => (
                "preferences-system-symbolic",
                "Set up AdGuard",
                format!(
                    "AdGuard CLI has not been configured on this machine yet — there is no \
                     {where_}. Creating it writes AdGuard's own defaults, exactly as \
                     `adguard-cli configure` would; you can then change the few settings \
                     worth deciding up front, and everything else afterwards from the \
                     sidebar."
                ),
                Some(("Create the configuration", true)),
            ),
            Err(Some(complaint)) => (
                "dialog-warning-symbolic",
                "Activate AdGuard first",
                format!(
                    "{complaint}\n\nSetting up needs an active licence — AdGuard refuses \
                     `configure` without one. Activation lives on the Status page."
                ),
                Some(("Continue to the app", false)),
            ),
            Err(None) => (
                "dialog-warning-symbolic",
                "Could not read the licence",
                "The licence state could not be read, which is not the same as the licence \
                 being inactive — so nothing is assumed about it here. Try again, or carry \
                 on to the app and set AdGuard up later."
                    .to_owned(),
                Some(("Continue to the app", false)),
            ),
        };

        let status = adw::StatusPage::builder()
            .icon_name(icon)
            .title(title)
            .description(description)
            .build();

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();

        if let Some((label, is_seed)) = primary {
            let button = gtk::Button::with_label(label);
            button.add_css_class("pill");
            button.add_css_class("suggested-action");
            on_click(&button, self, move |this| {
                if is_seed {
                    this.seed();
                } else {
                    this.finish();
                }
            });
            buttons.append(&button);
        }

        if result.is_err() {
            let again = gtk::Button::with_label("Check again");
            again.add_css_class("pill");
            on_click(&again, self, |this| this.show_welcome());
            buttons.append(&again);
        }

        // Offered in **every** branch, including the two that refuse to set up.
        // `import-settings` is not licence-gated where `configure` is (contract
        // §13), so a restore is reachable by exactly the user this screen
        // otherwise turns away: someone rebuilding a machine who has their
        // backup and has not activated yet. `architecture.md` §5 — leaving it
        // behind the licence check would have the app refuse to do something
        // the CLI would have done.
        let restore = gtk::Button::with_label("Restore from a backup");
        restore.add_css_class("pill");
        on_click(&restore, self, |this| this.choose_backup());
        buttons.append(&restore);

        status.set_child(Some(&buttons));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);
    }

    // --- restore, the second path through first run ------------------------

    /// Pick a backup, **read its manifest**, confirm, import.
    ///
    /// The manifest check is not optional: the two exports share one filename
    /// and `import-settings` takes a *logs* zip at exit 0, leaving a partial
    /// install (contract §13). Here that matters more than on the Advanced
    /// page, because there is no configuration yet to fall back to.
    fn choose_backup(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Backup (zip)"));
        filter.add_pattern("*.zip");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Choose a backup")
            .filters(&filters)
            .build();

        let this = self.clone();
        let parent = self
            .view
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok());
        dialog.open(parent.as_ref(), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            let Some(path) = file.path() else {
                this.toasts.add_toast(toast("That file is not on this machine"));
                return;
            };
            match zip::entries(&path).map(|names| zip::classify(&names)) {
                Ok(zip::Bundle::Settings) => this.restore(path),
                Ok(zip::Bundle::Logs) => this.toasts.add_toast(toast(
                    "That is a logs bundle, not a settings backup. AdGuard would accept it \
                     and leave this install half-configured.",
                )),
                Ok(zip::Bundle::Neither) => {
                    this.toasts.add_toast(toast("That zip was not made by AdGuard"));
                }
                Err(err) => this.toasts.add_toast(toast(&err.to_string())),
            }
        });
    }

    /// Run the import, then say what the backup could not carry.
    ///
    /// **No confirmation dialog here, unlike the Advanced page's restore.**
    /// There is nothing to overwrite: this screen only exists because
    /// `proxy.yaml` is absent, so the destructive-action discipline §5 applies
    /// to the other entry point has nothing to warn about at this one.
    fn restore(self: &Rc<Self>, path: PathBuf) {
        let status = adw::StatusPage::builder()
            .icon_name("document-save-symbolic")
            .title("Restoring your settings")
            .description("Reading the backup…")
            .build();
        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        status.set_child(Some(&spinner));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);

        let this = self.clone();
        let cli = self.cli.clone();
        worker::run(
            move || cli.import_settings(&path),
            move |result: Result<(), cli::Error>| match result {
                Ok(()) => this.show_restored(),
                Err(err) => {
                    this.toasts.add_toast(toast(&err.to_string()));
                    this.show_welcome();
                }
            },
        );
    }

    /// The end of a restore, which is **not** where a completed `configure`
    /// ends.
    ///
    /// A seeded install is ready and the window is handed to the pages
    /// silently. A restored one is not: it is unlicensed, it has no
    /// certificate, and the `proxy.yaml` it just wrote says HTTPS filtering is
    /// on (contract §13). Both are states this application already renders and
    /// neither is the user's mistake, so this names them rather than letting
    /// the user meet them as breakage — §6's detect-and-instruct pattern
    /// pointed at a state this app has just created.
    fn show_restored(self: &Rc<Self>) {
        let status = adw::StatusPage::builder()
            .icon_name("emblem-ok-symbolic")
            .title("Settings restored")
            .description(
                "Your settings are back. Two things a backup cannot carry, both of which \
                 this machine still needs:\n\n\
                 • Your licence. AdGuard exports never contain it — activation is on the \
                 Status page.\n\
                 • The HTTPS certificate. Your settings say HTTPS filtering is on, and it \
                 cannot work until the certificate is generated and trusted; Protection \
                 shows you how.\n\n\
                 Your DNS filter choices and DNS user rules are not in a backup either, so \
                 those are as this machine left them.",
            )
            .build();

        let button = gtk::Button::with_label("Continue to the app");
        button.add_css_class("pill");
        button.add_css_class("suggested-action");
        on_click(&button, self, |this| this.finish());
        status.set_child(Some(&button));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);
    }

    // --- the seed ----------------------------------------------------------

    /// Create `proxy.yaml`, then read it back.
    ///
    /// The one call in this application that runs `configure`. It is guarded
    /// inside [`Cli::configure`] rather than here, because a guard beside the
    /// spawn cannot be skipped by a second call site the way one beside a
    /// button can.
    fn seed(self: &Rc<Self>) {
        let status = adw::StatusPage::builder()
            .icon_name("preferences-system-symbolic")
            .title("Creating the configuration")
            .description("Writing AdGuard's defaults…")
            .build();
        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        status.set_child(Some(&spinner));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                // act -> re-read, as everywhere else. `configure` proves itself
                // by the file existing afterwards, but the *values* in it are
                // what the next page renders, so they are read here rather than
                // inferred from the defaults the CLI printed.
                let seeded = cli.configure().map_err(|err| err.to_string());
                (seeded, Config::load().ok())
            },
            move |(seeded, config)| match (seeded, config) {
                (Ok(()), Some(config)) => this.show_choices(&config),
                // Seeded, but the file cannot be read back. Nothing can be
                // pre-filled honestly, so hand over rather than guess.
                (Ok(()), None) => {
                    this.toasts.add_toast(toast(
                        "Created the configuration, but could not read proxy.yaml back",
                    ));
                    this.finish();
                }
                (Err(message), _) => this.show_failure(&message),
            },
        );
    }

    fn show_failure(self: &Rc<Self>, message: &str) {
        let status = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .title("Could not create the configuration")
            .description(message)
            .build();

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();

        let again = gtk::Button::with_label("Try again");
        again.add_css_class("pill");
        on_click(&again, self, |this| this.show_welcome());
        buttons.append(&again);

        let onward = gtk::Button::with_label("Continue to the app");
        onward.add_css_class("pill");
        on_click(&onward, self, |this| this.finish());
        buttons.append(&onward);

        status.set_child(Some(&buttons));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);
    }

    // --- page 2: the questions ---------------------------------------------

    /// The certificate-trust rows for the questions page.
    ///
    /// This screen is where the state they report is *created*: `configure`
    /// generates the CA and then skips its own *"Do you want to install the
    /// certificate on the system?"* prompt in silence, since that one needs a
    /// password and there is no TTY (contract §7). So the assistant is not
    /// merely a good place to mention the certificate — it is the first moment
    /// at which there is one to install, and the HTTPS row above has only ever
    /// been able to say the certificate is *needed*.
    ///
    /// Painted once, from the seeded file, and not held afterwards. The switch
    /// above can still be turned off before *Apply*, which would make these
    /// rows moot — but the pages this hands over to carry the same group, and
    /// repainting a wizard step from a control the user has not committed yet
    /// would flicker between two answers while they are still deciding.
    fn certificate(&self, config: &Config) -> Rc<CertificateView> {
        let view = CertificateView::new(&self.toasts);
        view.paint(
            config.toggle(Toggle::HttpsFiltering),
            config.certificate_name(),
        );
        view
    }

    fn show_choices(self: &Rc<Self>, config: &Config) {
        self.answers.borrow_mut().clear();

        let page_ = adw::PreferencesPage::new();

        let intro = adw::PreferencesGroup::builder()
            .title("AdGuard is configured")
            .description(format!(
                "{} now holds AdGuard's defaults. These are the few worth deciding \
                 before you start it; everything else is on the pages behind this one.",
                abbreviate(config.path())
            ))
            .build();
        page_.add(&intro);

        for group in &SETUP {
            let rendered = adw::PreferencesGroup::builder()
                .title(group.title)
                .description(group.description)
                .build();
            for setting in group.settings {
                rendered.add(self.answer(*setting, config).control.widget());
            }
            page_.add(&rendered);

            // Immediately under the question it qualifies, keyed off the
            // setting rather than off the group's position — the same way the
            // Advanced page keys its root-helper rows off `proxy_mode`, so a
            // reordered `SETUP` table takes these rows with it.
            if group
                .settings
                .iter()
                .any(|setting| setting.key == key::HTTPS_FILTERING)
            {
                page_.add(self.certificate(config).widget());
            }
        }

        // Not a question, and not deferrable either: AdGuard ships its helper
        // unmet, and until it is set up the HTTP proxy this assistant is about
        // to start answers every request with an error (contract §8). So every
        // install this screen completes ends in that state — the same thing
        // §6 says about the certificate, and the same reason this screen is
        // where it belongs rather than a page the user may never open.
        //
        // Below the questions rather than above them: it is an errand to run
        // outside this window, and putting a `sudo` command before the settings
        // would read as a demand to satisfy before continuing. Painted once,
        // like the certificate group, and not held afterwards.
        let helper = RootHelperView::new(&self.toasts);
        helper.paint();
        page_.add(helper.widget());

        // The two questions the CLI's wizard asks that this page deliberately
        // does not, said out loud rather than silently dropped. `listen_address`
        // in particular is not an omission of taste: moving beyond loopback with
        // `listen_auth` off is a measured silent no-op (contract §5), and the
        // seed always leaves auth off with empty credentials.
        let elsewhere = adw::PreferencesGroup::builder()
            .title("Left for later, on purpose")
            .description(
                "The listen address stays on this machine for now — exposing the proxy to \
                 your network needs a proxy username and password first, which the \
                 Advanced page asks for. Automatic proxy mode is on the Advanced page too, \
                 and needs the root helper above. Filter lists are the Filters page.",
            )
            .build();
        page_.add(&elsewhere);

        let apply = gtk::Button::with_label("Apply and continue");
        apply.add_css_class("pill");
        apply.add_css_class("suggested-action");
        apply.set_halign(gtk::Align::Center);
        on_click(&apply, self, |this| this.apply());

        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(12)
            .build();
        bottom.append(&apply);

        // No way back: the previous page's only action was the seed, and the
        // seed cannot be run twice. A back button that could only fail is worse
        // than no back button.
        let navigation = page("Setup", &wrap(&page_, Some(&bottom)));
        navigation.set_can_pop(false);
        self.view.replace(&[navigation]);
    }

    fn answer(self: &Rc<Self>, setting: Setting, config: &Config) -> Rc<Answer> {
        let control = match setting.kind {
            Kind::Switch => Control::Switch(adw::SwitchRow::new()),
            Kind::Number { min, max, .. } => {
                Control::Number(adw::SpinRow::with_range(min as f64, max as f64, 1.0))
            }
            // The table is fixed and holds neither, but a silent wrong control
            // would be worse than a compile-time reminder if that ever changes.
            Kind::Text { .. } | Kind::Choice { .. } => Control::Switch(adw::SwitchRow::new()),
        };

        let row = control.widget();
        // `AdwPreferencesRow:use-markup` defaults to true and the label is
        // rendered as the title is assigned, so this goes first — the same
        // ordering every other page in this app uses.
        row.set_use_markup(false);
        row.set_title(setting.title);
        match &control {
            Control::Switch(switch) => {
                switch.set_subtitle(setting.description);
                switch.set_subtitle_lines(2);
            }
            Control::Number(number) => {
                number.set_subtitle(setting.description);
                number.set_subtitle_lines(2);
            }
        }

        let seeded = reading(config, setting);
        if let Some(value) = &seeded {
            control.set(value);
        }

        let answer = Rc::new(Answer {
            setting,
            control,
            seeded,
        });
        self.answers.borrow_mut().push(answer.clone());
        answer
    }

    // --- applying ----------------------------------------------------------

    /// Write the settings the user moved, then read the file to find out what
    /// actually landed.
    fn apply(self: &Rc<Self>) {
        let writes: Vec<(Setting, String)> = self
            .answers
            .borrow()
            .iter()
            .filter_map(|answer| answer.delta().map(|value| (answer.setting, value)))
            .collect();

        // Nothing was moved, so there is nothing to verify and nothing to
        // report. The seeded defaults are the answer.
        if writes.is_empty() {
            self.finish();
            return;
        }

        let status = adw::StatusPage::builder()
            .icon_name("preferences-system-symbolic")
            .title("Applying")
            .description("Writing your answers…")
            .build();
        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        status.set_child(Some(&spinner));
        self.view
            .replace(&[page("Setup", &wrap(&status, None::<&gtk::Widget>))]);

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                // One call per setting, sequentially on this one worker.
                // Deliberately not concurrent: two `adguard-cli` invocations at
                // once against the same data directory can lose a race with
                // each other's initialisation (contract §3), and there is
                // nothing to gain — each call costs 10–30 ms.
                let mut refused = Vec::new();
                for (setting, value) in &writes {
                    if let Err(err) = cli.config_set(setting.key, value) {
                        refused.push((*setting, err.to_string()));
                    }
                }
                (writes, refused, Config::load().ok())
            },
            move |(writes, refused, config)| this.show_summary(&writes, &refused, config.as_ref()),
        );
    }

    // --- page 3: what actually happened ------------------------------------

    fn show_summary(
        self: &Rc<Self>,
        writes: &[(Setting, String)],
        refused: &[(Setting, String)],
        config: Option<&Config>,
    ) {
        let page_ = adw::PreferencesGroup::builder().title("Your answers").build();

        let mut all_landed = true;
        for (setting, requested) in writes {
            // The file decides, not the CLI. `Config has been updated` is
            // printed for a no-op and for a change the CLI silently declined,
            // so the only evidence that counts is the value in `proxy.yaml`.
            let landed = config.and_then(|config| reading(config, *setting));
            let complaint = refused
                .iter()
                .find(|(other, _)| other.key == setting.key)
                .map(|(_, message)| message.clone());

            let (icon, detail) = match (&landed, complaint) {
                (Some(landed), _) if landed == requested => {
                    ("object-select-symbolic", format!("Set to {landed}"))
                }
                // The CLI explained itself, and its wording beats ours.
                (_, Some(message)) => {
                    all_landed = false;
                    ("dialog-warning-symbolic", message)
                }
                (Some(landed), None) => {
                    all_landed = false;
                    ("dialog-warning-symbolic", format!(
                        "Asked for {requested}, but the file says {landed}"
                    ))
                }
                (None, None) => {
                    all_landed = false;
                    (
                        "dialog-warning-symbolic",
                        format!("Asked for {requested}, but the file could not be read back"),
                    )
                }
            };

            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            row.set_title(setting.title);
            row.set_subtitle(&detail);
            row.set_subtitle_lines(2);
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            page_.add(&row);
        }

        let status = adw::StatusPage::builder()
            .icon_name(if all_landed {
                "emblem-ok-symbolic"
            } else {
                "dialog-warning-symbolic"
            })
            .title(if all_landed {
                "AdGuard is ready"
            } else {
                "Set up, with exceptions"
            })
            .description(if all_landed {
                "Start the proxy from the Status page whenever you are ready."
            } else {
                "Everything below that did not land can be changed from the sidebar."
            })
            .build();

        let content = adw::PreferencesPage::new();
        content.add(&page_);

        let onward = gtk::Button::with_label("Open AdGuard UI");
        onward.add_css_class("pill");
        onward.add_css_class("suggested-action");
        onward.set_halign(gtk::Align::Center);
        on_click(&onward, self, |this| this.finish());

        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(12)
            .build();
        bottom.append(&onward);

        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        box_.append(&status);
        box_.append(&content);

        let navigation = page("Setup", &wrap(&box_, Some(&bottom)));
        navigation.set_can_pop(false);
        self.view.replace(&[navigation]);
    }
}

/// Read one setting out of the config, rendered the way `config set` takes it.
///
/// `None` is "could not be read", never "off" — the same distinction every
/// other page in this app keeps.
fn reading(config: &Config, setting: Setting) -> Option<String> {
    match setting.kind {
        Kind::Switch => config
            .bool_at(setting.key)
            .map(|on| if on { "true" } else { "false" }.to_owned()),
        Kind::Number { .. } => config.int_at(setting.key).map(|value| value.to_string()),
        Kind::Text { .. } => config.str_at(setting.key).map(str::to_owned),
        Kind::Choice { options } => config
            .choice_at(setting.key, options)
            .map(str::to_owned),
    }
}

/// A scrolling body with a header bar, and optionally a bottom action bar.
fn wrap(content: &impl IsA<gtk::Widget>, bottom: Option<&impl IsA<gtk::Widget>>) -> adw::ToolbarView {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(content)
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    if let Some(bottom) = bottom {
        view.add_bottom_bar(bottom);
    }
    view
}

fn page(title: &str, content: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    adw::NavigationPage::new(content, title)
}
