//! The request search on the Activity page.
//!
//! Over AdGuard's own log, read in place, and never over anything this
//! application keeps: `adguard_core::requests` says why, and the group's
//! description says it to the user. A search reads up to a few hundred
//! thousand lines, measured at under 0.2 s in a release build when nothing
//! matches, so it runs on a worker a moment after typing stops rather than on
//! every key.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adguard_core::access;
use adguard_core::activity::Action;
use adguard_core::requests::{self, Found, Hit, Query};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::worker;

/// Said under the group title until the first search says how far back the
/// log reaches.
const WHAT_IT_IS: &str = "Single requests, searched in AdGuard's own log. Nothing found here is \
                          kept by AdGuard UI.";

/// The action filter's entries, in the dropdown's order.
const ACTIONS: [(Option<Action>, &str); 5] = [
    (None, "Any outcome"),
    (Some(Action::Blocked), "Blocked"),
    (Some(Action::Modified), "Modified"),
    (Some(Action::Allowed), "Allowed by exception"),
    (Some(Action::Passed), "Passed"),
];

pub struct RequestSearch {
    group: adw::PreferencesGroup,
    entry: gtk::SearchEntry,
    outcome: gtk::DropDown,
    /// The rows of the last result, removed whole before the next.
    rows: RefCell<Vec<gtk::Widget>>,
    /// Filter list names by id, from the Activity page's last reading.
    names: RefCell<HashMap<i64, String>>,
    busy: Cell<bool>,
    /// A search was asked for while one ran; run the newest when it ends.
    again: Cell<bool>,
}

impl RequestSearch {
    pub fn new() -> Rc<Self> {
        let outcome = gtk::DropDown::from_strings(&ACTIONS.map(|(_, label)| label));
        outcome.set_valign(gtk::Align::Center);
        outcome.set_tooltip_text(Some("Only requests that ended this way"));
        let group = adw::PreferencesGroup::builder()
            .title("Requests")
            .description(WHAT_IT_IS)
            .header_suffix(&outcome)
            .build();
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Site, address, app or rule")
            .search_delay(300)
            .hexpand(true)
            .margin_bottom(12)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label("Search requests")]);
        group.add(&entry);

        let this = Rc::new(Self {
            group,
            entry,
            outcome,
            rows: RefCell::new(Vec::new()),
            names: RefCell::new(HashMap::new()),
            busy: Cell::new(false),
            again: Cell::new(false),
        });
        this.entry.connect_search_changed({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.run();
                }
            }
        });
        this.outcome.connect_selected_notify({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.run();
                }
            }
        });
        this
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// The filter list names the rule lines are shown with.
    pub fn set_names(&self, names: HashMap<i64, String>) {
        self.names.replace(names);
    }

    /// Search for `text`, as though it had been typed. For links from other
    /// pages to this one.
    pub fn search_for(self: &Rc<Self>, text: &str) {
        self.outcome.set_selected(0);
        if self.entry.text() == text {
            self.run();
        } else {
            // Fires `search-changed` after the delay, which runs the search.
            self.entry.set_text(text);
        }
        self.entry.grab_focus();
    }

    /// Search with what the controls hold now.
    pub fn run(self: &Rc<Self>) {
        if self.busy.replace(true) {
            self.again.set(true);
            return;
        }
        let query = Query {
            text: self.entry.text().to_string(),
            action: ACTIONS.get(self.outcome.selected() as usize).and_then(|(action, _)| *action),
            limit: requests::LIMIT,
        };
        let this = self.clone();
        worker::run(
            move || access::path().map(|live| requests::search(&live, &query)),
            move |found: Option<Found>| {
                this.busy.set(false);
                if this.again.take() {
                    // What was just found answers a question nobody is asking
                    // any more; ask the current one instead.
                    this.run();
                    return;
                }
                this.render(found.as_ref());
            },
        );
    }

    fn render(&self, found: Option<&Found>) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let mut rows: Vec<gtk::Widget> = Vec::new();

        match found {
            None => {
                self.group.set_description(Some(
                    "AdGuard's data directory could not be found, so there is no log to search.",
                ));
            }
            Some(found) => {
                self.group.set_description(Some(&describe(found)));
                let names = self.names.borrow();
                for hit in &found.hits {
                    rows.push(hit_row(hit, &names).upcast());
                }
                if found.hits.is_empty() {
                    let row = adw::ActionRow::builder()
                        .title("No request in AdGuard's log matches")
                        .build();
                    row.add_css_class("dim-label");
                    rows.push(row.upcast());
                }
                if found.more {
                    let row = adw::ActionRow::builder()
                        .title(format!("Showing the newest {}", found.hits.len()))
                        .subtitle("Add words to the search to reach older requests.")
                        .build();
                    row.add_css_class("dim-label");
                    rows.push(row.upcast());
                }
            }
        }
        for row in &rows {
            self.group.add(row);
        }
        self.rows.replace(rows);
    }
}

/// The group's description once a search is in: how far back it could look.
fn describe(found: &Found) -> String {
    match found.reaches_back {
        Some(oldest) => format!(
            "Single requests, searched in AdGuard's own log, which reaches back to {}. Nothing \
             found here is kept by AdGuard UI.",
            stamp(oldest, "%a %-d %b %H:%M")
        ),
        None => "AdGuard has not logged any requests yet.".to_owned(),
    }
}

fn hit_row(hit: &Hit, names: &HashMap<i64, String>) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    // Addresses and rule text are full of characters markup would read.
    row.set_use_markup(false);
    let target = hit
        .target
        .as_deref()
        .or(hit.host.as_deref())
        .unwrap_or("(no address)");
    row.set_title(target);
    row.set_title_lines(2);
    row.set_title_selectable(true);
    row.set_subtitle(&subtitle(hit, names));
    row.set_subtitle_lines(3);

    let (icon, class, word) = marker(hit.action);
    let image = gtk::Image::from_icon_name(icon);
    if let Some(class) = class {
        image.add_css_class(class);
    }
    image.update_property(&[gtk::accessible::Property::Label(word)]);
    image.set_tooltip_text(Some(word));
    row.add_prefix(&image);
    row
}

/// When, which program, over what, and what became of it — and by which rule.
fn subtitle(hit: &Hit, names: &HashMap<i64, String>) -> String {
    let client = match hit.client.as_str() {
        "internal_proxy_client" => "AdGuard itself",
        name => name,
    };
    let mut line = format!(
        "{} · {client} · {}",
        stamp(hit.at, "%a %-d %b %H:%M:%S"),
        hit.protocol
    );
    if let Some(status) = hit.status {
        line.push_str(&format!(" · {status}"));
    }
    line.push_str(" · ");
    line.push_str(outcome(hit.action));
    if let (Some(filter), Some(rule)) = (hit.filter, hit.rule.as_deref()) {
        let list = names.get(&filter).cloned().unwrap_or_else(|| format!("filter list {filter}"));
        line.push_str(&format!("\n{rule}  — {list}"));
    }
    line
}

fn outcome(action: Action) -> &'static str {
    match action {
        Action::Passed => "passed",
        Action::Blocked => "blocked",
        Action::Modified => "modified",
        Action::Allowed => "allowed by an exception",
        // Contract §9: what `-` means is not measured, so it is named for
        // what the log shows.
        Action::Uninspected => "no action logged",
    }
}

/// The action's icon, colour and spoken word.
fn marker(action: Action) -> (&'static str, Option<&'static str>, &'static str) {
    match action {
        Action::Blocked => ("action-unavailable-symbolic", Some("error"), "Blocked"),
        Action::Modified => ("document-edit-symbolic", Some("warning"), "Modified"),
        Action::Allowed => ("object-select-symbolic", Some("success"), "Allowed by exception"),
        Action::Passed => ("network-transmit-receive-symbolic", Some("dim-label"), "Passed"),
        Action::Uninspected => ("dialog-question-symbolic", Some("dim-label"), "No action logged"),
    }
}

/// Microseconds as local time.
fn stamp(micros: i64, format: &str) -> String {
    glib::DateTime::from_unix_local(micros.div_euclid(1_000_000))
        .ok()
        .and_then(|at| at.format(format).ok())
        .map_or_else(|| micros.to_string(), |text| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(action: Action, rule: Option<(i64, &str)>) -> Hit {
        Hit {
            at: 0,
            client: "firefox".to_owned(),
            protocol: "HTTP2".to_owned(),
            target: Some("https://a.example/".to_owned()),
            host: Some("a.example".to_owned()),
            status: Some(200),
            action,
            filter: rule.map(|(id, _)| id),
            rule: rule.map(|(_, text)| text.to_owned()),
        }
    }

    #[test]
    fn a_decided_request_names_its_rule_and_its_list() {
        let names = HashMap::from([(2, "AdGuard Base filter".to_owned())]);
        let line = subtitle(&hit(Action::Blocked, Some((2, "||a.example^"))), &names);
        assert!(line.contains("blocked"), "{line}");
        assert!(line.contains("||a.example^"), "{line}");
        assert!(line.contains("AdGuard Base filter"), "{line}");
        let unnamed = subtitle(&hit(Action::Blocked, Some((9, "||a.example^"))), &HashMap::new());
        assert!(unnamed.contains("filter list 9"), "{unnamed}");
    }

    #[test]
    fn only_a_block_is_drawn_in_the_error_colour() {
        for action in Action::ALL {
            let (_, class, _) = marker(action);
            assert_eq!(class == Some("error"), action == Action::Blocked, "{action:?}");
        }
    }

    #[test]
    fn every_outcome_the_filter_offers_is_one_the_log_has() {
        assert_eq!(ACTIONS[0].0, None, "the first entry is the unfiltered one");
        for (action, _) in &ACTIONS[1..] {
            assert!(Action::ALL.contains(&action.expect("a real action")));
        }
    }
}
