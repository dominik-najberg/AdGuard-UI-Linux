//! Editing a user-rules file — `user.txt` or `dns_user.txt` — in place.
//!
//! AdGuard CLI has no command for a user rule: the file *is* the interface,
//! and before this the pages could only print its path. So the dialog edits
//! the file itself, and saving replaces it whole through
//! [`filters::write_user_rules`], the same rename the allowlist uses, so
//! AdGuard never reads half a file.
//!
//! **Saving is not the end of it.** AdGuard does not watch either file, so
//! each caller decides what makes the running proxy read it again — the web
//! rules have a reload command, the DNS rules only a restart. The dialog
//! writes, and hands back.
//!
//! **Closing with unsaved text asks first.** The text is the user's own
//! typing, with no other copy anywhere, and Escape is one keypress away.

use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::filters;
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::abbreviate;

/// Under the text, so the syntax is not something to look up first.
const HOW_TO_WRITE: &str = "One rule per line. Lines starting with ! are comments.";

/// Open the editor for the rules at `path`. `saved` runs after a successful
/// save, with the dialog already closed.
pub fn open(parent: &impl IsA<gtk::Widget>, heading: &str, path: PathBuf, saved: impl Fn() + 'static) {
    // Absent is a normal state — AdGuard creates the file lazily — and reads
    // as an empty file. Anything else is shown rather than edited over: an
    // editor that opened empty on a read error would save over the rules.
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            let dialog = adw::AlertDialog::new(
                Some("Your rules could not be opened"),
                Some(&format!("{}: {err}", abbreviate(&path))),
            );
            dialog.add_response("close", "Close");
            dialog.present(Some(parent));
            return;
        }
    };

    let buffer = gtk::TextBuffer::new(None);
    // The file as it was is not an edit: Ctrl+Z stops there, not at empty.
    buffer.begin_irreversible_action();
    buffer.set_text(&original);
    buffer.end_irreversible_action();
    let text = gtk::TextView::builder()
        .buffer(&buffer)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .vexpand(true)
        .build();
    text.update_property(&[gtk::accessible::Property::Label(heading)]);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&text)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder().label("Save").sensitive(false).build();
    save.add_css_class("suggested-action");
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);

    // The syntax on the left, the live count and the file on the right — the
    // count answers "did that line count as a rule" while typing.
    let hint = gtk::Label::builder()
        .label(HOW_TO_WRITE)
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    let count = gtk::Label::builder().xalign(1.0).build();
    count.add_css_class("dim-label");
    count.add_css_class("caption");
    count.add_css_class("numeric");
    count.set_tooltip_text(Some(&abbreviate(&path)));
    let footer = gtk::Box::builder()
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    footer.append(&hint);
    footer.append(&count);

    let view = adw::ToolbarView::builder()
        .content(&scroller)
        .top_bar_style(adw::ToolbarStyle::Raised)
        .bottom_bar_style(adw::ToolbarStyle::Raised)
        .build();
    view.add_top_bar(&header);
    view.add_bottom_bar(&footer);

    let dialog = adw::Dialog::builder()
        .title(heading)
        .content_width(640)
        .content_height(520)
        .child(&view)
        .build();

    let original = Rc::new(original);
    let paint_count = {
        let count = count.clone();
        let path = path.clone();
        move |text: &str| {
            let rules = filters::rule_count(text);
            let plural = if rules == 1 { "rule" } else { "rules" };
            count.set_label(&format!("{rules} {plural} · {}", short_name(&path)));
        }
    };
    paint_count(&original);

    buffer.connect_changed({
        let save = save.clone();
        let dialog = dialog.downgrade();
        let original = original.clone();
        move |buffer| {
            let now = contents(buffer);
            let dirty = now != *original;
            save.set_sensitive(dirty);
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_can_close(!dirty);
            }
            paint_count(&now);
        }
    });

    // Fires only while there are unsaved changes: `can-close` is true
    // otherwise, and the dialog just closes.
    dialog.connect_close_attempt(confirm_discard);
    cancel.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });

    let saved: Rc<dyn Fn()> = Rc::new(saved);
    let write: Rc<dyn Fn()> = Rc::new({
        let dialog = dialog.downgrade();
        let buffer = buffer.clone();
        let hint = hint.clone();
        move || {
            let Some(dialog) = dialog.upgrade() else { return };
            let mut next = contents(&buffer);
            // A file of lines ends in a newline, the shape the allowlist
            // appends to.
            if !next.is_empty() && !next.ends_with('\n') {
                next.push('\n');
            }
            match filters::write_user_rules(&path, &next) {
                Ok(()) => {
                    dialog.force_close();
                    saved();
                }
                // Said where the user is looking, and the text stays, so
                // nothing typed is lost to a failed write.
                Err(err) => {
                    hint.set_label(&format!("Not saved: {err}"));
                    hint.remove_css_class("dim-label");
                    hint.add_css_class("error");
                }
            }
        }
    });
    save.connect_clicked({
        let write = write.clone();
        move |_| write()
    });

    // Ctrl+S, the shortcut every editor has taught. Inert while there is
    // nothing to save, as the button is.
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_scope(gtk::ShortcutScope::Managed);
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>s"),
        Some(gtk::CallbackAction::new({
            let save = save.clone();
            move |_, _| {
                if save.is_sensitive() {
                    write();
                }
                glib::Propagation::Stop
            }
        })),
    ));
    dialog.add_controller(shortcuts);

    dialog.present(Some(parent));
    text.grab_focus();
}

/// The pencil that opens the editor, put on `row` just before its switch.
///
/// An icon, as the entry rows above it on the DNS page have, and before the
/// switch so every switch on a page still lines up in the last column. Flat,
/// so it does not compete with the switch, and named for a screen reader and
/// in a tooltip, since the icon alone says nothing about which file.
pub fn attach(row: &adw::SwitchRow, heading: &'static str, path: PathBuf, saved: impl Fn() + Clone + 'static) {
    let button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text(format!("Edit {}", abbreviate(&path)))
        .build();
    button.add_css_class("flat");
    button.update_property(&[gtk::accessible::Property::Label(&format!("Edit {heading}"))]);
    button.connect_clicked(move |button| open(button, heading, path.clone(), saved.clone()));

    row.add_suffix(&button);
    // `AdwSwitchRow` adds its switch as the first suffix when it is built, so
    // anything added later lands after it. The suffixes share one box; moving
    // the button to the front of it puts it before the switch.
    if let Some(suffixes) = button.parent().and_downcast::<gtk::Box>() {
        suffixes.reorder_child_after(&button, None::<&gtk::Widget>);
    }
}

/// Ask before throwing away what was typed.
fn confirm_discard(dialog: &adw::Dialog) {
    let alert = adw::AlertDialog::new(
        Some("Discard your changes?"),
        Some("The rules you changed have not been saved."),
    );
    alert.add_response("keep", "Keep Editing");
    alert.add_response("discard", "Discard");
    alert.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("keep"));
    alert.set_close_response("keep");
    let target = dialog.downgrade();
    alert.connect_response(None, move |_, response| {
        if response == "discard" {
            if let Some(dialog) = target.upgrade() {
                dialog.force_close();
            }
        }
    });
    alert.present(Some(dialog));
}

fn contents(buffer: &gtk::TextBuffer) -> String {
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

/// The file's name alone, for the footer; the full path is its tooltip.
fn short_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| abbreviate(path))
}


