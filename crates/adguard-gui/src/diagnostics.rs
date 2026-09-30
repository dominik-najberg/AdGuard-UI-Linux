//! The Diagnostics page: every check this application makes, one line each,
//! and a copy of them for a bug report.
//!
//! Built for [issue #21]. The checks are all `adguard_core::diagnostics`'s, and
//! it already says the important part: nothing here is a new way of knowing
//! anything, and the difference from the Status page is that nothing is
//! reduced. Status answers *am I protected?*; this page shows each thing that
//! answer was weighed from.
//!
//! # It reads when it is opened, and not otherwise
//!
//! A full reading is three `adguard-cli` invocations, a walk of `/proc` and a
//! read of a few mebibytes of access log. That is fine once, on request, and
//! wrong on a timer — so there is no poll and no file monitor behind this page.
//! It reads when the sidebar selects it and when the refresh button is pressed
//! while it is showing, and it says when the reading was taken, so a snapshot
//! is never mistaken for a live view.
//!
//! # It writes nothing, and it links nowhere yet
//!
//! Every fix for a problem shown here already lives on the page that owns it —
//! the helper's command on Advanced, the certificate's and the browsers' on
//! Protection, the restart on Status. Repeating those controls here would be a
//! second copy of each to keep in step; the page names the problem and the
//! other pages keep their one writer.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::diagnostics::{self, Inputs, Level, Report};
use adguard_core::Cli;
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{toast, worker};

/// What the page is, before anything has been read.
const WHAT_IT_IS: &str =
    "Every check this application makes, each on its own line. Read when you open this \
     page — press refresh to read again.";

/// What the copy button leaves out, said beside the button rather than after
/// the fact: the report is written for a public issue tracker.
const WHAT_THE_COPY_HOLDS: &str =
    "Copied as plain text for a bug report. Your home folder is shortened to ~, and the \
     report never includes your licence key, e-mail, network addresses or browsing.";

pub struct DiagnosticsPage {
    cli: Cli,
    toasts: adw::ToastOverlay,
    page: adw::PreferencesPage,
    /// The first group: what the page is, when it was read, and the copy button.
    summary: adw::PreferencesGroup,
    copy: gtk::Button,
    /// One group per report section. Rebuilt whole on every reading: the set of
    /// lines depends on the machine — one per browser found, per-run lines only
    /// with exactly one daemon — so there is nothing stable to patch.
    sections: RefCell<Vec<adw::PreferencesGroup>>,
    /// The last reading, for the copy button. Kept as text rather than rebuilt
    /// on click, so what is copied is exactly what was on screen.
    text: RefCell<Option<String>>,
    /// A reading is in flight. A second press of refresh while one is running
    /// would put two sets of CLI calls against one data directory (contract
    /// §3) for an answer the first is about to give.
    busy: Cell<bool>,
}

impl DiagnosticsPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        let copy = gtk::Button::builder()
            .label("Copy report")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .tooltip_text(WHAT_THE_COPY_HOLDS)
            .build();
        let summary = adw::PreferencesGroup::builder()
            .title("Diagnostics")
            .description(WHAT_IT_IS)
            .header_suffix(&copy)
            .build();
        page.add(&summary);

        let this = Rc::new(Self {
            cli,
            toasts,
            page,
            summary,
            copy,
            sections: RefCell::new(Vec::new()),
            text: RefCell::new(None),
            busy: Cell::new(false),
        });

        this.copy.connect_clicked({
            // Weak: the button is inside the page this owns.
            let this = Rc::downgrade(&this);
            move |button| {
                let Some(this) = this.upgrade() else { return };
                let text = this.text.borrow().clone();
                if let Some(text) = text {
                    button.clipboard().set_text(&text);
                    this.toasts.add_toast(toast("Report copied"));
                }
            }
        });

        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    /// Read everything again. Called when the page is selected and by the
    /// refresh button while it is showing.
    pub fn reload(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.copy.set_sensitive(false);
        self.summary.set_description(Some("Reading…"));

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || Report::build(&Inputs::collect(&cli)),
            move |report: Report| {
                this.busy.set(false);
                this.render(&report);
            },
        );
    }

    fn render(&self, report: &Report) {
        for group in self.sections.take() {
            self.page.remove(&group);
        }

        let home = std::env::var("HOME").ok();
        let groups: Vec<_> = report
            .sections
            .iter()
            .map(|section| section_group(section, home.as_deref()))
            .collect();
        for group in &groups {
            self.page.add(group);
        }
        self.sections.replace(groups);

        let text = diagnostics::redact_home(
            &report.text(env!("CARGO_PKG_VERSION")),
            home.as_deref(),
        );
        self.text.replace(Some(text));
        self.copy.set_sensitive(true);
        self.summary.set_description(Some(&summary(report.problems().count())));
    }
}

/// The group's description once a reading is in: when, and how many problems.
fn summary(problems: usize) -> String {
    let when = glib::DateTime::now_local()
        .ok()
        .and_then(|now| now.format("%H:%M").ok())
        .map_or_else(|| "just now".to_owned(), |at| format!("at {at}"));
    let found = match problems {
        0 => "nothing wrong found".to_owned(),
        1 => "1 problem found".to_owned(),
        n => format!("{n} problems found"),
    };
    format!("Read {when} — {found}. Press refresh to read again.")
}

/// One report section as a group. Values are shown with the home directory
/// shortened, as every other page shows a path, and so the screen and the
/// copied text read the same.
fn section_group(section: &diagnostics::Section, home: Option<&str>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(section.title).build();
    if let Some(note) = section.note {
        group.set_description(Some(note));
    }
    for finding in &section.findings {
        group.add(&finding_row(finding, home));
    }
    group
}

fn finding_row(finding: &diagnostics::Finding, home: Option<&str>) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    // Before the strings, which are consumed as they are set: CLI messages and
    // paths can contain `&`, and markup is on by default.
    row.set_use_markup(false);
    row.set_title(finding.label);
    row.set_subtitle(&diagnostics::redact_home(&finding.value, home));
    row.set_subtitle_selectable(true);

    let image = match marker(finding.level) {
        Some((icon, class, word)) => {
            let image = gtk::Image::from_icon_name(icon);
            image.add_css_class(class);
            // The icon is the only thing distinguishing a failure from a pass,
            // so it has to reach a screen reader as a word.
            image.update_property(&[gtk::accessible::Property::Label(word)]);
            image
        }
        // An empty image of the same size, so a plain reading's title lines up
        // with the checked ones around it instead of sitting an icon's width
        // to the left of them.
        None => {
            let spacer = gtk::Image::new();
            spacer.set_pixel_size(16);
            spacer.set_accessible_role(gtk::AccessibleRole::Presentation);
            spacer
        }
    };
    row.add_prefix(&image);
    row
}

/// The icon, its colour and its spoken word, by level. A plain reading gets
/// none, so a row with an icon is always one that is saying something.
fn marker(level: Level) -> Option<(&'static str, &'static str, &'static str)> {
    match level {
        // Not `emblem-ok-symbolic`: Adwaita 50 no longer ships it, and a name
        // the theme lacks renders as a broken-image glyph beside every passing
        // line — measured headlessly on this page's first run.
        Level::Healthy => Some(("object-select-symbolic", "success", "OK")),
        Level::Problem => Some(("dialog-warning-symbolic", "error", "Problem")),
        Level::Unknown => Some(("dialog-question-symbolic", "dim-label", "Unknown")),
        Level::Fact => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Unknown" must not look like a failure — the rule the core module keeps
    /// has to survive the colour, too.
    #[test]
    fn only_a_problem_is_drawn_in_the_error_colour() {
        for level in [Level::Healthy, Level::Unknown, Level::Fact] {
            assert_ne!(marker(level).map(|(_, class, _)| class), Some("error"));
        }
        assert_eq!(marker(Level::Problem).map(|(_, class, _)| class), Some("error"));
    }

    #[test]
    fn the_copy_disclosure_names_what_is_left_out() {
        for left_out in ["licence key", "e-mail", "browsing", "~"] {
            assert!(WHAT_THE_COPY_HOLDS.contains(left_out), "{left_out}");
        }
    }
}
