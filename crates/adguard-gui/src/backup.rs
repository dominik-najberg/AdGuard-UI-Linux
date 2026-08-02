//! Backup and restore, and the logs bundle — the three zip commands.
//!
//! Buttons rather than settings, so they arrive the way the root-helper view
//! does: built here and added to the Advanced page during its build. The design
//! is `architecture.md` §5, *Import and export, and the first-run collision*.
//!
//! Three measured facts shape every string in this file, and each one forbids
//! something a reasonable person would otherwise write (`cli-contract.md` §13):
//!
//! - **A round trip silently loses DNS filter selections and user rules.**
//!   `agflm_dns.db` and `dns_user.txt` are not exported, while `proxy.yaml` —
//!   which is — carries every `dns_filtering.*` setting. So an export looks
//!   complete and is not, and the confirmation is the only place that can say
//!   so.
//! - **An import does not destroy the licence or the certificate.** Measured on
//!   a licensed install. So the confirmation may **not** warn about losing
//!   them: it would be false.
//! - **A logs bundle discloses `proxy.yaml`** and does *not* contain
//!   `access.log`. Less sensitive than assumed about browsing, more sensitive
//!   than assumed about configuration — and the button says the true half.

use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::zip::{classify, entries, Bundle};
use adguard_core::Cli;
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{toast, worker};

/// What a settings export leaves behind, in the one place a user meets it.
const ROUND_TRIP_LOSS: &str = "The backup holds your settings, your custom rules and your HTTP \
                               filter list. It does not hold your DNS filter choices or your DNS \
                               user rules — those are not in AdGuard's export and will not come \
                               back.";

/// Shown before an import replaces the configuration.
///
/// Says nothing about the licence or the certificate **on purpose**: both
/// survive an import, measured, so a warning about them would be false.
const IMPORT_WARNING: &str = "This replaces your current settings with the ones in the backup. \
                              Your licence and your certificate are not affected. DNS filter \
                              choices and DNS user rules are not in a backup and will be left as \
                              they are.";

pub struct BackupView {
    group: adw::PreferencesGroup,
}

impl BackupView {
    pub fn new(cli: &Cli, toasts: &adw::ToastOverlay) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("Backup and restore")
            .description(ROUND_TRIP_LOSS)
            .build();

        let export = row(
            "Export settings",
            "Choose a folder to write the backup into. It is large — most of it \
             is the filter list, which AdGuard can download again",
            "Export…",
        );
        let import = row(
            "Restore settings",
            "Replaces your settings with a backup made here",
            "Restore…",
        );
        group.add(&export.0);
        group.add(&import.0);

        export.1.connect_clicked({
            let cli = cli.clone();
            let toasts = toasts.clone();
            move |button| choose_folder(button, &cli, &toasts, Kind::Settings)
        });
        import.1.connect_clicked({
            let cli = cli.clone();
            let toasts = toasts.clone();
            move |button| choose_backup(button, &cli, &toasts)
        });

        Rc::new(Self { group })
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }
}

/// The logs button, which belongs in *Diagnostics* beside `log_level` — the
/// setting that decides what ends up in it.
pub fn logs_row(cli: &Cli, toasts: &adw::ToastOverlay) -> adw::ActionRow {
    let (row, button) = row(
        "Export logs",
        // The true half of §13's correction. It is not a browsing history —
        // `access.log` is not in it — but it does carry the configuration.
        "A bundle for a bug report. It includes your configuration file, and \
         does not include the record of sites you visited",
        "Export…",
    );
    button.connect_clicked({
        let cli = cli.clone();
        let toasts = toasts.clone();
        move |button| choose_folder(button, &cli, &toasts, Kind::Logs)
    });
    row
}

#[derive(Clone, Copy)]
enum Kind {
    Settings,
    Logs,
}

fn row(title: &str, subtitle: &str, label: &str) -> (adw::ActionRow, gtk::Button) {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    row.set_use_markup(false);
    row.set_subtitle_lines(4);
    let button = gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));
    (row, button)
}

fn window(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root().and_then(|root| root.downcast::<gtk::Window>().ok())
}

/// Both exports: pick a folder, then let the CLI name the file inside it.
///
/// A folder rather than a filename, deliberately. `-o` writes *into* an
/// existing directory and *as* any other path (contract §13), so offering a
/// filename would mean handing the CLI a path whose meaning depends on whether
/// it happens to exist. Picking a folder makes it always the first case, and
/// the name AdGuard chooses is the one reported back.
fn choose_folder(button: &gtk::Button, cli: &Cli, toasts: &adw::ToastOverlay, kind: Kind) {
    let dialog = gtk::FileDialog::builder().title("Choose a folder").build();
    let cli = cli.clone();
    let toasts = toasts.clone();
    let button = button.clone();
    dialog.select_folder(window(&button).as_ref(), gtk::gio::Cancellable::NONE, move |result| {
        let Ok(folder) = result else {
            return; // Cancelled. Not an error, and not worth a toast.
        };
        let Some(path) = folder.path() else {
            toasts.add_toast(toast("That folder is not on this machine"));
            return;
        };
        button.set_sensitive(false);
        let label = button.label().unwrap_or_default().to_string();
        button.set_label("Working…");

        worker::run(
            move || match kind {
                Kind::Settings => cli.export_settings(&path),
                Kind::Logs => cli.export_logs(&path),
            },
            move |result: Result<PathBuf, adguard_core::Error>| {
                button.set_sensitive(true);
                button.set_label(&label);
                match result {
                    // The path AdGuard reported, not the one we asked for —
                    // the two differ, and this is the one that exists.
                    Ok(path) => toasts.add_toast(toast(&format!("Written to {}", path.display()))),
                    Err(err) => toasts.add_toast(toast(&err.to_string())),
                }
            },
        );
    });
}

/// Restore: pick a zip, **read its manifest**, then confirm.
///
/// The manifest check is not optional and not cosmetic. `import-settings`
/// accepts a *logs* zip at exit 0 with wording identical to the correct case
/// and leaves a partial install (contract §13), so a picker wired straight to
/// the CLI is unsafe. This is the point where the file can still be refused
/// with an explanation, which is why `Cli::import_settings` does not do the
/// check itself.
fn choose_backup(button: &gtk::Button, cli: &Cli, toasts: &adw::ToastOverlay) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Backup (zip)"));
    filter.add_pattern("*.zip");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder()
        .title("Choose a backup")
        .filters(&filters)
        .build();
    let cli = cli.clone();
    let toasts = toasts.clone();
    let button = button.clone();
    dialog.open(window(&button).as_ref(), gtk::gio::Cancellable::NONE, move |result| {
        let Ok(file) = result else {
            return;
        };
        let Some(path) = file.path() else {
            toasts.add_toast(toast("That file is not on this machine"));
            return;
        };

        // Read the manifest before anything else. A file picker filters on the
        // extension, which both bundles share along with their default name.
        match entries(&path).map(|names| classify(&names)) {
            Ok(Bundle::Settings) => confirm_import(&button, &cli, &toasts, path),
            Ok(Bundle::Logs) => toasts.add_toast(toast(
                "That is a logs bundle, not a settings backup. AdGuard would accept it \
                 and leave your settings half-replaced.",
            )),
            Ok(Bundle::Neither) => {
                toasts.add_toast(toast("That zip was not made by AdGuard"));
            }
            Err(err) => toasts.add_toast(toast(&err.to_string())),
        }
    });
}

fn confirm_import(button: &gtk::Button, cli: &Cli, toasts: &adw::ToastOverlay, path: PathBuf) {
    let dialog = adw::AlertDialog::new(Some("Restore these settings?"), Some(IMPORT_WARNING));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("restore", "Restore");
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    // Resolved before the button moves into the closure below.
    let parent = window(button);
    let cli = cli.clone();
    let toasts = toasts.clone();
    let button = button.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response != "restore" {
            return;
        }
        dialog.close();
        let cli = cli.clone();
        let toasts = toasts.clone();
        let path = path.clone();
        let button = button.clone();
        button.set_sensitive(false);
        worker::run(
            move || cli.import_settings(&path),
            move |result: Result<(), adguard_core::Error>| {
                button.set_sensitive(true);
                match result {
                    // Deliberately does not claim the settings are live. The
                    // daemon may need a restart, which contract §5 has its own
                    // wording for and this path cannot see.
                    Ok(()) => toasts.add_toast(toast(
                        "Settings restored. Restart the proxy for them to take effect.",
                    )),
                    Err(err) => toasts.add_toast(toast(&err.to_string())),
                }
            },
        );
    });
    dialog.present(parent.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **An import does not destroy the licence or the CA** — measured on a
    /// licensed install, contract §13 — so a confirmation that warned about
    /// them would be false. This is the assertion that keeps a well-meaning
    /// edit from adding the scariest sentence in the file.
    #[test]
    fn the_import_warning_does_not_claim_the_licence_is_at_risk() {
        let lowered = IMPORT_WARNING.to_lowercase();
        assert!(lowered.contains("licence"), "it should mention them, to say they are safe");
        assert!(
            !lowered.contains("lose your licence") && !lowered.contains("licence will be"),
            "the warning implies losing the licence, which is measured false"
        );
        assert!(lowered.contains("not affected"));
    }

    /// The one thing nothing else in the flow would tell the user: a round trip
    /// drops the DNS catalogue and `dns_user.txt`.
    #[test]
    fn both_strings_name_what_a_backup_does_not_carry() {
        for text in [ROUND_TRIP_LOSS, IMPORT_WARNING] {
            let lowered = text.to_lowercase();
            assert!(lowered.contains("dns"), "stopped naming DNS: {text}");
        }
        assert!(ROUND_TRIP_LOSS.to_lowercase().contains("will not come back"));
    }
}
