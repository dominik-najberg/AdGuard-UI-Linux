//! The Filters page: the filter catalogue, one switch per filter.
//!
//! State is read from the SQLite catalogue and written through
//! `adguard-cli filters` — the two directions never cross (see
//! `docs/architecture.md` §3). Every toggle therefore follows
//! act -> re-read -> reconcile: the CLI reports semantic failures at exit 0,
//! so the switch is only allowed to settle on a state the database confirms.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use adguard_core::filters::{self, Catalogue};
use adguard_core::{
    Cli, Consent, Filter, FilterAction, FilterCatalogue, FilterSet, FilterState, Locale,
    ANNOYANCE_TERMS,
};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, worker};

/// One read of everything the page renders.
struct Loaded {
    catalogue: FilterCatalogue,
    /// How many rules the user has actually written, for the "Your rules"
    /// subtitle. `None` when the file is unreadable or absent.
    user_rules: Option<usize>,
    user_rules_path: Option<PathBuf>,
}

/// A rendered filter and the state it was rendered from.
struct Row {
    switch: adw::SwitchRow,
    /// Last state the database confirmed. `Filter::action_for` reads from
    /// this, so a switch flip is always decided against observed reality
    /// rather than against what the UI happens to be showing.
    filter: RefCell<Filter>,
    /// The trust control, on the rows that have one. `None` everywhere else —
    /// which is every row but a custom list on the HTTP page, since a DNS list
    /// has no command to call and AdGuard refuses a catalogue filter itself.
    /// See `Filter::supports_trust`.
    trust: Option<Trust>,
}

/// The two widgets that report whether a list is trusted.
///
/// Painted together by [`FiltersPage::paint_trust`], from the row's own record
/// and never from what was asked for, so the icon, the tooltip, the label a
/// screen reader reads and the sentence under the name cannot disagree with
/// each other or with the database.
struct Trust {
    button: gtk::Button,
    /// The prefix warning image, hidden while the list is untrusted — the same
    /// widget and glyph the Protection and DNS pages use to mark a switch whose
    /// consequence the row would not otherwise disclose. Undecorated for
    /// accessibility, as those are: the subtitle says it in words, and labelling
    /// the image would announce it twice.
    caveat: gtk::Image,
}

/// What a trusted list's row says under its name.
///
/// It **displaces** the list's own `! Description:` header rather than joining
/// it. The Protection page settles the same competition the same way — while
/// the DNS row is inert its subtitle becomes the caveat and goes back to the
/// description afterwards — and two sentences sharing a two-line ellipsised
/// subtitle would leave whichever came second truncated. A custom list's
/// description is also text written by the list the user has just decided to
/// trust, which makes it the least authoritative string on the row.
const TRUSTED_SUBTITLE: &str =
    "Trusted — may run scriptlets and HTML-filtering rules in the pages you visit";

/// One group as the search field sees it.
struct Section {
    group: adw::PreferencesGroup,
    /// The filter rows, each with the text a query is matched against — see
    /// [`haystack`], which has already lowercased it.
    rows: Vec<(gtk::Widget, String)>,
    /// Rows that are not filters: the "add by URL" entry. There is nothing in
    /// them for a query to match, so they step aside while one is running
    /// rather than sitting above results as the only row a group still shows.
    extras: Vec<gtk::Widget>,
}

impl Section {
    fn new(group: adw::PreferencesGroup) -> Self {
        Self { group, rows: Vec::new(), extras: Vec::new() }
    }

    fn add_row(&mut self, row: &impl IsA<gtk::Widget>, haystack: String) {
        self.rows.push((row.clone().upcast(), haystack));
    }

    /// Show every row, and the group with them. What no search at all looks
    /// like.
    fn show_all(&self) {
        for (row, _) in &self.rows {
            row.set_visible(true);
        }
        for row in &self.extras {
            row.set_visible(true);
        }
        self.group.set_visible(true);
    }

    /// Leave only the rows matching every term, and hide the group when that
    /// is none of them.
    ///
    /// A group with no rows of its own — a host's settings groups — therefore
    /// disappears for the length of a search, which is the point: the field
    /// searches filter lists, and a settings group cannot answer.
    fn narrow(&self, terms: &[&str]) -> bool {
        let mut matched = false;
        for (row, haystack) in &self.rows {
            let hit = terms.iter().all(|term| haystack.contains(term));
            row.set_visible(hit);
            matched |= hit;
        }
        for row in &self.extras {
            row.set_visible(false);
        }
        self.group.set_visible(matched);
        matched
    }
}

/// The text a query is matched against.
///
/// The description is in as well as the name because that is where a list says
/// what it is for — "cookie", "tracking", "annoyance" name no list in the
/// catalogue and describe a dozen. The URL is in because a custom list
/// installed without a `! Title:` header is *only* its URL, and the group name
/// because AdGuard's own categories are how the page is otherwise read.
fn haystack(filter: &Filter, group: &str) -> String {
    format!(
        "{} {} {} {}",
        filter.display_name(),
        filter.description,
        filter.download_url,
        group
    )
    .to_lowercase()
}

/// What a page hosting a catalogue contributes to it, and what it takes over.
///
/// The DNS page needs both halves. Its settings come from `proxy.yaml` and the
/// catalogue from `agflm_dns.db`, and they belong in one scrolling page rather
/// than two — so the host supplies the groups that go above, and says that the
/// user-rules row is its own.
pub struct Host {
    /// Groups rendered above the catalogue. Called on every build, and must
    /// return **fresh** widgets each time: the page they were added to is
    /// dropped when the catalogue is rebuilt, and a widget cannot be re-parented
    /// out of a dying one.
    pub prelude: Box<dyn Fn() -> Vec<adw::PreferencesGroup>>,

    /// The host renders the user-rules row itself, so the catalogue must not.
    ///
    /// Set for DNS, where the built-in row is wrong three ways over: the
    /// database has it permanently `is_enabled = 1`, `Filter::action_for` would
    /// send `dns filters enable -2147483648`, which is refused outright
    /// (contract §6), and the verification read afterwards looks at a database
    /// that cannot see the `proxy.yaml` list where the real switch lives.
    pub owns_user_rules: bool,
}

pub struct FiltersPage {
    /// The page content is swapped wholesale — spinner, error, or catalogue —
    /// which is simpler and less error-prone than reconciling child lists.
    bin: adw::Bin,
    cli: Cli,
    toasts: adw::ToastOverlay,
    set: FilterSet,
    locale: Locale,
    rows: RefCell<HashMap<i64, Row>>,
    /// What the search field hides, rebuilt with the page. See [`Section`].
    sections: RefCell<Vec<Section>>,
    /// Swaps the catalogue for "nothing matched". `None` before the first
    /// paint, and after an error, where there is no catalogue to search.
    results: RefCell<Option<gtk::Stack>>,
    /// The live query, kept across a rebuild. Installing or removing a list
    /// reloads the page, and a search that survives the row it was used to
    /// find is the difference between adding three lists and typing the same
    /// word three times.
    query: RefCell<String>,
    /// Set while we write switch states ourselves, so the `active` handler can
    /// tell a user's click from our own reconcile. Property notifications are
    /// synchronous, so a plain flag around the write is enough.
    reconciling: Cell<bool>,
    /// The first group of the catalogue proper — where [`Self::scroll_to_lists`]
    /// takes a link that meant "the lists".
    ///
    /// Deliberately not the first group on the page: a host's settings go above
    /// it, and on the DNS page those are three groups deep, so a link to the
    /// catalogue that landed at the top would land on the wrong half of a page
    /// it shares.
    lists: RefCell<Option<adw::PreferencesGroup>>,
    host: Option<Host>,
}

impl FiltersPage {
    /// A catalogue rendered inside another page's content. See [`Host`].
    pub fn hosted(
        cli: Cli,
        toasts: adw::ToastOverlay,
        set: FilterSet,
        host: Option<Host>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            cli,
            toasts,
            set,
            locale: Locale::from_env(),
            rows: RefCell::new(HashMap::new()),
            sections: RefCell::new(Vec::new()),
            results: RefCell::new(None),
            query: RefCell::new(String::new()),
            reconciling: Cell::new(false),
            lists: RefCell::new(None),
            host,
        });
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.bin
    }

    /// Re-read the catalogue and rebuild the page.
    ///
    /// Used for the initial load and the explicit refresh. Individual toggles
    /// do **not** come through here — they patch the one row they touched, so
    /// flipping a switch two thirds of the way down "Language-specific" does
    /// not throw away the scroll position.
    pub fn reload(self: &Rc<Self>) {
        self.bin.set_child(Some(&loading_view()));

        let set = self.set;
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let catalogue = Catalogue::open_set(set).map_err(|err| err.to_string())?;
                let read = catalogue.read(&locale).map_err(|err| err.to_string())?;
                let path = set.user_rules_file();
                Ok(Loaded {
                    catalogue: read,
                    user_rules: path.as_deref().and_then(filters::user_rule_count),
                    user_rules_path: path,
                })
            },
            move |result: Result<Loaded, String>| match result {
                Ok(loaded) => {
                    let view = this.view(&loaded);
                    this.bin.set_child(Some(&view));
                }
                Err(err) => {
                    // Nothing to search, and no field either: the query is kept
                    // so that a reload which succeeds returns to it.
                    *this.results.borrow_mut() = None;
                    this.sections.borrow_mut().clear();
                    // Along with them, so a link to the lists cannot be left
                    // holding a group that is no longer on any page.
                    this.lists.replace(None);
                    this.bin.set_child(Some(&error_view(&err)))
                }
            },
        );
    }

    /// Bring the filter lists to the top of the view, as a link from the Status
    /// page's count of them asks.
    ///
    /// Not marked, unlike [`crate::reveal`]: what the count is about is every
    /// group from here down, and tinting the first one would say the answer was
    /// that group. On a page with no host above the catalogue this only undoes a
    /// scroll the user left behind — which is the point, since the link says
    /// "show me the lists" and they are at the top of them.
    ///
    /// Nothing to do before the first build, or after one that failed; the page
    /// is showing a spinner or the reason, and neither scrolls.
    pub fn scroll_to_lists(&self) {
        if let Some(group) = self.lists.borrow().as_ref() {
            crate::scroll_to(group);
        }
    }

    /// The catalogue, and the field that narrows it to the list being looked
    /// for.
    ///
    /// The field sits above the page rather than scrolling with it, because
    /// what it acts on is what has scrolled off; and here rather than in the
    /// window's header bar, which is shared with five pages that have nothing
    /// to search.
    fn view(self: &Rc<Self>, loaded: &Loaded) -> gtk::Box {
        let page = self.build(loaded);

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&page, Some(RESULTS));
        stack.add_named(&no_results_view(), Some(NO_RESULTS));
        *self.results.borrow_mut() = Some(stack.clone());

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search filter lists")
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        // A query kept across the rebuild goes back in the field, so what is
        // showing and what is typed cannot disagree. `search-changed` is
        // delayed rather than synchronous, so this may still reach the handler
        // below once it is connected — harmless, since it arrives carrying the
        // query that is already applied.
        search.set_text(&self.query.borrow());

        let this = Rc::downgrade(self);
        search.connect_search_changed(move |search| {
            let Some(this) = this.upgrade() else {
                return;
            };
            *this.query.borrow_mut() = search.text().into();
            this.apply_search();
        });

        // The same clamp the page inside uses, so the field lines up with the
        // rows it filters instead of running the full width of the window.
        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .tightening_threshold(400)
            .child(&search)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&clamp);
        content.append(&stack);

        // A kept query applies to the rows that were just rebuilt.
        self.apply_search();

        content
    }

    /// Hide everything the current query excludes.
    fn apply_search(&self) {
        let query = self.query.borrow().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let sections = self.sections.borrow();

        if terms.is_empty() {
            for section in sections.iter() {
                section.show_all();
            }
        }
        // `fold`, not `any`: every section has to be narrowed, so the walk
        // cannot stop at the first one that matched.
        let matched = terms.is_empty()
            || sections.iter().fold(false, |matched, section| section.narrow(&terms) | matched);

        if let Some(results) = self.results.borrow().as_ref() {
            results.set_visible_child_name(if matched { RESULTS } else { NO_RESULTS });
        }
    }

    fn build(self: &Rc<Self>, loaded: &Loaded) -> adw::PreferencesPage {
        self.rows.borrow_mut().clear();
        let mut sections = Vec::new();

        let page = adw::PreferencesPage::new();

        // The host's groups go above the catalogue, and are rebuilt with it.
        if let Some(host) = &self.host {
            for group in (host.prelude)() {
                page.add(&group);
                // Registered with no rows, so a search hides it: DNS's settings
                // are not filter lists and cannot answer a query about one.
                sections.push(Section::new(group));
            }
        }

        let host_owns_user_rules = self.host.as_ref().is_some_and(|host| host.owns_user_rules);
        if let Some(user_rules) = &loaded.catalogue.user_rules {
            if !host_owns_user_rules {
                let section = self.user_rules_group(user_rules, loaded);
                page.add(&section.group);
                sections.push(section);
            }
        }

        let grouped = loaded.catalogue.grouped();

        // Rendered here rather than in the loop below, because it is the one
        // group that must appear while it is *empty* — it carries the row that
        // installs the first custom list, and `grouped` drops empty groups. Its
        // position is unchanged: AdGuard gives "Custom filters"
        // `display_number = 0`, so it sorts above "Ad blocking" either way.
        let customs: Vec<&Filter> = grouped
            .iter()
            .find(|(group, _)| group.is_custom())
            .map(|(_, filters)| filters.clone())
            .unwrap_or_default();
        let section = self.custom_group(&customs);
        page.add(&section.group);
        // The catalogue starts here — this group is rendered unconditionally,
        // where every group after it is dropped when it holds no filters, so it
        // is the one anchor that is always present to scroll to.
        self.lists.replace(Some(section.group.clone()));
        sections.push(section);

        for (group, filters) in grouped {
            if group.is_custom() {
                continue; // already rendered, with its install row
            }
            // Group names come from AdGuard's own categories, which carry no
            // markup; row text does, so only the rows opt out of Pango.
            let rendered = adw::PreferencesGroup::builder().title(&group.name).build();
            let mut section = Section::new(rendered);
            for filter in filters {
                let row = self.row(filter);
                section.group.add(&row);
                section.add_row(&row, haystack(filter, &group.name));
            }
            page.add(&section.group);
            sections.push(section);
        }

        *self.sections.borrow_mut() = sections;

        page
    }

    /// Lists the user installed by URL, plus the row that installs another.
    ///
    /// The description warns that a bad link is added rather than rejected
    /// because that is measured (contract §6): AdGuard checks only whether the
    /// response *begins* with HTML. A link to JSON, to prose, or to the wrong
    /// plain-text file installs a filter holding no rules and reports success —
    /// leaving a switch reading on over something that filters nothing, which
    /// nothing else in this UI would ever reveal.
    fn custom_group(self: &Rc<Self>, customs: &[&Filter]) -> Section {
        let group = adw::PreferencesGroup::builder()
            .title("Custom filters")
            .description(
                "Lists that are not in AdGuard's catalogue. Add only ones you trust: \
                 a link that does not return a filter list is still added, holding no rules.",
            )
            .build();

        let entry = adw::EntryRow::builder()
            .title("Add a filter list by URL")
            .show_apply_button(true)
            .build();

        // Shown for the length of the install, which is not a formality: the
        // CLI's own deadline is 60 s, so this row can sit there for a minute
        // (contract §6).
        let spinner = adw::Spinner::new();
        spinner.set_visible(false);
        entry.add_suffix(&spinner);

        let this = Rc::downgrade(self);
        let busy = spinner.clone();
        entry.connect_apply(move |entry| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let url = entry.text().trim().to_owned();
            if url.is_empty() {
                return;
            }
            this.install(url, entry.clone(), busy.clone());
        });

        let mut section = Section::new(group);
        section.group.add(&entry);
        section.extras.push(entry.upcast());
        for filter in customs {
            let row = self.row(filter);
            row.add_suffix(&self.remove_button(filter));
            section.group.add(&row);
            section.add_row(&row, haystack(filter, "Custom filters"));
        }

        section
    }

    /// The one control in this application that destroys something.
    ///
    /// **Only custom rows get one.** `filters remove` against a catalogue
    /// filter merely clears `is_installed` and the row stays, so turning a
    /// catalogue switch off is `disable` and there is nothing to remove;
    /// against a custom filter the row is deleted from the database outright
    /// and the only undo is re-fetching the URL (contract §6). That asymmetry
    /// is why this is a button with a confirmation of its own rather than a
    /// quiet suffix action — `architecture.md` §5.
    ///
    /// A suffix button rather than a swipe or a context menu because the row is
    /// an `AdwSwitchRow` whose activatable widget is the switch: anything
    /// subtler would be reached by the same gesture that toggles the list, and
    /// "off" and "gone" are exactly the two things that must not be confusable
    /// here.
    fn remove_button(self: &Rc<Self>, filter: &Filter) -> gtk::Button {
        let button = gtk::Button::from_icon_name("user-trash-symbolic");
        button.set_tooltip_text(Some("Remove this list"));
        button.set_valign(gtk::Align::Center);
        button.add_css_class("flat");
        // Adwaita's own colour for an action with no undo. The confirmation is
        // the real safeguard; this is what stops the button reading as "off".
        button.add_css_class("destructive-action");
        // The switch already carries the row's name, so an icon button beside
        // it reaches the accessibility tree as an unnamed control otherwise —
        // and a screen reader would announce "button" next to every list.
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove {}",
            filter.display_name()
        ))]);

        let this = Rc::downgrade(self);
        let filter = filter.clone();
        button.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let filter = filter.clone();
            glib::spawn_future_local(async move {
                if this.confirm_removal(&filter).await {
                    this.remove(&filter);
                }
            });
        });

        button
    }

    /// Ask before deleting a list, and say what cannot be undone.
    ///
    /// The wording names the URL because that is the only thing that can bring
    /// the list back, and because a custom list installed without a `! Title:`
    /// header has no name of its own — its URL *is* its identity (contract §6).
    async fn confirm_removal(&self, filter: &Filter) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("Remove this filter list?"),
            Some(&format!(
                "{} will be deleted from AdGuard, not just switched off. There is no \
                 undo: getting it back means adding {} again.\n\nTo stop it filtering \
                 without losing it, switch it off instead.",
                filter.display_name(),
                filter.download_url,
            )),
        );
        // Not markup: a list's title is AdGuard's text or the user's, and both
        // can carry `&` — the same rule every row and toast here follows.
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        // Cancel is the default and the escape route, because the other answer
        // is the irreversible one.
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "remove"
    }

    /// Delete one custom list, then confirm it against the database.
    ///
    /// Verified by the row being **gone**, not by `Filter [ID: …] removed`,
    /// which proves nothing: every filter command exits 0 and prints a
    /// confirmation whether or not it did anything (contract §6). This is the
    /// mirror of `install`'s check — there, a row that was not there before;
    /// here, a row that is not there after.
    fn remove(self: &Rc<Self>, filter: &Filter) {
        let id = filter.id;
        let name = filter.display_name().to_owned();
        if let Some(row) = self.rows.borrow().get(&id) {
            // Insensitive for the duration, so the switch cannot be flipped on
            // a filter that is being deleted.
            row.switch.set_sensitive(false);
        }

        let cli = self.cli.clone();
        let set = self.set;
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let refused = cli
                    // Removal is never gated: the agreement is about switching
                    // a list *on*.
                    .filter_action(set, FilterAction::Remove, id, Consent::Withheld)
                    .err()
                    .map(|err| err.to_string());
                // `None` when the catalogue could not be read at all, which is
                // not the same as "no row with that id" — reporting an
                // unreadable database as a successful deletion would be the
                // worst possible direction to guess in.
                let still_there = Catalogue::open_set(set)
                    .and_then(|catalogue| catalogue.custom_filters(&locale))
                    .ok()
                    .map(|customs| customs.iter().any(|f| f.id == id));
                (refused, still_there)
            },
            move |(refused, still_there)| this.settle_removal(id, &name, refused, still_there),
        );
    }

    fn settle_removal(
        self: &Rc<Self>,
        id: i64,
        name: &str,
        refused: Option<String>,
        still_there: Option<bool>,
    ) {
        match still_there {
            Some(false) => {
                self.toasts.add_toast(toast(&format!("Removed {name}")));
                // A row has disappeared, so there is nothing to patch — the
                // same reason `install` rebuilds rather than reconciling.
                self.reload();
            }
            // The CLI's own wording first: a filter another window already
            // removed comes back as `Failed to remove filter with ID: …:
            // Filter not found`, which says more than we would.
            Some(true) => {
                if let Some(row) = self.rows.borrow().get(&id) {
                    row.switch.set_sensitive(true);
                }
                self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                    format!("AdGuard reported {name} was removed, but it is still in the list")
                })));
            }
            // The command may well have worked; we simply cannot say. Reload
            // rather than claim either outcome, and let the page show what the
            // database actually holds once it can be read.
            None => {
                self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                    format!("Could not re-read the catalogue to confirm {name} was removed")
                })));
                self.reload();
            }
        }
    }

    /// Fetch and subscribe to one list, then confirm it against the database.
    ///
    /// The verification is not the usual re-read of a known row: the new
    /// filter's id is assigned by AdGuard and cannot be known in advance, so the
    /// custom rows are read *before* and *after* and a row that was not there
    /// before is the evidence. Matching on the URL would be the obvious
    /// alternative and is wrong — a local path is stored back as `file://…`.
    fn install(self: &Rc<Self>, url: String, entry: adw::EntryRow, spinner: adw::Spinner) {
        entry.set_sensitive(false);
        spinner.set_visible(true);

        let cli = self.cli.clone();
        let set = self.set;
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let read = || {
                    Catalogue::open_set(set)
                        .and_then(|catalogue| catalogue.custom_filters(&locale))
                        .ok()
                };
                // `None` rather than an empty set when the read fails: an
                // unreadable "before" would make every list already installed
                // look new, and report a success that did not happen.
                let before: Option<HashSet<i64>> =
                    read().map(|filters| filters.into_iter().map(|f| f.id).collect());

                let refused = cli.filters_install(set, &url).err().map(|err| err.to_string());

                let added = match (before, read()) {
                    (Some(before), Some(after)) => {
                        after.into_iter().find(|f| !before.contains(&f.id))
                    }
                    _ => None,
                };
                (refused, added)
            },
            move |(refused, added)| this.settle_install(refused, added, &entry, &spinner),
        );
    }

    fn settle_install(
        self: &Rc<Self>,
        refused: Option<String>,
        added: Option<Filter>,
        entry: &adw::EntryRow,
        spinner: &adw::Spinner,
    ) {
        spinner.set_visible(false);
        entry.set_sensitive(true);

        if let Some(added) = added {
            self.toasts
                .add_toast(toast(&format!("Added {}", added.display_name())));
            // The whole page is rebuilt rather than one row patched, unlike a
            // toggle: an install adds a row, and there is nothing to patch.
            // It also clears the entry, which is what should happen on success.
            self.reload();
            return;
        }

        // Left as it was typed, so a mistyped URL can be corrected rather than
        // retyped.
        self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
            "AdGuard reported the list was installed, but it is not in the catalogue".to_owned()
        })));
    }

    /// The user's own rules — a toggle, not a subscribable list, so it gets
    /// its own group above the catalogue.
    fn user_rules_group(self: &Rc<Self>, user_rules: &Filter, loaded: &Loaded) -> Section {
        let group = adw::PreferencesGroup::builder().title("Your rules").build();

        let row = self.row(user_rules);
        row.set_subtitle(&match (loaded.user_rules, &loaded.user_rules_path) {
            (Some(0), Some(path)) => format!("No rules yet — edit {}", abbreviate(path)),
            (Some(count), Some(path)) => {
                let plural = if count == 1 { "rule" } else { "rules" };
                format!("{count} {plural} in {}", abbreviate(path))
            }
            (_, Some(path)) => format!("Edit {} to add your own rules", abbreviate(path)),
            (_, None) => "Your own filtering rules".to_owned(),
        });

        let mut section = Section::new(group);
        section.group.add(&row);
        // The subtitle here is a path and a count rather than a description, so
        // the row is registered under the group's name too — "your rules" finds
        // it, which is what it is called on screen.
        section.add_row(&row, haystack(user_rules, "Your rules"));

        section
    }

    fn row(self: &Rc<Self>, filter: &Filter) -> adw::SwitchRow {
        let switch = adw::SwitchRow::new();

        // Filter names and descriptions are data, not markup: filter 216 is
        // literally "Official Polish filters for AdBlock, uBlock Origin &
        // AdGuard", and 251's description quotes '$' and '&'.
        //
        // This must precede the title. `AdwPreferencesRow:use-markup` defaults
        // to true and the label is rendered the moment the title is assigned,
        // so passing a title to the builder warns and mangles the text however
        // the property is set afterwards. Turning markup off first covers the
        // subtitle too.
        switch.set_use_markup(false);
        // `display_name`, not `name`: a custom list installed without a
        // `! Title:` header has an empty title and no localisation rows, so the
        // catalogue's whole fallback chain resolves to "" and the row would
        // render nameless (contract §6).
        switch.set_title(filter.display_name());

        if !filter.description.is_empty() {
            switch.set_subtitle(&filter.description);
            switch.set_subtitle_lines(2);
        }

        // A row that can be trusted has a subtitle in at least one of its two
        // states, so the line count is set whether or not there is a
        // description for it to displace.
        let trust = filter.supports_trust(self.set).then(|| {
            switch.set_subtitle_lines(2);
            self.trust_control(filter, &switch)
        });

        // Before the handler is connected, so the initial state is not read as
        // a click.
        switch.set_active(filter.enabled);

        let id = filter.id;
        let this = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.reconciling.get() {
                return; // our own write, not a click
            }
            this.toggle(id, switch.is_active());
        });

        self.rows.borrow_mut().insert(
            filter.id,
            Row {
                switch: switch.clone(),
                filter: RefCell::new(filter.clone()),
                trust,
            },
        );

        // The control is built blank and painted from the record, so there is
        // one function deciding what a trusted row looks like and the first
        // paint cannot drift from every later one.
        self.paint_trust(id);

        switch
    }

    /// Build the control that grants and withdraws trust, blank.
    ///
    /// **A plain `GtkButton`, not a `GtkToggleButton`**, and that is a property
    /// of the design rather than a preference. A toggle's `active` moves under
    /// the user's finger, so the row would read *trusted* for the length of a
    /// dialog they may then cancel — a state the database has never confirmed,
    /// which is the one thing this page does not do. It would also emit
    /// `toggled` on our own writes and so need its own copy of the
    /// [`reconciling`] guard, where a button has no state property to write:
    /// `set_icon_name`, `set_tooltip_text`, `update_property` and the CSS
    /// classes all emit nothing.
    ///
    /// A suffix button rather than a gesture, for the reason the removal button
    /// is one: the row's activatable widget is the switch, so a swipe, a long
    /// press or a row click is reached by the same motion that toggles the
    /// list. That argument had two outcomes to keep apart and now has three —
    /// off, gone, and trusted — so they get three shapes at three positions.
    ///
    /// It never borrows `.destructive-action`. Red on this page means the one
    /// control that destroys something, and making a grant look like a deletion
    /// is exactly the confusion being designed against.
    ///
    /// [`reconciling`]: Self::reconciling
    fn trust_control(self: &Rc<Self>, filter: &Filter, switch: &adw::SwitchRow) -> Trust {
        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(false);
        switch.add_prefix(&caveat);

        let button = gtk::Button::new();
        button.set_valign(gtk::Align::Center);
        button.add_css_class("flat");
        // Added before the removal button by construction — `row` runs before
        // `custom_group` adds that one — so the row reads switch, trust, trash,
        // with the destructive control last.
        switch.add_suffix(&button);

        let id = filter.id;
        let this = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.toggle_trust(id);
        });

        Trust { button, caveat }
    }

    /// Show one row's trust, in all five places it is said at once — the glyph,
    /// its colour, the tooltip, the accessible label and the subtitle.
    ///
    /// Read from the row's recorded filter — the last state the *database*
    /// confirmed — never from what a click asked for.
    ///
    /// **It renders a known state and has no way to render an unknown one**,
    /// which is why [`settle_trust`] does not call it when the verifying read
    /// failed: the record would then be a pre-click value, and painting it
    /// would be this page asserting the safe-looking answer about the one
    /// setting where that is the dangerous direction to be wrong in.
    ///
    /// [`settle_trust`]: Self::settle_trust
    ///
    /// The accessible label is repainted with the icon deliberately: a stale
    /// *"Trust X"* on a list that is already trusted is a lie told to precisely
    /// the users who cannot see the glyph that would have corrected it.
    fn paint_trust(&self, filter_id: i64) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(&filter_id) else {
            return;
        };
        let Some(trust) = &row.trust else {
            return;
        };

        let filter = row.filter.borrow();
        let trusted = filter.trusted;

        trust.button.set_icon_name(if trusted {
            "changes-allow-symbolic"
        } else {
            "changes-prevent-symbolic"
        });
        // `.warning` is a plain colour class — measured against the installed
        // stylesheet, it sets `color` and neither `button` nor `button.flat`
        // sets one, so the symbolic follows it. Amber rather than red, which
        // belongs to the removal button.
        if trusted {
            trust.button.add_css_class("warning");
        } else {
            trust.button.remove_css_class("warning");
        }
        trust.button.set_tooltip_text(Some(if trusted {
            "Withdraw trust from this list"
        } else {
            "Trust this list"
        }));
        trust.button.update_property(&[gtk::accessible::Property::Label(&if trusted {
            format!("Withdraw trust from {}", filter.display_name())
        } else {
            format!("Trust {}", filter.display_name())
        })]);
        trust.caveat.set_visible(trusted);
        row.switch.set_subtitle(if trusted {
            TRUSTED_SUBTITLE
        } else {
            &filter.description
        });
    }

    /// Send one switch flip to the CLI, then confirm it against the database.
    ///
    /// A list from the Annoyances group takes the long way round: AdGuard will
    /// not switch one on without an agreement, so the agreement is asked for
    /// here, before anything is run. Asking *afterwards* was the tempting
    /// shape and is wrong — `filters add` subscribes to the list and only then
    /// refuses to enable it, so a declined dialog would leave behind a
    /// subscription the user never got.
    fn toggle(self: &Rc<Self>, filter_id: i64, on: bool) {
        let (action, name, needs_consent) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(&filter_id) else {
                return;
            };
            // Insensitive until the database has spoken, so a second click
            // cannot race the first one's verification.
            row.switch.set_sensitive(false);
            let filter = row.filter.borrow();
            let action = filter.action_for(on);
            (
                action,
                filter.display_name().to_owned(),
                filter.needs_annoyance_consent(self.set, action),
            )
        };

        if needs_consent {
            let this = self.clone();
            glib::spawn_future_local(async move {
                if this.confirm_annoyances().await {
                    this.apply(filter_id, on, action, name, Consent::Granted);
                } else {
                    this.abandon(filter_id);
                }
            });
            return;
        }

        self.apply(filter_id, on, action, name, Consent::Withheld);
    }

    /// Show AdGuard's annoyance-filter agreement and answer it for the CLI.
    ///
    /// The body is [`ANNOYANCE_TERMS`] verbatim, because the point of the
    /// dialog is that the user agrees to the same thing the CLI is about to ask
    /// about — and what it says they are agreeing to is that they, not AdGuard,
    /// answer for breaking a website's terms of use. Summarising that would be
    /// this application deciding how much of a disclaimer someone needs to see.
    ///
    /// Cancel is the default and the escape route: closing the dialog with
    /// Escape must not read as consent.
    async fn confirm_annoyances(&self) -> bool {
        let dialog =
            adw::AlertDialog::new(Some("Enable annoyance filters?"), Some(ANNOYANCE_TERMS));
        // Not markup: AdGuard's text is prose, and prose carries `&`. The same
        // rule every row, toast and dialog on this page follows.
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("enable", "I Agree");
        dialog.set_response_appearance("enable", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "enable"
    }

    /// Put a switch back where it was, for a flip that was never sent.
    ///
    /// The row's recorded `filter` is the last state the *database* confirmed,
    /// so this reverts to observed reality rather than to the negation of what
    /// was clicked.
    fn abandon(&self, filter_id: i64) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(&filter_id) else {
            return;
        };
        row.switch.set_sensitive(true);
        let enabled = row.filter.borrow().enabled;
        self.set_active(&row.switch, enabled);
    }

    /// act -> re-read -> reconcile, once the decision to act has been made.
    fn apply(
        self: &Rc<Self>,
        filter_id: i64,
        on: bool,
        action: FilterAction,
        name: String,
        consent: Consent,
    ) {
        let cli = self.cli.clone();
        let set = self.set;
        let this = self.clone();
        worker::run(
            move || {
                let refused = cli
                    .filter_action(set, action, filter_id, consent)
                    .err()
                    .map(|err| err.to_string());
                // Verify from the database. A CLI that printed a confirmation
                // is not proof, and one that complained is not disproof.
                let state = Catalogue::open_set(set)
                    .ok()
                    .and_then(|catalogue| catalogue.state(filter_id).ok().flatten());
                (refused, state)
            },
            move |(refused, state)| this.settle(filter_id, on, &name, refused, state),
        );
    }

    /// Reconcile one row against what the database now says.
    fn settle(
        &self,
        filter_id: i64,
        requested: bool,
        name: &str,
        refused: Option<String>,
        state: Option<FilterState>,
    ) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(&filter_id) else {
            return;
        };
        row.switch.set_sensitive(true);

        if let Some(state) = state {
            {
                let mut filter = row.filter.borrow_mut();
                filter.enabled = state.enabled;
                filter.installed = state.installed;
                // Not written by anything a switch flip does — but the read
                // that verifies the flip carries it, and dropping it here would
                // let the row's record of its own trust go stale every time the
                // list was switched. A second window is one invocation away.
                filter.trusted = state.trusted;
            }
            self.set_active(&row.switch, state.enabled);
            // Cheap, and it is what keeps a trust changed elsewhere from
            // surviving on screen until the page is rebuilt.
            self.paint_trust(filter_id);

            // The switch is where the user asked it to be; whether the CLI was
            // chatty about it is not interesting.
            if state.enabled == requested {
                return;
            }
        }

        let message = refused.unwrap_or_else(|| {
            let verb = if requested { "enable" } else { "disable" };
            format!("Could not {verb} {name}")
        });
        self.toasts.add_toast(toast(&message));
    }

    /// Turn one list's trust the other way, asking first if that way is *on*.
    ///
    /// The two directions are not symmetrical and the control does not pretend
    /// they are. Granting trust lets a third party run script in the user's
    /// pages and is confirmed; withdrawing it takes that away and is issued
    /// straight away, because a dialog in front of the safe direction is a
    /// dialog the user learns to click through before reaching the one that
    /// matters.
    ///
    /// The target is `!trusted` read from the row's record — the database's
    /// last word — rather than from anything the widget shows, for the same
    /// reason `toggle` reads `action_for` from there.
    fn toggle_trust(self: &Rc<Self>, filter_id: i64) {
        let filter = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(&filter_id) else {
                return;
            };
            // Insensitive from the click rather than from the answer, and the
            // whole row rather than the button: it greys the switch and the
            // removal button with it, so nothing else can be started against a
            // filter whose trust is already in flight, and a second click
            // cannot open a second dialog.
            row.switch.set_sensitive(false);
            // Bound rather than returned directly, so the `Ref` is dropped
            // inside the block and not after `rows` at the end of it.
            let filter = row.filter.borrow().clone();
            filter
        };

        let name = filter.display_name().to_owned();
        if filter.trusted {
            self.apply_trust(filter_id, false, name);
            return;
        }

        let this = self.clone();
        glib::spawn_future_local(async move {
            if this.confirm_trust(&filter).await {
                this.apply_trust(filter_id, true, name);
            } else if let Some(row) = this.rows.borrow().get(&filter_id) {
                // Nothing was sent and nothing was painted, so the row is the
                // only thing there is to put back.
                row.switch.set_sensitive(true);
            }
        });
    }

    /// Ask before letting a list run script in the pages the user visits.
    ///
    /// Shaped like [`confirm_removal`], and it names the URL for the same
    /// reason: a list installed without a `! Title:` header has no name of its
    /// own, and the URL is what the grant actually attaches to.
    ///
    /// Three sentences, one fact each — what the list may do, that the grant
    /// outlives whatever rules were inspected before giving it, and **when it
    /// takes effect**. It claims nothing beyond what those rule types are: this
    /// application's warnings are measurements, and speculating about what a
    /// hostile list would *do* with the privilege would be the one dialog here
    /// that argues rather than reports.
    ///
    /// The third sentence used to read *"can be withdrawn at any time"*, which
    /// was written before anyone had measured it and is **false in the
    /// direction that matters**. Measured 6 August 2026 (contract §6): the
    /// proxy reads the flag when it starts and not again, so a withdrawal
    /// leaves the list's scriptlets running until the next restart. A dialog
    /// that reassures the user about taking a privilege back must not describe
    /// a control that does not act when they use it.
    ///
    /// `Destructive` rather than `Suggested`, though nothing is destroyed.
    /// `Suggested` is Adwaita's *recommended answer*, and this application does
    /// not recommend granting a third party script execution; the plain default
    /// would make the dangerous answer look exactly like Cancel. That trust can
    /// be withdrawn later does not make it harmless — withdrawing it does not
    /// unrun the scriptlets that ran meanwhile.
    ///
    /// [`confirm_removal`]: Self::confirm_removal
    async fn confirm_trust(&self, filter: &Filter) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("Trust this filter list?"),
            Some(&format!(
                "{} will be allowed to run scriptlets in the pages you visit — script \
                 chosen by whoever writes the list, executing alongside the page's \
                 own.\n\nTrust attaches to the subscription, not to the rules it holds \
                 today: {} is re-fetched as it updates, and what arrives next is \
                 trusted too.\n\nAdGuard reads this when the proxy starts. Granting it \
                 takes effect at the next restart — and so does taking it back, which \
                 means trust cannot be withdrawn from a list while the proxy is \
                 running.",
                filter.display_name(),
                filter.download_url,
            )),
        );
        // Not markup: a list's title is AdGuard's text or the user's and both
        // can carry `&`, a URL carries one in any query string, and the body
        // itself carries `$$`. The same rule every row, toast and dialog on
        // this page follows.
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("trust", "Trust");
        dialog.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
        // Cancel is the default and the escape route: dismissing a dialog must
        // never be capable of granting a privilege.
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "trust"
    }

    /// act -> re-read -> reconcile, for trust.
    ///
    /// The re-read is issued whatever the CLI returned, including after a
    /// timeout — that error means only that we stopped waiting, not that
    /// nothing happened — and including after a refusal, since a refusal is not
    /// disproof any more than a confirmation is proof. `set-trusted` prints the
    /// same success line for a no-op (contract §6), so the database is the
    /// whole of the verification.
    fn apply_trust(self: &Rc<Self>, filter_id: i64, trusted: bool, name: String) {
        let cli = self.cli.clone();
        let set = self.set;
        let this = self.clone();
        worker::run(
            move || {
                let refused = cli
                    .filters_set_trusted(filter_id, trusted)
                    .err()
                    .map(|err| err.to_string());
                let state = Catalogue::open_set(set)
                    .ok()
                    .and_then(|catalogue| catalogue.state(filter_id).ok().flatten());
                // Whether the change has actually reached anything. The proxy
                // reads this flag at start and not again (contract §6), so a
                // change made while it is up is written and not yet in force —
                // and nothing else in the app would ever say so, because unlike
                // `config set` the CLI does not report it. Asked here rather
                // than on the main thread, and only after the write, so it
                // cannot contend with it (contract §3).
                let running = cli.status().map(|status| status.running).unwrap_or(false);
                (refused, state, running)
            },
            move |(refused, state, running)| {
                this.settle_trust(filter_id, trusted, &name, refused, state, running)
            },
        );
    }

    /// Reconcile one row's trust against what the database now says.
    fn settle_trust(
        self: &Rc<Self>,
        filter_id: i64,
        requested: bool,
        name: &str,
        refused: Option<String>,
        state: Option<FilterState>,
        running: bool,
    ) {
        {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(&filter_id) else {
                return;
            };
            row.switch.set_sensitive(true);

            if let Some(state) = state {
                {
                    let mut filter = row.filter.borrow_mut();
                    filter.enabled = state.enabled;
                    filter.installed = state.installed;
                    filter.trusted = state.trusted;
                }
                // The re-read carries all three flags, so it can have caught a
                // switch that moved for a reason of its own — this page is not
                // the only writer, and `auto_enable_language_filters` does
                // exactly that. Through `set_active`, or the reconcile would be
                // replayed to the CLI as if the user had clicked it.
                self.set_active(&row.switch, state.enabled);
            }
        }

        // The catalogue could not be read, so nothing here knows whether the
        // change took — and **this is the one control whose two answers are not
        // equally safe to guess at**. Repainting from the row's record would put
        // the safe-looking one on screen: in the granting direction that record
        // still reads untrusted, and an untrusted row is deliberately unmarked,
        // so a list AdGuard may now be treating as trusted would render exactly
        // like one that never was — the "off is not unknown" rule
        // (`architecture.md` §5) failing in its dangerous direction.
        //
        // There is no per-row way to say *unknown* here, so the page says it
        // instead, exactly as `settle_removal` does for the same unreadable
        // catalogue: reload, which re-reads — and if the read fails again the
        // page shows *Filters unavailable*, which is the honest answer and the
        // one no single row can render.
        let Some(state) = state else {
            self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                format!("Could not re-read the catalogue to confirm whether {name} is trusted")
            })));
            self.reload();
            return;
        };

        self.paint_trust(filter_id);

        if state.trusted == requested {
            // The control is where the user asked it to be, and the row already
            // says so wherever it says anything — so the only thing left worth
            // saying is the thing the row *cannot* show: that the flag is
            // written and not yet in force. The proxy reads it at start and not
            // again (contract §6, measured both ways), so while it is up this
            // is the same class of fact `config set` reports for itself on the
            // Protection and Advanced pages, and it gets their sentence.
            //
            // The withdrawing direction is why this is not optional. A user who
            // takes trust back from a list is doing the one thing here that is
            // urgent, and until the proxy restarts that list's scriptlets are
            // still running in their pages.
            if running {
                self.toasts
                    .add_toast(toast("Restart the proxy to apply this change"));
            }
            return;
        }

        // The CLI's own wording first, wherever there is one: `Filter not
        // found`, from a list another window has already removed, says more
        // than we would, and a lapsed licence explains itself better than a
        // sentence of ours could.
        let message = refused.unwrap_or_else(|| {
            if requested {
                format!("Could not trust {name}")
            } else {
                format!("{name} is still trusted — AdGuard did not clear it")
            }
        });
        self.toasts.add_toast(toast(&message));
    }

    /// Move a switch without the change reading as a user action.
    fn set_active(&self, switch: &adw::SwitchRow, active: bool) {
        self.reconciling.set(true);
        switch.set_active(active);
        self.reconciling.set(false);
    }
}

/// The two things the page can be showing under the search field.
const RESULTS: &str = "results";
const NO_RESULTS: &str = "no-results";

/// Shown instead of a page of hidden groups, which would otherwise read as a
/// catalogue that had failed to load.
fn no_results_view() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("No filter lists found")
        .description("No list matches that search. Try a shorter one.")
        .build()
}

fn loading_view() -> adw::Spinner {
    adw::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build()
}

fn error_view(message: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Filters unavailable")
        .description(message)
        .build()
}
