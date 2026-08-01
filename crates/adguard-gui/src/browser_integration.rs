//! The rows that report whether AdGuard's browser extension can reach the CLI,
//! and carry AdGuard's own command for making it able to.
//!
//! The third of these, after [`crate::root_helper`] and [`crate::certificate`],
//! and the shape is theirs: a step this application will not take, an upstream
//! command that takes it, and a check that has to render the unmet state rather
//! than merely prevent it.
//!
//! **What makes this one worth a row is that nothing else reports it honestly.**
//! With no native-messaging manifest the extension says it cannot detect
//! `adguard-cli`, which sends the user looking at their AdGuard install — at the
//! binary, at `$PATH`, at whether the proxy is running — when all of that may be
//! perfectly fine and the missing thing is a 500-byte JSON file the extension
//! never names. `install-browser-integration` is not part of unpacking the CLI,
//! so **every stock install is in this state**, and the command reports success
//! even when it wrote nothing at all (see [`adguard_core::browser`]).
//!
//! [`adguard_core::BrowserIntegration`] does the looking; everything here is
//! wording.
//!
//! **Nothing in this file runs anything.** There is a copy button and no other
//! affordance, as with the `sudo` commands elsewhere in this app — though the
//! reason is not privilege this time, since this command needs none. It is that
//! the command writes into five other applications' configuration directories,
//! and which browsers on this machine should be given a native-messaging host
//! is the user's decision rather than ours to make on their behalf.

use std::cell::RefCell;
use std::rc::Rc;

use adguard_core::browser::{BrowserIntegration, State};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::root_helper::join_with_and;
use crate::{abbreviate, toast};

/// A group of two rows: what the check found, and what to run about it.
///
/// Hidden entirely when every browser on this machine is set up, and when there
/// is no browser AdGuard knows about. A standing row on a machine with nothing
/// to fix would invite a command that does nothing.
pub struct BrowserIntegrationView {
    group: adw::PreferencesGroup,
    status: adw::ActionRow,
    command: adw::ActionRow,
    /// The last reading rendered, so a re-check that found nothing new does not
    /// rebuild rows under the user's pointer.
    painted: RefCell<Option<String>>,
}

impl BrowserIntegrationView {
    pub fn new(toasts: &adw::ToastOverlay) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("AdGuard's browser extension")
            .build();

        let status = adw::ActionRow::new();
        status.set_use_markup(false);
        status.set_title("Browser integration");
        status.set_subtitle_lines(5);
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
        // Weak, both of them, for the reason `certificate.rs` sets out: the row
        // owns the button which owns this closure, so a strong `command` closes
        // a GObject cycle nothing breaks, and a strong `toasts` would keep the
        // whole widget tree alive from a leaked row.
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

        Rc::new(Self {
            group,
            status,
            command,
            painted: RefCell::new(None),
        })
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Re-read the check and render it.
    ///
    /// Cheap enough for a focus handler: five or six `stat`s and, on a machine
    /// that has run the command, that many ~500-byte reads. Re-read rather than
    /// cached for the reason the other two checks are — the user's way out is a
    /// command they run elsewhere, so the moment they come back is the moment
    /// the answer has changed. Here that cuts a second way: installing a
    /// *browser* changes it too, and this is the only check in the app whose
    /// answer can be invalidated by something that has nothing to do with
    /// AdGuard.
    pub fn paint(&self) {
        let check = BrowserIntegration::detect();

        // The command goes into the snapshot as well as the check, exactly as
        // on the certificate rows: it looks up the CLI binary, which can appear
        // or vanish while the window is open, and a snapshot of the check alone
        // would suppress the repaint that mattered.
        let command = check.as_ref().and_then(BrowserIntegration::install_command);
        let snapshot = format!("{check:?} {command:?}");
        if self.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return;
        }
        self.painted.replace(Some(snapshot));

        // `$HOME` unset. None of the six paths can be formed, so there is no
        // check to report rather than a check that passed.
        let Some(check) = check else {
            self.group.set_visible(false);
            return;
        };

        let unmet = check.unmet();
        if unmet.is_empty() {
            self.group.set_visible(false);
            return;
        }

        self.group.set_visible(true);
        self.status.set_subtitle(&explain(&unmet));

        // The host binary is checked before the command is offered, because
        // running the installer without it would write six manifests naming a
        // program that is not there — replacing "the extension cannot find
        // AdGuard" with a browser that launches nothing, which is harder to
        // diagnose and looks like the fix worked.
        match command.filter(|_| check.host_present) {
            Some(command) => {
                self.command.set_visible(true);
                self.command.set_subtitle(&command);
                self.group.set_description(Some(INSTALL));
            }
            None => {
                self.command.set_visible(false);
                self.group.set_description(Some(if check.host_present {
                    NO_CLI
                } else {
                    NO_HOST
                }));
            }
        }
    }
}

/// What the check found, browser by browser, grouped by what is wrong.
///
/// Grouped rather than listed one line per browser because the ordinary case is
/// every browser in the same state, and six sentences saying the same thing
/// about six browsers is a wall a user reads none of. The three states get
/// their own clauses because they are three different facts, and only the first
/// of them means "you have not run the command".
fn explain(unmet: &[&adguard_core::browser::Browser]) -> String {
    let names = |wanted: fn(&State) -> bool| -> Vec<&'static str> {
        unmet
            .iter()
            .filter(|browser| wanted(&browser.state))
            .map(|browser| browser.name)
            .collect()
    };

    let mut clauses: Vec<String> = Vec::new();

    let missing = names(|state| matches!(state, State::Missing));
    if !missing.is_empty() {
        // Name the directory that was looked in, so the row can be checked
        // rather than believed — one of them, because they differ only in the
        // browser's own name and six paths would bury the sentence.
        let where_ = unmet
            .iter()
            .find(|browser| browser.state == State::Missing)
            .map(|browser| abbreviate(&browser.manifest))
            .unwrap_or_default();
        clauses.push(format!(
            "{} {} no native-messaging manifest for AdGuard, so the extension there \
             cannot reach it. Looked for {where_}.",
            join_with_and(&missing),
            were(missing.len())
        ));
    }

    for browser in unmet {
        match &browser.state {
            State::Stale(named) => clauses.push(format!(
                "{}'s manifest names {}, which is not the AdGuard installed here — \
                 an install that has moved, or an older one left behind.",
                browser.name,
                abbreviate(named)
            )),
            State::Unreadable(err) => clauses.push(format!(
                "{}'s manifest could not be read — {err}.",
                browser.name
            )),
            State::Missing | State::Ready => {}
        }
    }

    clauses.join(" ")
}

/// "has" for one browser, "have" for several. The clause reads as a sentence
/// either way, which matters because one-browser machines are the common case
/// and "Google Chrome have no manifest" is the kind of thing that makes a user
/// distrust everything else on the page.
fn were(count: usize) -> &'static str {
    if count == 1 {
        "has"
    } else {
        "have"
    }
}

/// The ordinary case: the command has not been run, or a browser has been
/// installed since it was.
///
/// The last sentence is the other two views', and the reason is spelled out
/// because it is *not* the reason there — no password is involved here. What
/// is involved is writing into other applications' configuration.
const INSTALL: &str = "AdGuard's browser extension does not look for adguard-cli on your system \
                       path. It asks the browser for a native-messaging host, and the browser \
                       looks that up in a file AdGuard installs separately — so without it the \
                       extension reports that it cannot detect AdGuard even though AdGuard is \
                       installed and running. The command below is AdGuard's own; it writes that \
                       file for each browser it finds, and it needs no password. It writes into \
                       your browsers' configuration directories, so this application does not run \
                       it for you.";

/// The state is real and the command cannot be named, because the CLI could
/// not be located or its path cannot be quoted safely.
const NO_CLI: &str = "AdGuard's browser extension cannot reach adguard-cli through this browser, \
                      and the command that would fix it cannot be shown — the adguard-cli binary \
                      could not be located on this machine, or its path contains characters that \
                      cannot be written into a shell command safely.";

/// The manifests would point at a program that is not there. Worse than the
/// state the user is already in, so the command is withheld.
const NO_HOST: &str = "AdGuard's browser extension cannot reach adguard-cli through this browser, \
                       and running AdGuard's installer would not help: adguard_cli_nm, the \
                       program the browser would launch, is not beside the adguard-cli binary on \
                       this machine. Installing the manifests now would point every browser at a \
                       program that is not there. Reinstalling AdGuard CLI restores it.";

#[cfg(test)]
mod tests {
    use super::*;
    use adguard_core::browser::Browser;
    use std::path::PathBuf;

    /// Built under the real `$HOME`, because the row abbreviates the path it
    /// shows and a path outside `$HOME` would leave that untested — which is
    /// how the first version of these tests passed while asserting a `~` the
    /// row could never have produced.
    fn browser(name: &'static str, state: State) -> Browser {
        let home = std::env::var("HOME").unwrap_or_default();
        Browser {
            name,
            manifest: PathBuf::from(format!(
                "{home}/.config/google-chrome/NativeMessagingHosts/\
                 com.adguard.browser_extension_host.nm.json"
            )),
            state,
        }
    }

    /// One browser: the sentence has to agree with itself, and it has to name
    /// the file it looked for.
    #[test]
    fn a_single_browser_reads_as_a_sentence() {
        if std::env::var_os("HOME").is_none_or(|home| home.is_empty()) {
            eprintln!("skipping: no $HOME, so there is no abbreviation to assert");
            return;
        }
        let rows = [browser("Google Chrome", State::Missing)];
        let unmet: Vec<_> = rows.iter().collect();
        let text = explain(&unmet);
        assert!(text.starts_with("Google Chrome has no native-messaging manifest"), "{text}");
        assert!(text.contains("Looked for ~/.config/google-chrome/"), "{text}");
    }

    /// Several, joined the way the rest of the app joins things.
    #[test]
    fn several_browsers_are_named_together_once() {
        let rows = [
            browser("Google Chrome", State::Missing),
            browser("Chromium", State::Missing),
            browser("Firefox", State::Missing),
        ];
        let unmet: Vec<_> = rows.iter().collect();
        let text = explain(&unmet);
        assert!(
            text.starts_with("Google Chrome, Chromium and Firefox have no native-messaging"),
            "{text}"
        );
        // One clause, not three.
        assert_eq!(text.matches("native-messaging manifest").count(), 1, "{text}");
    }

    /// A stale manifest is a different fact and must not be folded into the
    /// missing clause — the user has run the command and needs to know why it
    /// did not take.
    #[test]
    fn a_stale_manifest_says_what_it_names() {
        let rows = [browser(
            "Vivaldi",
            State::Stale(PathBuf::from("/opt/old-adguard/adguard_cli_nm")),
        )];
        let unmet: Vec<_> = rows.iter().collect();
        let text = explain(&unmet);
        assert!(
            text.contains("Vivaldi's manifest names /opt/old-adguard/adguard_cli_nm"),
            "{text}"
        );
        assert!(!text.contains("no native-messaging manifest"), "{text}");
    }

    /// Mixed states produce both clauses, in that order.
    #[test]
    fn missing_and_stale_are_reported_separately() {
        let rows = [
            browser("Google Chrome", State::Missing),
            browser("Vivaldi", State::Stale(PathBuf::from("/opt/old/adguard_cli_nm"))),
        ];
        let unmet: Vec<_> = rows.iter().collect();
        let text = explain(&unmet);
        assert!(text.starts_with("Google Chrome has no native-messaging"), "{text}");
        assert!(text.contains("Vivaldi's manifest names"), "{text}");
    }

    /// An unreadable manifest carries the error, and never reads as met.
    #[test]
    fn an_unreadable_manifest_carries_its_reason() {
        let rows = [browser(
            "Brave",
            State::Unreadable(String::from("no \"path\" in it")),
        )];
        let unmet: Vec<_> = rows.iter().collect();
        let text = explain(&unmet);
        assert!(
            text.contains("Brave's manifest could not be read — no \"path\" in it."),
            "{text}"
        );
    }
}
