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
//! [`adguard_core::CaTrust`] does the looking; everything here is wording.
//!
//! **Nothing in this file runs anything.** There is a copy button and no other
//! affordance, exactly as with the `sudo` command on the Advanced page.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::trust::{self, CaTrust};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast};

/// A group of two rows: what the check found, and what to run about it.
///
/// Hidden entirely when the certificate is trusted, or when HTTPS filtering is
/// off and so nothing depends on it. A requirement that is met is not worth a
/// standing row, and a permanent one would invite a user to run a `sudo`
/// command they do not need.
pub struct CertificateView {
    group: adw::PreferencesGroup,
    status: adw::ActionRow,
    command: adw::ActionRow,
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

        let command = adw::ActionRow::new();
        command.set_use_markup(false);
        command.set_title("Run this in a terminal");
        command.set_subtitle_lines(3);
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
        group.add(&command);

        Rc::new(Self {
            group,
            status,
            command,
            painted: RefCell::new(None),
            installer: std::env::var_os("ADGUARD_CERT_INSTALLER")
                .map(PathBuf::from)
                .or_else(adguard_core::paths::cert_installer),
        })
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
    /// — a thirtieth of a frame, and pinned by a test with a 50 ms bound. It is
    /// re-read every time rather than cached, for the reason the helper check
    /// is: the user's way out is a command they run elsewhere, so a cache would
    /// be stale at exactly the moment that matters.
    pub fn paint(&self, filtering: Option<bool>, certificate_name: &str) {
        let check = (filtering != Some(false))
            .then(|| CaTrust::detect(certificate_name))
            .flatten();

        // The installer's own absence belongs in the snapshot: a check that has
        // not moved can still want a different command row than last time.
        let snapshot = format!("{check:?} installer={:?}", self.installer);
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

        if trust.is_trusted() {
            self.group.set_visible(false);
            return;
        }

        self.group.set_visible(true);
        self.status.set_subtitle(&self.explain(&trust));

        match self.remedy(&trust) {
            Some(remedy) => {
                self.command.set_visible(true);
                self.command.set_subtitle(&remedy.command);
                self.group.set_description(Some(remedy.description));
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
                        .is_some_and(|path| !trust::quotable(path));
                self.group
                    .set_description(Some(if unshowable { UNSHOWABLE } else { NO_INSTALLER }));
            }
        }
    }

    /// What the check found, in the order the machine applies it.
    fn explain(&self, trust: &CaTrust) -> String {
        let missing = trust
            .unmet()
            .first()
            .copied()
            .unwrap_or("it is not trusted");

        if !trust.generated {
            return format!(
                "HTTPS filtering is on, but {missing} — nothing is at {}. \
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

        format!("HTTPS filtering is on, but {missing}.{where_}")
    }

    /// The command that moves this machine to the next state, or `None` when
    /// there is nothing honest to name.
    ///
    /// Each carries its own explanation, because they are three different
    /// programs doing three different things and one description covering all
    /// of them would be wrong about two — the group used to say "the command
    /// below is AdGuard's own installer" over `adguard-cli cert`, which
    /// generates rather than installs.
    fn remedy(&self, trust: &CaTrust) -> Option<Remedy> {
        // Installed already, just not in the bundle: the step AdGuard's script
        // takes after copying the file, and the only one still outstanding. No
        // installer needed, so this is answered before one is required.
        if trust.generated && trust.anchored && !trust.bundled {
            return Some(Remedy {
                command: trust::refresh_command(),
                description: REBUILD,
            });
        }

        if !trust.generated {
            // A different program: `install_cert.sh` installs a certificate, it
            // does not make one. AdGuard's own help for this is `cert`
            // ("Generate a certificate for HTTPS filtering"), which generates
            // and then offers to install in the same run.
            let cli = adguard_core::paths::cli_binary().filter(|path| trust::quotable(path))?;
            return Some(Remedy {
                command: format!("\"{}\" cert", cli.display()),
                description: GENERATE,
            });
        }

        let installer = self.installer.as_ref().filter(|path| path.is_file())?;
        let install = trust::install_command(installer, &trust.certificate)?;
        match (trust.stale, &trust.anchor) {
            // The one state AdGuard's installer cannot repair: it tests whether
            // the anchor path exists and stops if it does, so the old
            // certificate has to go first. One line, because two rows would
            // leave the user holding half a fix — and the `rm` gets the same
            // quoting check as everything else on the line, since this is the
            // one command here that destroys something.
            (true, Some(anchor)) => trust::quotable(anchor).then(|| Remedy {
                command: format!("sudo rm \"{}\" && {install}", anchor.display()),
                description: REPLACE,
            }),
            _ => Some(Remedy {
                command: install,
                description: INSTALL,
            }),
        }
    }
}

/// A command to show, and the sentence that says what it does.
struct Remedy {
    command: String,
    description: &'static str,
}

/// The ordinary case: a certificate that exists and has never been installed.
///
/// The last sentence is the root-helper group's, for the same reason: the step
/// needs a password, and a GUI that collects one to run a shell script as root
/// is a different proposition from a user typing `sudo` at their own prompt
/// (`architecture.md` §6). The browser note is not padding — the system store
/// is the only thing this check can see, and the script does more than the
/// check reports, so saying nothing would leave a user with Firefox still
/// broken and no idea why.
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
                          trust, and the installer for it is here — but this certificate's file \
                          name contains characters that cannot be written into a shell command \
                          safely, such as a quotation mark, a backtick, a dollar sign or a line \
                          break. Rather than show you a command that might not do what it says, \
                          this application shows none. The name comes from \
                          https_filtering.root_certificate_name.";
