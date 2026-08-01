//! The rows that report AdGuard's root-helper check and carry AdGuard's own
//! command for setting it up (`docs/architecture.md` §6).
//!
//! These began as `AdvancedPage::build_helper_view` and moved here when the
//! helper turned out to matter on a second screen, which is the same journey
//! [`crate::certificate`] made and for the same reason. That module's header
//! says its shape is the root helper's; this is now literally so.
//!
//! **What put it on a second screen.** The check reads as an automatic-mode
//! prerequisite — AdGuard's own strings say so — and for as long as that was
//! the whole story, the Advanced page beside the mode row was the only place it
//! belonged. It is not the whole story: with the helper unmet, `manual` mode's
//! HTTP proxy answers every request with 502 and never opens an upstream
//! connection (contract §8). The helper ships unmet, so **every install this
//! application sets up ends with an HTTP proxy that cannot serve a request** —
//! which is precisely what §6 already says about the certificate, and it makes
//! the first-run assistant the screen where the state is met rather than a
//! screen that may mention it.
//!
//! [`adguard_core::RootHelper`] does the looking; everything here is wording.
//!
//! **Nothing in this file runs anything.** There is a copy button and no other
//! affordance. The helper lives in a user-writable directory, so suid-root on
//! it makes anyone who can write that file root — AdGuard's design, accepted by
//! installing AdGuard, and the deliberateness of typing `sudo` at a prompt is
//! the only safeguard the arrangement has.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::RootHelper;
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::toast;

/// A group of two rows: what the check found, and what to run about it.
///
/// Hidden entirely once the check passes. A requirement that is met is not
/// worth a standing row, and leaving one would invite a user to run a `sudo`
/// command they no longer need.
pub struct RootHelperView {
    group: adw::PreferencesGroup,
    /// What the check found, in AdGuard's own three-property wording.
    status: adw::ActionRow,
    /// AdGuard's own command, with a copy button. Never run from here.
    command: adw::ActionRow,
    /// The last reading rendered, so a re-check that found nothing new does not
    /// rebuild the rows under the user's pointer.
    painted: RefCell<Option<String>>,
    /// Where the check reads from, resolved once at construction.
    ///
    /// `$ADGUARD_ROOT_HELPER` overrides it. That override is what keeps both
    /// branches reachable: this machine's helper was shipped unmet, has since
    /// been set up by hand, and the rendering that is unreachable locally is
    /// now the one this view exists to show.
    path: Option<PathBuf>,
}

impl RootHelperView {
    pub fn new(toasts: &adw::ToastOverlay) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("AdGuard's root helper")
            .build();

        let status = adw::ActionRow::new();
        status.set_use_markup(false);
        status.set_title("Root helper");
        status.set_subtitle_lines(3);
        status.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        group.add(&status);

        let command = adw::ActionRow::new();
        command.set_use_markup(false);
        command.set_title("Run this in a terminal");
        command.set_subtitle_lines(3);
        // `.monospace` on the subtitle would need a stylesheet rule; the
        // command is the subtitle so it stays a plain label and the copy button
        // is the thing that makes it usable.
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
            path: std::env::var_os("ADGUARD_ROOT_HELPER")
                .map(PathBuf::from)
                .or_else(adguard_core::paths::root_helper),
        })
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Re-read the check and render it into the two rows.
    ///
    /// Cheap enough to call from a focus handler — one `stat` — and re-read
    /// rather than cached, because the moment the caller cares about is
    /// precisely the moment the answer has changed.
    pub fn paint(&self) {
        let check = self.path.as_ref().map(RootHelper::inspect);
        let snapshot = format!("{check:?}");
        if self.painted.borrow().as_deref() == Some(snapshot.as_str()) {
            return;
        }
        self.painted.replace(Some(snapshot));

        match check {
            Some(Ok(helper)) if helper.is_set_up() => {
                self.group.set_visible(false);
            }
            Some(Ok(helper)) => {
                self.group.set_visible(true);
                self.command.set_visible(true);
                self.status.set_subtitle(&format!(
                    "Missing {}. Checked {}.",
                    join_with_and(&helper.unmet()),
                    helper.path.display()
                ));
                self.command.set_subtitle(&helper.setup_command());
                self.group.set_description(Some(
                    "Until this is set up the HTTP proxy answers every request with an \
                     error, in any proxy mode, and automatic mode does nothing at all. \
                     The setuid bit lets any user on this machine run the helper as \
                     root. AdGuard's helper lives in a directory you can write to, so \
                     anyone who can replace that file would gain root with it — which \
                     is why this application shows the command rather than running it \
                     for you.",
                ));
            }
            // Installed, but the helper could not be read. Different from "not
            // set up", and the command would be a guess.
            Some(Err(err)) => {
                self.group.set_visible(true);
                self.command.set_visible(false);
                self.status
                    .set_subtitle(&format!("Could not be read — {err}"));
                self.group.set_description(Some(
                    "The HTTP proxy and automatic mode both need AdGuard's root \
                     helper, and this check could not read it.",
                ));
            }
            // The CLI itself could not be located, which the window already
            // says elsewhere. Nothing useful to add here.
            None => self.group.set_visible(false),
        }
    }
}

/// Join the missing properties into something readable: "owned by root and the
/// setuid bit set", rather than a debug-printed list.
///
/// The three facts are reported separately on purpose (`architecture.md` §6),
/// and a user who has already run AdGuard's command needs to read which one is
/// still missing without decoding punctuation.
///
/// Lives here rather than on the Advanced page because the wording it serves
/// does: that page still calls it for the toast that refuses `auto`.
pub fn join_with_and(parts: &[&str]) -> String {
    match parts {
        [] => "nothing".to_owned(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::join_with_and;

    #[test]
    fn joins_the_way_the_cli_lists_them() {
        assert_eq!(join_with_and(&[]), "nothing");
        assert_eq!(join_with_and(&["owned by root"]), "owned by root");
        assert_eq!(
            join_with_and(&["owned by root", "the setuid bit set"]),
            "owned by root and the setuid bit set"
        );
        assert_eq!(
            join_with_and(&[
                "owned by root",
                "the setuid bit set",
                "the executable bit set"
            ]),
            "owned by root, the setuid bit set and the executable bit set"
        );
    }
}
