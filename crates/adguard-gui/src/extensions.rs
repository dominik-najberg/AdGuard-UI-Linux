//! The Extensions page: one row per installed userscript.
//!
//! State is read from the `userscripts/` directory and `proxy.yaml` together —
//! the directory says what is installed, the config says what is switched on —
//! and written through `adguard-cli userscripts`. Every toggle follows
//! act -> re-read -> reconcile, exactly as the Filters page does: the CLI
//! reports semantic failures at exit 0, so a switch is only allowed to settle
//! on a state the files confirm.
//!
//! See `docs/cli-contract.md` §15 for the measured behaviour behind all of it,
//! and `architecture.md` §7 for why this page exists at all.
//!
//! # The one row that cannot be used
//!
//! `enable`, `disable` and `remove` match a case-insensitive substring against
//! every installed script's id *and* title, with no exact-match flag. So a
//! script whose id is contained in another's is unreachable — the exact id is
//! refused — and no argument this page could construct would get past it.
//!
//! That is an upstream boundary rather than a gap here, and the page treats it
//! the way `architecture.md` §6 treats the certificate and root-helper checks:
//! it detects the condition, says so on the row in plain words, and declines to
//! offer a control that would fail at exit 0. [`adguard_core::Userscript::ambiguous`]
//! is where the condition is computed.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use adguard_core::script_update::{self, Check};
use adguard_core::userscript_state::{Aliases, Carried, Copies, Pending};
use adguard_core::userscripts::{self, Helper};
use adguard_core::{Cli, Config, Locale, Userscript, INSTALL_LINKS};
use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use adguard_core::preview;

use crate::install_link::{self, Details, Reach, Summary};
use crate::{toast, worker};

/// One read of everything the page renders.
struct Loaded {
    scripts: Vec<Userscript>,
    /// AdGuard's own scripts that are not installed yet. Derived from
    /// `scripts`, but computed on the worker thread with everything else so the
    /// view builder has nothing left to work out.
    offered: Vec<&'static adguard_core::Recommended>,
    /// Where this application's own script for browser install links stands.
    install_links: Helper,
    /// Kept edits an update replaced, by id ([`Copies::pending`]).
    pending: HashMap<String, Pending>,
    /// The scripts whose source names no update address, switched on or not
    /// ([`adguard_core::update_crash`]): `check-update` crashes on one that is.
    bare: HashSet<String>,
}

/// What a row knows beyond AdGuard's files and the copies' *Edited*: where an
/// update stands, and whether `check-update` would crash on it.
#[derive(Debug, Clone, Default)]
struct Extra {
    /// A newer version the last check found. Shown only while it is newer
    /// than what is installed ([`Row::available`]), so an update that lands by
    /// any route clears it without the check running again.
    available: Option<String>,
    pending: Option<Pending>,
    bare: bool,
}

/// A rendered userscript and the state it was rendered from.
struct Row {
    switch: adw::SwitchRow,
    /// Last state the files confirmed. Every decision is taken against this
    /// rather than against what the switch happens to be showing.
    script: RefCell<Userscript>,
    /// The trash, held so it can be greyed out with the switch while something
    /// else is in flight against this script.
    remove: gtk::Button,
    /// The cog.
    menu: gtk::MenuButton,
    /// The *Edited* note beside the cog, and the cog's *Revert to original*:
    /// shown together, whenever the source differs from AdGuard's copy, and
    /// moved without a rebuild when an editor saves ([`ExtensionsPage::watch`]).
    edited: gtk::Box,
    revert: gtk::Button,
    /// The warning glyph before the name: an ambiguous row, or one
    /// `check-update` crashes on.
    caveat: gtk::Image,
    /// The cog's *Update to x.y*, shown while a newer version is known.
    update: gtk::Button,
    /// The cog's *Bring back your changes…*, shown while an update has
    /// replaced a kept edit.
    restore: gtk::Button,
    extra: RefCell<Extra>,
}

impl Row {
    /// Grey out, or restore, everything on this row that writes.
    ///
    /// One place rather than three call sites, because a row that is half
    /// fenced is worse than one that is not fenced at all: the switch would be
    /// safe and the trash would not.
    ///
    /// An ambiguous row's switch and trash stay out whatever this is told:
    /// they cannot work for it at all, and finishing some other action on the
    /// row must not hand them back.
    fn set_busy(&self, busy: bool) {
        let actionable = self.script.borrow().actionable();
        // The row as a whole: on an ambiguous row the switch inside it stays
        // out on its own ([`ExtensionsPage::row`]), so this cannot hand it
        // back, and the cog beside it stays usable.
        self.switch.set_sensitive(!busy);
        self.remove.set_sensitive(!busy && actionable);
        self.menu.set_sensitive(!busy);
    }

    /// Paint the name, from the last state on record.
    fn show_name(&self) {
        let script = self.script.borrow();
        self.switch.set_title(script.display_name());
        // The original name stays one hover away: it is what AdGuard, its
        // logs and `adguard-cli userscripts list` go on calling the script.
        let original = script
            .alias
            .is_some()
            .then(|| format!("Originally “{}”", script.original_name()));
        self.switch.set_tooltip_text(original.as_deref());
    }

    /// Show or hide everything that says the source was edited.
    fn show_edited(&self, edited: bool) {
        self.script.borrow_mut().edited = edited;
        self.edited.set_visible(edited);
        self.revert.set_visible(edited);
    }

    /// The newer version on offer, if the last check found one and it is still
    /// newer than what is installed.
    fn available(&self) -> Option<String> {
        let installed = self.script.borrow().version.clone()?;
        self.extra
            .borrow()
            .available
            .clone()
            .filter(|offered| script_update::compare(offered, &installed) == std::cmp::Ordering::Greater)
    }

    /// Paint the subtitle, the caveat and the update entries from the last
    /// state on record — after a check, a toggle, or a save in an editor.
    fn paint(&self) {
        let available = self.available();
        let script = self.script.borrow();
        let extra = self.extra.borrow();
        self.switch
            .set_subtitle(&row_subtitle(&script, available.as_deref(), extra.pending, extra.bare));
        self.caveat.set_visible(script.ambiguous || (extra.bare && script.enabled));
        // Not on an ambiguous row, for *Reinstall*'s reason: an update is a
        // reinstall, and it switches the script on.
        let can_update = script.download_url.is_some() && script.actionable();
        match available.filter(|_| can_update) {
            Some(version) => {
                set_menu_label(&self.update, &format!("Update to {version}"));
                self.update.set_visible(true);
            }
            None => self.update.set_visible(false),
        }
        self.restore.set_visible(extra.pending.is_some());
    }
}

/// What an unreachable row says instead of its description.
///
/// It **displaces** the script's own description rather than joining it, for
/// the reason the Filters page displaces one with its trusted caveat: two
/// sentences sharing a two-line ellipsised subtitle leave whichever came second
/// truncated, and the fact that a control does nothing outranks the script's
/// account of itself.
///
/// It no longer says *rename* the other one. The cog now has a *Rename…*
/// that changes only what this application shows, and the CLI's substring
/// match never sees it — so pointing at renaming here would send the user to
/// a control that cannot fix this.
const AMBIGUOUS_SUBTITLE: &str =
    "AdGuard cannot tell this apart from another installed script, so it cannot be \
     switched or removed until the other one is removed";

/// What the add row's group says above the field.
///
/// It names the scheme because `userscripts install` refuses everything else —
/// measured, a local path and a `file://` URL are both rejected with the same
/// unhelpful sentence, which explains none of that (contract §15). Saying so
/// here is cheaper than letting a user discover it by having a paste fail.
///
/// In the group description rather than as a placeholder on the field:
/// `AdwEntryRow` has no `placeholder-text` property, and reaching for one
/// through `set_property` panics at run time rather than failing to compile.
/// No other page here sets a placeholder either.
const ADD_DESCRIPTION: &str =
    "AdGuard fetches userscripts over the web, so this takes an http or https address \
     ending in .user.js — a file on this computer cannot be installed.";

/// What the *From your browser* group says while the helper is installed but
/// switched off — the state that, on 11 October 2026, left *Install Script* on
/// a GitHub page doing nothing, with nothing anywhere saying why.
const HELPER_OFF: &str = "The browser helper is installed but switched off, so the Install \
                          button on Greasy Fork, Sleazy Fork, OpenUserJS and GitHub opens the \
                          script as text instead of bringing it here.";

/// What the banner says after an address was pasted while the browser helper
/// is missing or switched off (#29). It names the button people already know from Greasy Fork
/// rather than the helper, whose name means nothing until it is explained.
const HELPER_HINT: &str = "You can skip copying addresses: with the browser helper, the Install \
                           button on Greasy Fork brings scripts here.";

pub struct ExtensionsPage {
    /// The banner above the page, then the page.
    root: gtk::Box,
    /// The browser-helper suggestion. Outside `bin` so a rebuild of the page —
    /// which every install triggers — does not take it down with it.
    banner: adw::Banner,
    /// Swapped wholesale — spinner, error, or the list — which is simpler and
    /// less error-prone than reconciling child lists.
    bin: adw::Bin,
    /// The *From your browser* group in the current build, for the banner to
    /// point at.
    browser_group: RefCell<Option<adw::PreferencesGroup>>,
    /// That group's *Turn on*, while the helper is installed: the group is
    /// built either way and shown only while the helper is off, so a switch
    /// flipped on the helper's own row moves it without a rebuild.
    helper_turn_on: RefCell<Option<gtk::Button>>,
    cli: Cli,
    toasts: adw::ToastOverlay,
    locale: Locale,
    rows: RefCell<HashMap<String, Row>>,
    /// Set while we write switch states ourselves, so the `active` handler can
    /// tell a user's click from our own reconcile. Property notifications are
    /// synchronous, so a plain flag around the write is enough.
    reconciling: Cell<bool>,
    /// Install links waiting for their dialog, oldest first.
    offers: RefCell<VecDeque<String>>,
    /// The link whose dialog — or install — is under way, if any.
    asking: RefCell<Option<String>>,
    /// Whether this run has already suggested the browser helper.
    hinted: Cell<bool>,
    /// Watches AdGuard's `userscripts/` directory for a save from an editor
    /// ([`Self::watch`]). Held, because a monitor nobody holds is dropped, and
    /// a dropped monitor stops reporting.
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// The pending re-check after a burst of changes in that directory.
    settle_edits: RefCell<Option<glib::SourceId>>,
    /// The ids whose source changed in that burst: the ones to have AdGuard
    /// load again once it is over.
    touched: RefCell<HashSet<String>>,
    /// How many installs and reinstalls of ours are fetching right now
    /// ([`Fetching`]). While any is, the re-check waits: AdGuard writes the
    /// fresh source before the fetch's own worker records it as pristine, and
    /// a comparison in between would file AdGuard's bytes as the user's edit,
    /// overwriting the one that was kept.
    fetching: Rc<Cell<u32>>,
    /// Newer versions the last update check found, by id. Kept across
    /// rebuilds; [`Row::available`] drops one that is no longer newer.
    updates: RefCell<HashMap<String, String>>,
    /// The last read's [`Loaded::pending`] and [`Loaded::bare`], for the rows
    /// built from it.
    pending: RefCell<HashMap<String, Pending>>,
    bare: RefCell<HashSet<String>>,
    /// When the last update check finished, as the local time, and how many
    /// scripts it could not ask.
    checked: RefCell<Option<(String, usize)>>,
    /// An update check is running.
    checking: Cell<bool>,
    /// Whether this run has checked at all: the first build of the page does,
    /// once, and the daily timer after that.
    ever_checked: Cell<bool>,
    /// The *Installed* group's header: when it last checked, and *Check now*.
    check_label: RefCell<Option<gtk::Label>>,
    check_button: RefCell<Option<gtk::Button>>,
}

/// One install or reinstall of ours in flight; see
/// [`ExtensionsPage::fetching`]. Counted down when dropped, on every path.
struct Fetching(Rc<Cell<u32>>);

impl Fetching {
    fn start(count: &Rc<Cell<u32>>) -> Self {
        count.set(count.get() + 1);
        Self(count.clone())
    }
}

impl Drop for Fetching {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl ExtensionsPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let banner = adw::Banner::builder()
            .title(HELPER_HINT)
            .button_label("Show")
            .use_markup(false)
            .revealed(false)
            .build();
        let bin = adw::Bin::builder().vexpand(true).build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner);
        root.append(&bin);

        let this = Rc::new(Self {
            root,
            banner,
            bin,
            browser_group: RefCell::new(None),
            helper_turn_on: RefCell::new(None),
            cli,
            toasts,
            locale: Locale::from_env(),
            rows: RefCell::new(HashMap::new()),
            reconciling: Cell::new(false),
            offers: RefCell::new(VecDeque::new()),
            asking: RefCell::new(None),
            hinted: Cell::new(false),
            monitor: RefCell::new(None),
            settle_edits: RefCell::new(None),
            touched: RefCell::new(HashSet::new()),
            fetching: Rc::new(Cell::new(0)),
            updates: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
            bare: RefCell::new(HashSet::new()),
            checked: RefCell::new(None),
            checking: Cell::new(false),
            ever_checked: Cell::new(false),
            check_label: RefCell::new(None),
            check_button: RefCell::new(None),
        });

        // *Show* rather than an install from the banner: the group it points
        // at says what the helper does, and adding it is that group's button.
        let page = Rc::downgrade(&this);
        this.banner.connect_button_clicked(move |banner| {
            banner.set_revealed(false);
            let Some(page) = page.upgrade() else {
                return;
            };
            let group = page.browser_group.borrow().clone();
            if let Some(group) = group {
                crate::reveal(&group);
            }
        });

        // Once a day while the application runs, badges only: the check
        // installs nothing. The first one is the first build's (`reload`).
        let daily = Rc::downgrade(&this);
        glib::timeout_add_seconds_local(DAILY, move || match daily.upgrade() {
            Some(page) => {
                page.check_updates(false);
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

        this.watch();
        this.reload();
        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Re-read both sources and rebuild the page.
    ///
    /// Used for the initial load, the explicit refresh, and after anything that
    /// adds or removes a row. Individual toggles do **not** come through here —
    /// they patch the one row they touched, so flipping a switch does not throw
    /// away the scroll position.
    pub fn reload(self: &Rc<Self>) {
        self.bin.set_child(Some(&loading_view()));

        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || read(&locale),
            move |result: Result<Loaded, String>| match result {
                Ok(loaded) => {
                    let view = this.view(&loaded);
                    this.bin.set_child(Some(&view));
                    if !this.ever_checked.replace(true) {
                        this.check_updates(false);
                    }
                }
                Err(err) => {
                    this.rows.borrow_mut().clear();
                    this.browser_group.replace(None);
                    this.helper_turn_on.replace(None);
                    this.bin.set_child(Some(&error_view(&err)));
                }
            },
        );
    }

    /// Repaint the switches from a `proxy.yaml` that changed underneath us.
    ///
    /// Called by [`crate::watch`], and it is not optional polish: enabled state
    /// *lives* in that file, so `adguard-cli userscripts disable` typed in a
    /// terminal — or a second window — moves exactly what this page renders.
    ///
    /// Returns how many rows moved, which is what the watcher gates its toast
    /// on: a rewrite that changed nothing the user can see should not announce
    /// itself (see `watch.rs`).
    ///
    /// A script appearing or disappearing cannot be patched — there is no row
    /// to move — so that triggers a full [`reload`] instead and reports nothing,
    /// the rebuild being its own announcement.
    ///
    /// [`reload`]: Self::reload
    pub fn reconcile(self: &Rc<Self>, config: &Config) -> usize {
        let enabled: Vec<&str> = config.enabled_userscripts();

        // A `meta:` path naming a script this page has no row for means the set
        // of installed scripts moved, not just their states.
        let known = self.rows.borrow().len();
        let unknown = enabled.iter().any(|meta| {
            let stem = std::path::Path::new(meta.trim())
                .file_name()
                .map(|name| name.to_string_lossy().replace(".meta.json", ""));
            match stem {
                Some(id) => !self.rows.borrow().contains_key(&id),
                None => false,
            }
        });
        if unknown || known == 0 {
            self.reload();
            return 0;
        }

        let mut moved = 0;
        self.reconciling.set(true);
        for (id, row) in self.rows.borrow().iter() {
            let now = is_enabled(id, &enabled);
            if row.script.borrow().enabled != now {
                row.script.borrow_mut().enabled = now;
                row.switch.set_active(now);
                // The crash caveat follows the switch.
                row.paint();
                moved += 1;
            }
        }
        self.reconciling.set(false);
        self.show_helper_state();
        moved
    }

    /// The add row, then one group holding every script.
    fn view(self: &Rc<Self>, loaded: &Loaded) -> gtk::Widget {
        self.rows.borrow_mut().clear();
        self.pending.replace(loaded.pending.clone());
        self.bare.replace(loaded.bare.clone());
        self.check_label.replace(None);
        self.check_button.replace(None);

        let page = adw::PreferencesPage::new();
        page.add(&self.add_group());
        // Directly under the address field: the two are ways of doing the same
        // thing, and the helper is the one that saves typing the address.
        let browser_group = self.install_links_group(loaded.install_links);
        page.add(&browser_group);
        if loaded.install_links.works() {
            // Added and on, from the banner's group or anywhere else: nothing
            // left to suggest.
            self.banner.set_revealed(false);
        }
        self.browser_group.replace(Some(browser_group));
        if let Some(group) = self.offered_group(&loaded.offered) {
            page.add(&group);
        }

        if loaded.scripts.is_empty() {
            // Not an error: an install whose last script was removed is a
            // perfectly ordinary state, and the row above is what to do about
            // it. A `StatusPage` here would read as a failure.
            let empty = adw::PreferencesGroup::new();
            empty.add(&inert_row(
                "No userscripts installed",
                "Add one above to extend what AdGuard does on the pages you visit.",
            ));
            page.add(&empty);
            return page.upcast();
        }

        let group = adw::PreferencesGroup::builder()
            .title("Installed")
            .description(
                "Userscripts run inside the pages you visit. AdGuard injects the ones \
                 switched on here.",
            )
            .build();
        group.set_header_suffix(Some(&self.check_header()));
        for script in &loaded.scripts {
            group.add(&self.row(script));
        }
        page.add(&group);
        page.upcast()
    }

    /// The install-by-URL row.
    fn add_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Add an extension")
            .description(ADD_DESCRIPTION)
            .build();

        let entry = adw::EntryRow::builder()
            .title("Userscript URL")
            .show_apply_button(true)
            .build();
        // The one place this page shows a string the user is about to act on;
        // `&` in a URL is ordinary and markup would mangle it.
        entry.set_use_markup(false);

        let spinner = adw::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        entry.add_suffix(&spinner);

        let this = Rc::downgrade(self);
        entry.connect_apply(move |entry| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let url = entry.text().trim().to_owned();
            if url.is_empty() {
                return;
            }
            let field = Some((entry.clone(), spinner.clone()));
            glib::spawn_future_local(async move { this.install(url, field, None).await });
        });

        group.add(&entry);
        group
    }

    /// AdGuard's own userscripts, for the ones the user has not installed.
    ///
    /// `None` once all four are installed — the group would be a heading over
    /// nothing, and its absence is the useful signal that there is nothing left
    /// to add. The same reason `FilterCatalogue::grouped` drops empty groups.
    ///
    /// **Offered, never installed unsolicited.** These are AdGuard's own
    /// scripts and this is a shortcut past having to find their URLs — not a
    /// reason for the application to fetch script the user did not ask for
    /// (`architecture.md` §7).
    fn offered_group(
        self: &Rc<Self>,
        offered: &[&'static adguard_core::Recommended],
    ) -> Option<adw::PreferencesGroup> {
        if offered.is_empty() {
            return None;
        }

        let group = adw::PreferencesGroup::builder()
            .title("From AdGuard")
            .description(
                "AdGuard's own userscripts, which its Windows and Mac applications come with. \
                 AdGuard CLI ships only AdGuard Extra, so the rest are fetched on request.",
            )
            .build();

        for entry in offered {
            group.add(&self.offer_row(entry));
        }

        Some(group)
    }

    /// The browser-links script, until it is installed (#29).
    ///
    /// A group of its own rather than a fifth row under *From AdGuard*: it is
    /// this application's script, not AdGuard's, and it changes what happens
    /// on other sites when a link is clicked — which is worth a sentence of its
    /// own before anyone presses *Add*.
    fn install_links_group(self: &Rc<Self>, helper: Helper) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("From your browser").build();
        self.helper_turn_on.replace(None);

        let ambiguous = match helper {
            Helper::Missing(entry) => {
                group.set_description(Some(
                    "With this added, the Install button on Greasy Fork, Sleazy Fork, OpenUserJS \
                     and GitHub brings the script here to be added, instead of opening it as text. \
                     It needs HTTPS filtering for your browser, and the browser asks once whether \
                     to open AdGuard UI.",
                ));
                // Titled for what it does rather than with the script's own
                // name, which reads as a product nobody has heard of. The name
                // follows, so the row can be matched to the one that appears
                // under *Installed*.
                let row = self.offer_row(entry);
                row.set_title("Install from your browser");
                row.set_subtitle(&format!(
                    "Adds the “{}” userscript. Each script is still added only after you confirm it.",
                    entry.name
                ));
                group.add(&row);
                return group;
            }
            Helper::Off { ambiguous } => ambiguous,
            Helper::On => false,
        };

        // Installed: built either way, and shown only while it is off, so the
        // switch on the helper's own row below can move it without a rebuild.
        group.set_description(Some(HELPER_OFF));
        let row = adw::ActionRow::builder()
            .title("Install from your browser")
            .subtitle(format!(
                "“{}” is switched off. Each script is still added only after you confirm it.",
                INSTALL_LINKS.name
            ))
            .build();
        row.set_use_markup(false);
        row.set_subtitle_lines(2);
        // A helper AdGuard cannot be told to switch on by name (its id is
        // inside another script's) gets the explanation and no button, as its
        // own row does.
        if !ambiguous {
            let turn_on = gtk::Button::builder()
                .label("Turn on")
                .valign(gtk::Align::Center)
                .build();
            turn_on.add_css_class("suggested-action");
            let this = Rc::downgrade(self);
            turn_on.connect_clicked(move |button| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                button.set_sensitive(false);
                // Through the row's own path, so the switch below moves with
                // it and the files have the last word.
                this.toggle(INSTALL_LINKS.id, true);
            });
            row.add_suffix(&turn_on);
            self.helper_turn_on.replace(Some(turn_on));
        }
        group.add(&row);
        group.set_visible(!helper.works());
        group
    }

    /// Show the *From your browser* group exactly while the installed helper
    /// is switched off, from the state its row last settled on.
    ///
    /// Nothing to do while the helper is missing: then the group offers it
    /// for installing, and an install rebuilds the page.
    fn show_helper_state(&self) {
        let Some(on) = self
            .rows
            .borrow()
            .get(INSTALL_LINKS.id)
            .map(|row| row.script.borrow().enabled)
        else {
            return;
        };
        if let Some(group) = &*self.browser_group.borrow() {
            group.set_visible(!on);
        }
        if let Some(button) = &*self.helper_turn_on.borrow() {
            button.set_sensitive(true);
        }
        if on {
            self.banner.set_revealed(false);
        }
    }

    /// One script offered for installation: what it is, and *Add*.
    fn offer_row(self: &Rc<Self>, entry: &'static adguard_core::Recommended) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(entry.name)
            .subtitle(offered_subtitle(entry))
            .build();
        row.set_use_markup(false);
        row.set_subtitle_lines(2);

        let add = gtk::Button::builder()
            .label("Add")
            .valign(gtk::Align::Center)
            .build();
        add.add_css_class("suggested-action");

        let this = Rc::downgrade(self);
        add.connect_clicked(move |button| {
            let Some(this) = this.upgrade() else {
                return;
            };
            // Fenced at the click, like every other action here: the fetch
            // takes seconds and a second press would issue a second install.
            button.set_sensitive(false);
            this.add_recommended(entry);
        });

        add.set_tooltip_text(Some(&format!("Add {}", entry.name)));
        row.add_suffix(&add);
        // Deliberately **not** the row's activatable widget. Setting it
        // makes `AdwActionRow` give the button the row's own name — a walk
        // confirms it announcing as "AdGuard Popup Blocker", with the word
        // *Add* surviving only as a label inside it, so the one thing the
        // control does is the thing a screen reader would not say. And
        // pressing this downloads and runs somebody else's script, which
        // should take a press on the button rather than a click anywhere
        // along a row the user might merely be reading.
        //
        // The button's own text is what names it, and an explicit
        // accessible label does **not** override it — measured both before
        // and after the row is assembled, so it is the widget's rule and
        // not a question of ordering. That leaves several buttons all reading
        // "Add", distinguished by the list item each sits in; the tooltip
        // carries the script's name for the pointer, and unlike the trash
        // buttons on the rows below there is no unnamed control here to
        // fix.
        row
    }

    /// Install one of AdGuard's own scripts, in the state AdGuard ships it.
    ///
    /// Two commands for one button where the default is *off*: `install` always
    /// enables (contract §15), so *Assistant* and *Web of Trust* are installed
    /// and then switched off. Doing it here rather than leaving the user to
    /// notice is the whole point of the catalogue — arriving in the state
    /// AdGuard's other applications put them in.
    ///
    /// **A failed disable is not a failed install.** The script is on the
    /// machine either way, so the toast says what happened rather than
    /// pretending nothing did, and the row that appears will show it switched
    /// on — which is true, and is the state the user can then change.
    fn add_recommended(self: &Rc<Self>, entry: &'static adguard_core::Recommended) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || {
                if let Err(err) = cli.userscripts_install(entry.url) {
                    return Err(err.to_string());
                }
                if !entry.enabled_by_default {
                    if let Err(err) = cli.userscripts_disable(entry.id) {
                        return Ok(Some(err.to_string()));
                    }
                }
                Ok(None)
            },
            move |result: Result<Option<String>, String>| {
                match result {
                    Ok(None) => this
                        .toasts
                        .add_toast(toast(&format!("Added {}", entry.name))),
                    Ok(Some(why)) => this.toasts.add_toast(toast(&format!(
                        "Added {}, but it could not be switched off: {why}",
                        entry.name
                    ))),
                    // AdGuard's own sentence, which for an install says nothing
                    // about the cause.
                    Err(refused) => this.toasts.add_toast(toast(&refused)),
                }
                // Either way the page is rebuilt: a success moves the row from
                // the catalogue into the list, and a failure has to put the
                // button back.
                this.reload();
            },
        );
    }

    /// One script's row: a switch, what it is, and the controls that change it.
    fn row(self: &Rc<Self>, script: &Userscript) -> adw::SwitchRow {
        let switch = adw::SwitchRow::builder().active(script.enabled).build();
        // A userscript's name and description are written by whoever wrote the
        // script. `&` is ordinary in both, and `use-markup` defaults to true —
        // left on, Pango fails the whole string and the row renders mangled.
        // Off *before* either is set: given to the builder, they were parsed
        // as markup once on the way in, which is the warning a `&` in a Greasy
        // Fork name printed on every load. The title is [`Row::show_name`]'s.
        switch.set_use_markup(false);
        switch.set_subtitle(&subtitle(script));
        switch.set_subtitle_lines(2);

        // The same glyph the Protection, DNS and Filters rows use for a
        // consequence the row would not otherwise disclose. Undecorated for
        // accessibility, as those are: the subtitle says it in words, and
        // labelling the image would announce it twice.
        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(script.ambiguous);
        switch.add_prefix(&caveat);

        let edited = edited_note();
        let remove = self.remove_button(script);
        let Menu { menu, revert, update, restore } = self.menu_button(script);

        // Order matters: the trash goes last so it is furthest from the switch,
        // the two controls that must never be confused sitting at opposite ends
        // of the row's suffix area. The note sits before the cog, whose
        // *Revert to original* is what it is about.
        switch.add_suffix(&edited);
        switch.add_suffix(&menu);
        switch.add_suffix(&remove);

        if script.ambiguous {
            // The switch and the trash go through the CLI's substring match,
            // which cannot single this script out, so neither is offered. The
            // cog stays: renaming, editing and the links are this
            // application's and the browser's business, not the CLI's, and
            // its *Reinstall* is left out ([`Self::menu_button`]).
            //
            // The switch *inside* the row, not the row: an insensitive row
            // greys out every suffix in it, the cog included.
            if let Some(toggle) = switch.activatable_widget() {
                toggle.set_sensitive(false);
            }
            switch.set_activatable(false);
            remove.set_sensitive(false);
        }

        let this = Rc::downgrade(self);
        let id = script.id.clone();
        switch.connect_active_notify(move |switch| {
            let Some(this) = this.upgrade() else {
                return;
            };
            // Our own repaint, not a click.
            if this.reconciling.get() {
                return;
            }
            this.toggle(&id, switch.is_active());
        });

        let row = Row {
            switch: switch.clone(),
            script: RefCell::new(script.clone()),
            remove,
            menu,
            edited,
            revert,
            caveat,
            update,
            restore,
            extra: RefCell::new(Extra {
                available: self.updates.borrow().get(&script.id).cloned(),
                pending: self.pending.borrow().get(&script.id).copied(),
                bare: self.bare.borrow().contains(&script.id),
            }),
        };
        row.show_name();
        row.show_edited(script.edited);
        row.paint();
        self.rows.borrow_mut().insert(script.id.clone(), row);

        switch
    }

    /// The cog: where the script came from, what it is called, its source,
    /// and a reinstall. Returns the button and its *Revert to original*,
    /// which the row shows only while the source is edited.
    ///
    /// **Two links, because a homepage is not where a script came from.** The
    /// author's `@homepage` can be anything — for #9's Greasy Fork downloader
    /// it was the author's product site — while the user remembers the listing
    /// they installed it from. That page is worked out from the address AdGuard
    /// downloaded from ([`userscripts::script_page`]); for a host with no page
    /// to work out, the address itself is offered as *Installed from*. When
    /// the two would open the same address, only one is shown.
    ///
    /// *Storage*, which AdGuard for Windows also offers here, is out by
    /// decision — `architecture.md` §7. *Edit* is in, through the user's own
    /// editor rather than one of ours.
    fn menu_button(self: &Rc<Self>, script: &Userscript) -> Menu {
        let items = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let popover = gtk::Popover::builder().child(&items).build();

        // First, when there is one: the entry the row's subtitle points at.
        // Labelled with its version by [`Row::paint`], which also decides
        // whether it shows; its own separator goes with it.
        let update = menu_item("Update", None);
        update.add_css_class("accent");
        let update_rule = menu_separator();
        update.bind_property("visible", &update_rule, "visible").sync_create().build();
        items.append(&update);
        items.append(&update_rule);

        let source = script.download_url.as_deref().map(|url| {
            match userscripts::script_page(url) {
                Some(page) => ("Script page", page),
                None => ("Installed from", url.to_owned()),
            }
        });
        let homepage = script
            .homepage
            .clone()
            .filter(|home| source.as_ref().is_none_or(|(_, page)| !same_address(page, home)));
        let links = source
            .into_iter()
            .chain(homepage.map(|home| ("Author’s homepage", home)));
        let mut linked = false;
        for (label, uri) in links {
            let button = menu_item(label, userscripts::host(&uri).as_deref());
            button.set_tooltip_text(Some(&uri));
            let popover_ = popover.clone();
            button.connect_clicked(move |button| {
                popover_.popdown();
                // The launcher, not a hand-rolled `xdg-open`: it is what the
                // About page's links already use and it respects the portal.
                gtk::UriLauncher::new(&uri).launch(
                    button.root().and_downcast::<gtk::Window>().as_ref(),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            });
            items.append(&button);
            linked = true;
        }
        if linked {
            items.append(&menu_separator());
        }

        let rename = menu_item("Rename…", None);
        items.append(&rename);
        let edit = menu_item("Edit source…", None);
        items.append(&edit);
        let revert = menu_item("Revert to original…", None);
        items.append(&revert);
        let restore = menu_item("Bring back your changes…", None);
        items.append(&restore);

        // Not on an ambiguous row. A reinstall is an install by address, which
        // the substring trap does not reach — but it also switches the script
        // back on, and a switched-on script this page cannot switch off again
        // is the one outcome worse than not offering it.
        if script.download_url.is_some() && script.actionable() {
            items.append(&menu_separator());
            let reinstall = menu_item("Reinstall", None);
            let this = Rc::downgrade(self);
            let id = script.id.clone();
            let popover_ = popover.clone();
            reinstall.connect_clicked(move |_| {
                popover_.popdown();
                if let Some(this) = this.upgrade() {
                    this.begin_reinstall(&id);
                }
            });
            items.append(&reinstall);
        }

        for (button, action) in [
            (&rename, Self::begin_rename as fn(&Rc<Self>, &str)),
            (&edit, Self::edit_source),
            (&revert, Self::begin_revert),
            (&update, Self::begin_update),
            (&restore, Self::begin_restore),
        ] {
            let this = Rc::downgrade(self);
            let id = script.id.clone();
            let popover_ = popover.clone();
            button.connect_clicked(move |_| {
                popover_.popdown();
                if let Some(this) = this.upgrade() {
                    action(&this, &id);
                }
            });
        }

        let menu = gtk::MenuButton::builder()
            .icon_name("emblem-system-symbolic")
            .valign(gtk::Align::Center)
            .popover(&popover)
            .build();
        menu.add_css_class("flat");
        // The switch carries the row's name, so an icon button beside it would
        // otherwise reach the accessibility tree unnamed.
        menu.update_property(&[gtk::accessible::Property::Label(&format!(
            "More options for {}",
            script.display_name()
        ))]);
        Menu {
            menu,
            revert,
            update,
            restore,
        }
    }

    /// Ask for a new name, and keep it as this application's alias.
    ///
    /// The dialog says up front that the name is ours alone: AdGuard has no
    /// rename, so `adguard-cli`, its logs and its notifications go on using
    /// the original — which would otherwise look like the rename failing.
    fn begin_rename(self: &Rc<Self>, id: &str) {
        let Some((current, original, alias_now)) = self.rows.borrow().get(id).map(|row| {
            let script = row.script.borrow();
            (
                script.display_name().to_owned(),
                script.original_name().to_owned(),
                script.alias.clone(),
            )
        }) else {
            return;
        };
        let renamed = alias_now.is_some();

        let dialog = adw::AlertDialog::new(Some("Rename userscript"), Some(RENAME_BODY));
        dialog.set_body_use_markup(false);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        let entry = adw::EntryRow::builder().title("Name").text(&current).build();
        entry.set_activates_default(true);
        list.append(&entry);
        if renamed {
            let was = static_row("Original name");
            was.set_subtitle(&original);
            was.set_subtitle_lines(0);
            list.append(&was);
        }
        dialog.set_extra_child(Some(&list));
        dialog.add_response("cancel", "Cancel");
        if renamed {
            dialog.add_response("reset", "Use original");
        }
        dialog.add_response("rename", "Rename");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");

        let this = self.clone();
        let id = id.to_owned();
        glib::spawn_future_local(async move {
            let alias = match dialog.choose_future(Some(&this.bin)).await.as_str() {
                "rename" => Some(entry.text().to_string()),
                "reset" => None,
                _ => return,
            };
            // Saying the original name is the same as asking for it back.
            let alias = alias
                .map(|alias| alias.trim().to_owned())
                .filter(|alias| !alias.is_empty() && *alias != original);
            // *Rename* on an unchanged name is a way of closing the dialog,
            // not a reason to write a file.
            if alias != alias_now {
                this.rename(&id, alias);
            }
        });
    }

    /// Store an alias, or drop it, and repaint the one row.
    ///
    /// The row is not moved to where its new name sorts: a rename is not worth
    /// throwing away the scroll position for, and the next reload puts it
    /// there anyway.
    fn rename(self: &Rc<Self>, id: &str, alias: Option<String>) {
        let saved = match Aliases::locate() {
            Some(aliases) => aliases.set(id, alias.as_deref()).map_err(|err| err.to_string()),
            None => Err("there is no home directory to keep it in".to_owned()),
        };
        if let Err(err) = saved {
            self.toasts.add_toast(toast(&format!("Could not save the name: {err}")));
            return;
        }
        if let Some(row) = self.rows.borrow().get(id) {
            row.script.borrow_mut().alias = alias;
            row.show_name();
        }
    }

    /// Open the installed source in the user's own editor.
    ///
    /// Whatever the desktop opens a `.js` file with — through the portal,
    /// like every other link here. Nothing is copied or locked first: the
    /// pristine copy is already kept ([`Copies`]), and the directory watch
    /// notices the save whichever program makes it.
    fn edit_source(self: &Rc<Self>, id: &str) {
        let Some(dir) = adguard_core::paths::userscripts_dir() else {
            return;
        };
        self.open_file(&userscripts::source_path(&dir, id), true);
    }

    /// Ask before throwing an edit away, since nothing else keeps it.
    fn begin_revert(self: &Rc<Self>, id: &str) {
        let Some(name) = self
            .rows
            .borrow()
            .get(id)
            .map(|row| row.script.borrow().display_name().to_owned())
        else {
            return;
        };
        if let Some(row) = self.rows.borrow().get(id) {
            row.set_busy(true);
        }
        let dialog = adw::AlertDialog::new(
            Some("Revert to the original?"),
            Some(&format!(
                "Your changes to {name} will be replaced with the copy AdGuard installed. \
                 They are not kept anywhere else."
            )),
        );
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("revert", "Revert");
        dialog.set_response_appearance("revert", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = self.clone();
        let id = id.to_owned();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&this.bin)).await != "revert" {
                if let Some(row) = this.rows.borrow().get(&id) {
                    row.set_busy(false);
                }
                return;
            }
            let wanted = id.clone();
            let reverted = worker::job(move || {
                let dir = adguard_core::paths::userscripts_dir()
                    .ok_or_else(|| "AdGuard's data directory is unknown".to_owned())?;
                let copies =
                    Copies::locate().ok_or_else(|| "there is no copy to revert to".to_owned())?;
                copies
                    .revert(&wanted, &userscripts::source_path(&dir, &wanted))
                    .map_err(|err| err.to_string())
            })
            .await;
            if let Some(row) = this.rows.borrow().get(&id) {
                row.set_busy(false);
            }
            match reverted {
                Some(Ok(())) => {
                    this.toasts.add_toast(toast(&format!("Reverted {name}")));
                    // Through the same path as a save from an editor, so the
                    // directory watch reporting this write too is one reload
                    // and not two.
                    this.source_changed(&id);
                }
                Some(Err(err)) => {
                    this.toasts.add_toast(toast(&format!("Could not revert {name}: {err}")));
                }
                None => {}
            }
        });
    }

    /// Watch AdGuard's `userscripts/` directory, so a save from an editor
    /// moves the *Edited* note without a refresh.
    ///
    /// Editors save in bursts — a temporary file, a rename, an attribute
    /// change — so a change only schedules a re-check, a moment after the
    /// last one. A directory that cannot be watched, or does not exist yet,
    /// costs the live update and nothing else: every reload still compares.
    fn watch(self: &Rc<Self>) {
        let Some(dir) = adguard_core::paths::userscripts_dir() else {
            return;
        };
        let Ok(monitor) = gio::File::for_path(&dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            return;
        };
        let page = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, _| {
            let Some(page) = page.upgrade() else {
                return;
            };
            // Only a script's source; metadata moves with installs and
            // updates, which reload the page on their own. An editor's save
            // is often a rename onto the name, so the other file counts too.
            let id_of = |file: &gio::File| {
                file.basename().and_then(|name| {
                    name.to_string_lossy().strip_suffix(".user.js").map(str::to_owned)
                })
            };
            for id in [Some(file), other].into_iter().flatten().filter_map(id_of) {
                page.source_changed(&id);
            }
        });
        self.monitor.replace(Some(monitor));
    }

    /// Note that `id`'s source changed on disk, and re-check once things are
    /// quiet.
    ///
    /// Editors save in bursts, so the re-check waits until [`EDIT_SETTLE`]
    /// has passed without another change, and then handles every id the
    /// burst touched at once.
    fn source_changed(self: &Rc<Self>, id: &str) {
        self.touched.borrow_mut().insert(id.to_owned());
        if let Some(pending) = self.settle_edits.take() {
            pending.remove();
        }
        let later = Rc::downgrade(self);
        let pending = glib::timeout_add_local_once(EDIT_SETTLE, move || {
            if let Some(page) = later.upgrade() {
                page.settle_edits.take();
                let touched = page.touched.take();
                page.recheck_edits(touched);
            }
        });
        self.settle_edits.replace(Some(pending));
    }

    /// Compare every row's source with its pristine copy again, move the
    /// notes that changed, and have AdGuard load the sources in `touched`
    /// again — off the main thread, since a script can be half a megabyte.
    ///
    /// **The reload is what makes an edit do anything.** AdGuard reads a
    /// script into memory and serves it from there: measured 25 November
    /// 2026 on 1.5.1, a source changed on disk was still served unchanged 22
    /// seconds later, with the same `ts=` in the page's script address. What
    /// made it load the file again was `userscripts enable` on the
    /// already-enabled script — a no-op as far as the switch goes, which
    /// moved `ts=` and served the edit on the next page load (contract §15).
    ///
    /// Only for a script that is switched on, so this can never switch
    /// anything on: a switched-off script loads its source when it is
    /// switched on, which is soon enough. And not for an ambiguous one,
    /// which `enable` cannot single out; the toast says to restart instead.
    ///
    /// **Everything is read afresh on the worker, never taken from the rows.**
    /// A row's version is the one it was drawn with; a `check-update` run in a
    /// terminal while the page is open changes the version and the source
    /// together, and comparing the new source against the old version would
    /// file the update as the user's edit and overwrite the edit kept from
    /// before it. Whether a script is switched on is read under the CLI slot,
    /// right before the `enable`: a switch flipped off a moment ago may not be
    /// in the row yet, but it is in `proxy.yaml` before the slot comes free,
    /// and reading it from the row could switch the script back on.
    fn recheck_edits(self: &Rc<Self>, touched: HashSet<String>) {
        if self.rows.borrow().is_empty() {
            return;
        }
        if self.fetching.get() > 0 {
            // Again once the fetch is over. Its own reload compares every
            // source anyway; this keeps the ids so the reload of AdGuard's
            // copy still happens.
            for id in &touched {
                self.source_changed(id);
            }
            return;
        }
        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                // The page's own read: fresh metadata, aliases, and every
                // source compared against its copy at its *current* version.
                let Ok(loaded) = read(&locale) else {
                    return (Vec::new(), Vec::new());
                };
                // With whether a replaced edit is still missing — a save on
                // top of the update makes a new edit, and the old one stops
                // being anything to bring back — and whether the source still
                // names an update address, which a save can add or remove.
                let edited: Vec<(String, bool, Extra)> = loaded
                    .scripts
                    .iter()
                    .map(|script| {
                        let extra = Extra {
                            available: None,
                            pending: loaded.pending.get(&script.id).copied(),
                            bare: loaded.bare.contains(&script.id),
                        };
                        (script.id.clone(), script.edited, extra)
                    })
                    .collect();
                let reloaded: Vec<(String, Option<String>, Result<(), String>)> = loaded
                    .scripts
                    .iter()
                    .filter(|script| touched.contains(&script.id))
                    .filter_map(|script| {
                        let name = script.display_name().to_owned();
                        if !script.actionable() {
                            // Shown only for a switched-on script, as below.
                            return script.enabled.then(|| {
                                let reason = format!(
                                    "Restart protection to run your changes to {name}: \
                                     AdGuard cannot be asked to reload it on its own"
                                );
                                (script.id.clone(), Some(name), Err(reason))
                            });
                        }
                        let id = script.id.clone();
                        let outcome = cli.exclusively(RELOAD_WAIT, || {
                            let on = Config::load()
                                .is_ok_and(|config| is_enabled(&id, &config.enabled_userscripts()));
                            on.then(|| cli.userscripts_enable(&id).map_err(|err| err.to_string()))
                        });
                        match outcome {
                            Some(Some(outcome)) => Some((id, Some(name), outcome)),
                            // Switched off: nothing to reload.
                            Some(None) => None,
                            // Another command of ours held AdGuard for the
                            // whole wait. Not an answer to give the user: the
                            // id goes back on the list and is tried again.
                            None => Some((id, None, Ok(()))),
                        }
                    })
                    .collect();
                (edited, reloaded)
            },
            move |(edited, reloaded)| {
                {
                    let rows = this.rows.borrow();
                    for (id, edited, fresh) in &edited {
                        if let Some(row) = rows.get(id) {
                            row.show_edited(*edited);
                            {
                                let mut extra = row.extra.borrow_mut();
                                extra.pending = fresh.pending;
                                extra.bare = fresh.bare;
                            }
                            row.paint();
                        }
                    }
                }
                // Success is said only about an edit. A reinstall, an update
                // and a revert change the source too, and each already says
                // so itself; they are reloaded all the same, silently. A
                // failure is said whatever changed the source: after a revert
                // it means AdGuard is still running the edit.
                let is_edited =
                    |id: &str| edited.iter().any(|(other, edited, _)| other == id && *edited);
                for (id, name, outcome) in reloaded {
                    let Some(name) = name else {
                        this.source_changed(&id);
                        continue;
                    };
                    if outcome.is_ok() && !is_edited(&id) {
                        continue;
                    }
                    let message = match outcome {
                        Ok(()) => format!(
                            "AdGuard loaded the changed {name}. Reload the page in your browser \
                             to run it."
                        ),
                        Err(err) => err,
                    };
                    this.toasts.add_toast(toast(&message));
                }
            },
        );
    }

    /// The *Installed* group's header suffix: when updates were last checked
    /// for, and *Check now*.
    fn check_header(self: &Rc<Self>) -> gtk::Box {
        let label = gtk::Label::builder().valign(gtk::Align::Center).build();
        label.add_css_class("dim-label");
        label.add_css_class("caption");
        let button = gtk::Button::builder().label("Check now").valign(gtk::Align::Center).build();
        button.add_css_class("flat");
        button.set_tooltip_text(Some(
            "Ask each script's author's server for a newer version. Nothing is installed.",
        ));
        let page = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                page.check_updates(true);
            }
        });
        let header = gtk::Box::builder().spacing(6).build();
        header.append(&label);
        header.append(&button);
        self.check_label.replace(Some(label));
        self.check_button.replace(Some(button));
        self.paint_check_state();
        header
    }

    fn paint_check_state(&self) {
        let (Some(label), Some(button)) = (self.check_label.borrow().clone(), self.check_button.borrow().clone())
        else {
            return;
        };
        button.set_sensitive(!self.checking.get());
        let text = if self.checking.get() {
            "Checking for updates…".to_owned()
        } else {
            match self.checked.borrow().as_ref() {
                None => String::new(),
                Some((when, 0)) => format!("Checked {when}"),
                Some((when, unknown)) => format!("Checked {when} · {unknown} could not be checked"),
            }
        };
        label.set_visible(!text.is_empty());
        label.set_label(&text);
    }

    /// Ask every script's server whether there is a newer version, and put
    /// what it says on the rows.
    ///
    /// **Read only.** One request per script, to the address a userscript
    /// manager polls ([`script_update`]); nothing is installed, and *Update to
    /// x.y* stays the user's to press. It is also the only way a switched-off
    /// script learns it is out of date: `check-update` skips those entirely
    /// (contract §15).
    ///
    /// `announce` for *Check now*, which says what it found; the first build
    /// and the daily timer only paint.
    pub fn check_updates(self: &Rc<Self>, announce: bool) {
        if self.checking.replace(true) {
            return;
        }
        let targets: Vec<Userscript> =
            self.rows.borrow().values().map(|row| row.script.borrow().clone()).collect();
        if targets.is_empty() {
            self.checking.set(false);
            return;
        }
        self.paint_check_state();

        let this = self.clone();
        worker::run(
            move || {
                // In parallel: each is one request with its own timeout, and
                // a slow server should hold up its own row, not every row.
                std::thread::scope(|scope| {
                    let asked: Vec<_> = targets
                        .iter()
                        .map(|script| {
                            let check = scope.spawn(move || {
                                script_update::check(
                                    script.version.as_deref(),
                                    script.update_url.as_deref(),
                                    script.download_url.as_deref(),
                                )
                            });
                            (script.id.clone(), check)
                        })
                        .collect();
                    asked
                        .into_iter()
                        .map(|(id, check)| {
                            let check = check
                                .join()
                                .unwrap_or_else(|_| Check::Unknown("the check failed".to_owned()));
                            (id, check)
                        })
                        .collect::<Vec<_>>()
                })
            },
            move |results: Vec<(String, Check)>| {
                this.checking.set(false);
                let mut updates = HashMap::new();
                let mut unknown = 0;
                for (id, check) in results {
                    match check {
                        Check::Available(version) => {
                            updates.insert(id, version);
                        }
                        Check::UpToDate => {}
                        Check::Unknown(_) => unknown += 1,
                    }
                }
                let found = updates.len();
                this.updates.replace(updates);
                this.checked.replace(Some((clock(), unknown)));
                for (id, row) in this.rows.borrow().iter() {
                    row.extra.borrow_mut().available = this.updates.borrow().get(id).cloned();
                    row.paint();
                }
                this.paint_check_state();
                if announce {
                    this.toasts.add_toast(toast(&check_summary(found, unknown)));
                }
            },
        );
    }

    /// Ask, then update one script to the version the check found.
    fn begin_update(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        if let Some(row) = self.rows.borrow().get(&id) {
            row.set_busy(true);
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            if this.confirm_update(&id).await {
                this.update(&id);
            } else if let Some(row) = this.rows.borrow().get(&id) {
                row.set_busy(false);
            }
        });
    }

    /// *Reinstall*'s question, for an update: where it comes from, what
    /// happens to the user's changes, and — on a switched-off script, which is
    /// the only kind that needs this page to update at all — that it will be
    /// switched on.
    async fn confirm_update(&self, id: &str) -> bool {
        let (name, url, enabled, changed, from, to) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(id) else {
                return false;
            };
            let Some(to) = row.available() else {
                return false;
            };
            let script = row.script.borrow();
            let Some(url) = script.download_url.clone() else {
                return false;
            };
            let changed = script.edited || row.extra.borrow().pending.is_some();
            (
                script.display_name().to_owned(),
                url,
                script.enabled,
                changed,
                script.version.clone().unwrap_or_default(),
                to,
            )
        };

        let mut body = format!("AdGuard will fetch {name} {to} from {url} and install it in place of {from}.");
        if changed {
            body.push_str(
                "\n\nYour changes to its source are carried over to the new version. If they \
                 clash with the author's changes, you will be asked what to keep.",
            );
        }
        if !enabled {
            body.push_str(
                "\n\nIt will also switch this userscript back on — installing always enables, \
                 and there is no way to ask it not to.",
            );
        }

        let dialog = adw::AlertDialog::new(Some(&format!("Update to {to}?")), Some(&body));
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("update", "Update");
        dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.choose_future(Some(&self.bin)).await == "update"
    }

    /// Install the new version, then carry the user's edit into it.
    ///
    /// The edit goes through stage 1's copies, not around them: a read first,
    /// so an edit not yet observed is kept before AdGuard overwrites it; the
    /// fetch; [`rebase`], which files the new release and sets the edit aside
    /// with the copy it was made on; and [`carry`], which merges it back in.
    /// All of it under [`Fetching`], so the directory watch does not take
    /// AdGuard's fresh copy for an edit halfway through.
    fn update(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        let name = self.name_of(&id);
        let Some(url) = self
            .rows
            .borrow()
            .get(&id)
            .and_then(|row| row.script.borrow().download_url.clone())
        else {
            return;
        };

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        let wanted = id.clone();
        let fetching = Fetching::start(&self.fetching);
        worker::run(
            move || {
                let _ = read(&locale);
                let refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
                let carried = refused.is_none().then(|| {
                    rebase(&wanted, false);
                    carry(&wanted)
                });
                let version = read(&locale)
                    .ok()
                    .and_then(|loaded| loaded.scripts.into_iter().find(|s| s.id == wanted))
                    .and_then(|script| script.version);
                (refused, carried, version)
            },
            move |(refused, carried, version)| {
                drop(fetching);
                if let Some(row) = this.rows.borrow().get(&id) {
                    row.set_busy(false);
                }
                if let Some(refused) = refused {
                    // AdGuard's own sentence, the same for every cause.
                    this.toasts.add_toast(toast(&refused));
                    return;
                }
                let updated = match &version {
                    Some(version) => format!("Updated {name} to {version}"),
                    None => format!("Updated {name}"),
                };
                let carried = carried.unwrap_or(Carry::Nothing);
                let done = match carried {
                    Carry::Kept => format!("{updated}, your changes kept"),
                    _ => updated,
                };
                this.report_carry(&id, &name, &done, carried);
                this.reload();
            },
        );
    }

    /// Put a kept edit that an update replaced back into the installed source.
    fn begin_restore(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        let name = self.name_of(&id);
        if let Some(row) = self.rows.borrow().get(&id) {
            row.set_busy(true);
        }
        let this = self.clone();
        let wanted = id.clone();
        worker::run(
            move || carry(&wanted),
            move |carried| {
                if let Some(row) = this.rows.borrow().get(&id) {
                    row.set_busy(false);
                }
                this.report_carry(&id, &name, &format!("Your changes to {name} are back"), carried);
                // Through the watch's path, like a revert: the note moves,
                // and AdGuard is asked to load the source again.
                this.source_changed(&id);
            },
        );
    }

    /// Say how carrying an edit went, and ask about a clash.
    ///
    /// `done` is the sentence for when there was nothing to carry or it went
    /// in cleanly; the caller words it for each.
    fn report_carry(self: &Rc<Self>, id: &str, name: &str, done: &str, carried: Carry) {
        match carried {
            Carry::Nothing | Carry::Kept => self.toasts.add_toast(toast(done)),
            Carry::Moved => self.toasts.add_toast(toast(&format!(
                "The source of {name} changed meanwhile, so your changes were not put back. \
                 They are kept; try again from the menu."
            ))),
            Carry::Failed(err) => {
                self.toasts.add_toast(toast(&format!("Could not put your changes to {name} back: {err}")));
            }
            Carry::Clash { marked, conflicts } => {
                let this = self.clone();
                let id = id.to_owned();
                let name = name.to_owned();
                glib::spawn_future_local(async move {
                    this.resolve_clash(&id, &name, marked, conflicts).await;
                });
            }
        }
    }

    /// The user's changes and the author's touch the same lines. The update
    /// is installed either way; this asks what becomes of the changes.
    ///
    /// *Decide later* is the safe answer and the one Escape gives: the changes
    /// stay kept, and the row offers *Bring back your changes…* until the user
    /// picks. *Open both in editor* opens the installed update and the merge
    /// with git's markers, for the user to copy what they want across; the
    /// next save is their new edit, and the old one is let go then.
    async fn resolve_clash(self: &Rc<Self>, id: &str, name: &str, marked: Option<String>, conflicts: usize) {
        let places = match conflicts {
            0 | 1 => "a place".to_owned(),
            n => format!("{n} places"),
        };
        let body = match &marked {
            Some(_) => format!(
                "The update to {name} changes {places} you had also changed, so your changes were \
                 not carried over. The update is installed, and your changes are kept until you \
                 choose."
            ),
            None => format!(
                "Your changes to {name} could not be carried over automatically: one of the copies \
                 is not plain text. The update is installed, and your changes are kept until you \
                 choose."
            ),
        };
        let dialog = adw::AlertDialog::new(Some("Your changes clash with the update"), Some(&body));
        dialog.set_body_use_markup(false);
        dialog.set_prefer_wide_layout(true);
        dialog.add_response("later", "Decide later");
        dialog.add_response("take", "Use the update");
        dialog.add_response("keep", "Keep my version");
        if marked.is_some() {
            dialog.add_response("open", "Open both in editor");
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
        }
        dialog.set_response_appearance("take", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("later"));
        dialog.set_close_response("later");
        let answer = dialog.choose_future(Some(&self.bin)).await;

        let wanted = id.to_owned();
        match answer.as_str() {
            "take" => {
                let dropped = worker::job(move || {
                    Copies::locate().map(|copies| copies.discard_edit(&wanted).map_err(|err| err.to_string()))
                })
                .await
                .flatten();
                match dropped {
                    Some(Ok(())) => self.toasts.add_toast(toast(&format!("Using the update of {name} as it is"))),
                    Some(Err(err)) => self.toasts.add_toast(toast(&format!("Could not let your changes go: {err}"))),
                    None => {}
                }
            }
            "keep" => {
                let kept = worker::job(move || keep_mine(&wanted)).await;
                match kept {
                    Some(Carry::Kept) => {
                        self.toasts.add_toast(toast(&format!("Kept your version of {name}")));
                    }
                    Some(other) => self.report_carry(id, name, "", other),
                    None => {}
                }
            }
            "open" => {
                let Some(marked) = marked else {
                    return;
                };
                let written = worker::job(move || {
                    Copies::locate()
                        .ok_or_else(|| "there is nowhere to keep the merge".to_owned())
                        .and_then(|copies| copies.write_merge(&wanted, &marked).map_err(|err| err.to_string()))
                })
                .await;
                match written {
                    Some(Ok(merge)) => {
                        self.edit_source(id);
                        self.open_file(&merge, false);
                    }
                    Some(Err(err)) => self.toasts.add_toast(toast(&format!("Could not write the merge: {err}"))),
                    None => {}
                }
            }
            _ => {}
        }
        self.reload();
    }

    /// After a `check-update` — from the About page, the one place this
    /// application runs it — put back every kept edit it replaced.
    ///
    /// **Whatever the report says.** A script with no `updateURL` is
    /// downloaded again on every run, at the same version, over any edit
    /// (contract §15), and the report's `1 userscript(s) updated` says the
    /// same thing whether or not anything changed. So every script is
    /// compared, not just the ones that might have moved, and a clean carry is
    /// made without asking: the edit was the user's, and AdGuard replacing it
    /// was not their choice. A clash is not resolved behind their back; it is
    /// left on its row, which says so.
    pub fn after_check_update(self: &Rc<Self>) {
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let Ok(loaded) = read(&locale) else {
                    return Vec::new();
                };
                loaded
                    .scripts
                    .iter()
                    .filter(|script| loaded.pending.contains_key(&script.id))
                    .map(|script| (script.display_name().to_owned(), carry(&script.id)))
                    .collect::<Vec<_>>()
            },
            move |carried: Vec<(String, Carry)>| {
                for (name, carried) in carried {
                    let message = match carried {
                        Carry::Kept => format!("Kept your changes to {name} across AdGuard's update"),
                        Carry::Clash { .. } => format!(
                            "Your changes to {name} clash with its update. They are kept; bring them \
                             back from its menu on the Extensions page."
                        ),
                        Carry::Moved | Carry::Failed(_) => format!(
                            "AdGuard's update replaced your changes to {name}. They are kept; bring \
                             them back from its menu on the Extensions page."
                        ),
                        Carry::Nothing => continue,
                    };
                    this.toasts.add_toast(toast(&message));
                }
                this.reload();
            },
        );
    }

    /// Open a file in the user's editor. `writable` for AdGuard's script,
    /// which *Edit source* exists to change; not for a merge kept only to be
    /// read.
    fn open_file(&self, path: &std::path::Path, writable: bool) {
        let file = gio::File::for_path(path);
        let launcher = gtk::FileLauncher::new(Some(&file));
        // An editor reached through the document portal gets the file
        // read-only unless writable access is asked for, and then cannot save
        // — the one thing *Edit* is for. The property arrived in GTK 4.14,
        // and this application builds against 4.10, so it is set by name
        // where the running GTK has it rather than by raising the floor.
        if writable && launcher.find_property("writable").is_some() {
            launcher.set_property("writable", true);
        }
        let window = self.bin.root().and_downcast::<gtk::Window>();
        let toasts = self.toasts.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = launcher.launch_future(window.as_ref()).await {
                // Dismissing the portal's chooser is an answer, not a fault.
                if !err.matches(gtk::DialogError::Dismissed) {
                    toasts.add_toast(toast(&format!("Could not open the file: {err}")));
                }
            }
        });
    }

    /// The one control on this page that destroys something.
    ///
    /// A suffix button with a confirmation rather than a quieter action, for
    /// the reason the Filters page gives about its own: the row is an
    /// `AdwSwitchRow` whose activatable widget is the switch, and "off" and
    /// "gone" are exactly the two things that must not be confusable.
    ///
    /// **The row is fenced at the click, not at the answer** — [issue #5]'s
    /// lesson, applied here from the start. The confirmation is presented from
    /// a spawned future, so it is not up when this handler returns, and GDK can
    /// dispatch more than one queued event in a single main-loop iteration.
    ///
    /// [issue #5]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/5
    fn remove_button(self: &Rc<Self>, script: &Userscript) -> gtk::Button {
        let button = gtk::Button::from_icon_name("user-trash-symbolic");
        button.set_tooltip_text(Some("Remove this userscript"));
        button.set_valign(gtk::Align::Center);
        button.add_css_class("flat");
        button.add_css_class("destructive-action");
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove {}",
            script.display_name()
        ))]);

        let this = Rc::downgrade(self);
        let id = script.id.clone();
        button.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if let Some(row) = this.rows.borrow().get(&id) {
                row.set_busy(true);
            }
            let id = id.clone();
            glib::spawn_future_local(async move {
                if this.confirm_removal(&id).await {
                    this.remove(&id);
                } else if let Some(row) = this.rows.borrow().get(&id) {
                    // Nothing was sent and nothing was painted, so the row is
                    // the only thing there is to put back.
                    row.set_busy(false);
                }
            });
        });

        button
    }

    /// Ask before deleting a script, and say what cannot be undone.
    ///
    /// The body points at the switch, because "stop it running" is what most
    /// people reaching for this actually want. The rows below name the script
    /// and where it came from, since its address is the only thing that can
    /// bring it back — folded away and copyable rather than printed in the
    /// body, where an escaped URL is a wall of `%20`s centred across five
    /// lines.
    async fn confirm_removal(&self, id: &str) -> bool {
        let (name, source) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(id) else {
                return false;
            };
            let script = row.script.borrow();
            (
                script.display_name().to_owned(),
                script.download_url.clone(),
            )
        };

        let dialog = adw::AlertDialog::new(Some("Remove this userscript?"), Some(REMOVE_BODY));
        dialog.set_body_use_markup(false);
        dialog.set_prefer_wide_layout(true);
        dialog.set_extra_child(Some(&removal_details(&name, source.as_deref())));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "remove"
    }

    /// Delete one script, then confirm it by the row being **gone**.
    ///
    /// Not by `removed successfully`, which proves nothing: every userscript
    /// command exits 0 whether or not it did anything (contract §15).
    fn remove(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        let name = self.name_of(&id);
        if let Some(row) = self.rows.borrow().get(&id) {
            row.set_busy(true);
        }

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        let wanted = id.clone();
        worker::run(
            move || {
                let refused = cli.userscripts_remove(&wanted).err().map(|e| e.to_string());
                // `None` when the sources could not be read at all, which is not
                // the same as "no script with that id" — reporting an unreadable
                // install as a successful deletion is the worst way to guess.
                let still_there = read(&locale)
                    .ok()
                    .map(|loaded| loaded.scripts.iter().any(|s| s.id == wanted));
                (refused, still_there)
            },
            move |(refused, still_there)| match still_there {
                Some(false) => {
                    // Its name and copies were about a script that is gone; a
                    // later install of the same id starts afresh.
                    forget(&id);
                    this.toasts.add_toast(toast(&format!("Removed {name}")));
                    // A row has disappeared, so there is nothing to patch.
                    this.reload();
                }
                Some(true) => {
                    if let Some(row) = this.rows.borrow().get(&id) {
                        row.set_busy(false);
                    }
                    this.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                        format!("AdGuard reported {name} was removed, but it is still installed")
                    })));
                }
                None => {
                    this.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                        format!("Could not re-read the userscripts to confirm {name} was removed")
                    })));
                    this.reload();
                }
            },
        );
    }

    /// Switch one script on or off.
    fn toggle(self: &Rc<Self>, id: &str, on: bool) {
        let id = id.to_owned();
        let name = self.name_of(&id);
        if let Some(row) = self.rows.borrow().get(&id) {
            // Insensitive until the files have spoken, so a second click cannot
            // race the first one's verification.
            row.set_busy(true);
        }

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        let wanted = id.clone();
        worker::run(
            move || {
                let refused = if on {
                    cli.userscripts_enable(&wanted)
                } else {
                    cli.userscripts_disable(&wanted)
                }
                .err()
                .map(|err| err.to_string());

                let observed = read(&locale)
                    .ok()
                    .and_then(|loaded| loaded.scripts.into_iter().find(|s| s.id == wanted));
                (refused, observed)
            },
            move |(refused, observed)| this.settle(&id, on, &name, refused, observed),
        );
    }

    /// Paint the row from what the files say, whatever was asked for.
    fn settle(
        self: &Rc<Self>,
        id: &str,
        wanted: bool,
        name: &str,
        refused: Option<String>,
        observed: Option<Userscript>,
    ) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(id) else {
            return;
        };
        row.set_busy(false);

        let Some(observed) = observed else {
            // The re-read failed, so nothing is known. Say so and leave the
            // switch where the user put it rather than inventing a state.
            self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                format!("Could not re-read the userscripts to confirm {name}")
            })));
            return;
        };

        let settled = observed.enabled;
        self.reconciling.set(true);
        row.switch.set_active(settled);
        self.reconciling.set(false);
        *row.script.borrow_mut() = observed;
        // The crash caveat follows the switch.
        row.paint();
        drop(rows);
        self.show_helper_state();

        if settled == wanted {
            return;
        }

        // The CLI's own wording first — an ambiguity refusal explains itself
        // far better than we could, and it is the likeliest thing to land here.
        self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
            let verb = if wanted { "switch on" } else { "switch off" };
            format!("AdGuard did not {verb} {name}")
        })));
    }

    /// Ask before reinstalling, because of what it does to the switch.
    ///
    /// Measured (contract §15): reinstalling updates the script in place **and
    /// silently switches a disabled one back on**. A user who turned a script
    /// off and later updates it did not ask for it to start running again, so
    /// the dialog says so — and only when it applies, since warning about it on
    /// a script that is already on would be noise.
    fn begin_reinstall(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        if let Some(row) = self.rows.borrow().get(&id) {
            row.set_busy(true);
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            if this.confirm_reinstall(&id).await {
                this.reinstall(&id);
            } else if let Some(row) = this.rows.borrow().get(&id) {
                row.set_busy(false);
            }
        });
    }

    async fn confirm_reinstall(&self, id: &str) -> bool {
        let (name, url, enabled, edited) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(id) else {
                return false;
            };
            let script = row.script.borrow();
            let Some(url) = script.download_url.clone() else {
                return false;
            };
            (script.display_name().to_owned(), url, script.enabled, script.edited)
        };

        let mut body = format!(
            "AdGuard will fetch {name} from {url} again and replace the installed copy \
             with whatever is there now."
        );
        if edited {
            body.push_str("\n\nYour changes to its source will be replaced too.");
        }
        if !enabled {
            body.push_str(
                "\n\nIt will also switch this userscript back on — reinstalling always \
                 enables, and there is no way to ask it not to.",
            );
        }

        let dialog = adw::AlertDialog::new(Some("Reinstall this userscript?"), Some(&body));
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("reinstall", "Reinstall");
        dialog.set_response_appearance("reinstall", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future(Some(&self.bin)).await == "reinstall"
    }

    /// Re-fetch a script from the URL it came from.
    fn reinstall(self: &Rc<Self>, id: &str) {
        let id = id.to_owned();
        let name = self.name_of(&id);
        let Some(url) = self
            .rows
            .borrow()
            .get(&id)
            .and_then(|row| row.script.borrow().download_url.clone())
        else {
            return;
        };

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        let wanted = id.clone();
        let fetching = Fetching::start(&self.fetching);
        worker::run(
            move || {
                let refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
                if refused.is_none() {
                    // What is on disk now is AdGuard's fresh copy, even if
                    // the author changed it without moving the version — so it
                    // is the pristine copy, and not an edit, from here on.
                    rebase(&wanted, true);
                }
                let observed = read(&locale)
                    .ok()
                    .and_then(|loaded| loaded.scripts.into_iter().find(|s| s.id == wanted));
                (refused, observed)
            },
            move |(refused, observed)| {
                drop(fetching);
                if let Some(row) = this.rows.borrow().get(&id) {
                    row.set_busy(false);
                }
                match (refused, observed) {
                    (None, Some(script)) => {
                        let version = script
                            .version
                            .clone()
                            .map_or_else(|| name.clone(), |v| format!("{name} {v}"));
                        this.toasts.add_toast(toast(&format!("Reinstalled {version}")));
                        // The version, the description and the switch may all
                        // have moved; a rebuild is the honest repaint.
                        this.reload();
                    }
                    // AdGuard's own sentence, which for an install is the same
                    // one for every cause and says nothing about which.
                    (Some(refused), _) => this.toasts.add_toast(toast(&refused)),
                    (None, None) => {
                        this.toasts.add_toast(toast(&format!(
                            "Could not re-read the userscripts to confirm {name} was reinstalled"
                        )));
                        this.reload();
                    }
                }
            },
        );
    }

    /// An install link arrived from a browser (`crate::install_link`): ask,
    /// and install only on a yes.
    ///
    /// **One dialog at a time.** Links queue, and the next is asked about only
    /// once the last answer — and the install it led to — is over. A page that
    /// fires ten links gets ten questions in a row rather than a stack of
    /// dialogs to click through blind, and two installs never race each other
    /// to `proxy.yaml`. A link already queued, or the one being asked about, is
    /// not queued again: a double click in the browser is one question.
    pub fn offer_install(self: &Rc<Self>, url: String) {
        let duplicate = self.asking.borrow().as_deref() == Some(url.as_str())
            || self.offers.borrow().contains(&url);
        if duplicate {
            return;
        }
        self.offers.borrow_mut().push_back(url);
        if self.asking.borrow().is_some() {
            return;
        }

        let this = self.clone();
        glib::spawn_future_local(async move {
            loop {
                let Some(url) = this.offers.borrow_mut().pop_front() else {
                    break;
                };
                this.asking.replace(Some(url.clone()));
                if let Some((served, alias)) = this.confirm_install(&url).await {
                    this.install(served, None, alias).await;
                }
                this.asking.replace(None);
            }
        });
    }

    /// The link's only gate. Anything on the web can open one, so the dialog
    /// says what the script is — its own name, version, description and the
    /// sites it runs on, read from its metadata block — always beside the host
    /// it comes from, which is the part that says whose code this is. The
    /// whole URL is one click away. The safe answer is the default one.
    ///
    /// **Add cannot be pressed for the first [`ADD_DELAY`].** The dialog comes
    /// up because of a click in another window, often right under the pointer,
    /// and the click or keypress after it lands on whatever is there — a page
    /// can even ask for that, with a "click twice" game. So Add starts greyed
    /// out with a countdown on it, the way Firefox guards its install prompts,
    /// and the countdown starts again whenever the window loses focus, so it
    /// cannot be waited out elsewhere and then clicked through unread. Enter
    /// and Escape are Cancel throughout, and Add is a plain button rather than
    /// a highlighted one, so the eye does not land on it first.
    ///
    /// On *Add*, the address to install from: where the link's redirects led
    /// once the script was read there, so AdGuard — which follows none (#29) —
    /// fetches the file the dialog described. The link itself when it could
    /// not be read. Beside it, the name the user gave the script in the
    /// dialog, when they changed the one it came with.
    async fn confirm_install(&self, url: &str) -> Option<(String, Option<String>)> {
        let dialog = adw::AlertDialog::new(Some("Add this userscript?"), Some(INSTALL_BODY));
        dialog.set_body_use_markup(false);
        // Room for the rows to read as rows rather than as a column of
        // wrapped fragments.
        dialog.set_prefer_wide_layout(true);
        let details = Rc::new(InstallDetails::new(url));
        details.show(&install_link::summary(url, &Details::Reading));

        // The name it will be listed under, editable before it is added:
        // Greasy Fork names run to a hundred characters of emoji and
        // keywords. Its own list below the details, because those describe
        // the script and this is a choice about it. Filled in once the
        // script's `@name` is read, and left alone if the user has typed.
        let naming = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        naming.add_css_class("boxed-list");
        let name_entry = adw::EntryRow::builder().title("List it as").build();
        name_entry.set_sensitive(false);
        naming.append(&name_entry);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        content.append(&details.list);
        content.append(&naming);
        dialog.set_extra_child(Some(&content));
        let given = Rc::new(RefCell::new(String::new()));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("install", "Add");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        // *Add* waits for the script's own account of itself as well as for the
        // countdown: the point of reading it is that the answer is given with
        // it on screen. A fetch that fails unblocks it too — the dialog then
        // says so and shows the file name, and the choice is still the user's.
        let countdown = Countdown::new(&dialog, "install", "Add");
        countdown.set_blocked(true);
        let served = Rc::new(RefCell::new(url.to_owned()));
        {
            let url = url.to_owned();
            let countdown = countdown.clone();
            let details_view = details.clone();
            let served = served.clone();
            let given = given.clone();
            let name_entry = name_entry.clone();
            glib::spawn_future_local(async move {
                let fetched = {
                    let url = url.clone();
                    worker::job(move || preview::fetch(&url)).await
                };
                let details = match fetched {
                    Some(Ok(fetched)) => {
                        if fetched.url != url {
                            details_view.readdress(&fetched.url);
                        }
                        served.replace(fetched.url.clone());
                        if let Some(name) = fetched.preview.name.as_deref() {
                            given.replace(name.to_owned());
                            if name_entry.text().is_empty() {
                                name_entry.set_text(name);
                            }
                        }
                        name_entry.set_sensitive(true);
                        Details::Read(fetched)
                    }
                    Some(Err(err)) => Details::Unread(err.to_string()),
                    None => Details::Unread("reading it failed".to_owned()),
                };
                // Into rows that may already be gone with the dialog — then
                // this paints widgets nobody will see, which is harmless.
                details_view.show(&install_link::summary(&url, &details));
                countdown.set_blocked(false);
            });
        }
        let window = self.bin.root().and_downcast::<gtk::Window>();
        let focus = window.as_ref().map(|window| {
            let countdown = countdown.clone();
            window.connect_is_active_notify(move |window| {
                if window.is_active() {
                    countdown.start();
                } else {
                    countdown.hold();
                }
            })
        });
        // A window launched from a click in a browser may not have focus yet —
        // GNOME can decline to raise it — and then the countdown waits for the
        // first time it does.
        if window.as_ref().is_none_or(|window| window.is_active()) {
            countdown.start();
        } else {
            countdown.hold();
        }

        let answer = dialog.choose_future(Some(&self.bin)).await;

        countdown.hold();
        if let (Some(window), Some(focus)) = (window, focus) {
            window.disconnect(focus);
        }
        // A name left as the script gave it, or emptied, is no alias.
        let typed = name_entry.text().trim().to_owned();
        let alias = (!typed.is_empty() && typed != given.borrow().trim()).then_some(typed);
        (answer == "install").then(|| (served.take(), alias))
    }

    /// Fetch and install a userscript, then confirm it against the directory.
    ///
    /// The id is assigned by AdGuard from the filename, so — as with a custom
    /// filter — it cannot be known in advance: the scripts are read before and
    /// after, and one that was not there before is the evidence. An id that was
    /// *already* there is the reinstall case, which is a legitimate outcome of
    /// pasting a URL twice and is reported as such rather than as a failure.
    ///
    /// `field` is the add row and its spinner when the URL was typed there, and
    /// `None` when it arrived as an install link (`crate::install_link`) — that
    /// row is rebuilt with the page, so it cannot be held across a dialog.
    ///
    /// `alias` is the name the user gave it in the install dialog, if they
    /// changed it there; kept once the new id is known.
    async fn install(
        self: &Rc<Self>,
        url: String,
        field: Option<(adw::EntryRow, adw::Spinner)>,
        alias: Option<String>,
    ) {
        let typed = field.is_some();
        let _fetching = Fetching::start(&self.fetching);
        let set_busy = |busy: bool, clear: bool| {
            if let Some((entry, spinner)) = &field {
                entry.set_sensitive(!busy);
                spinner.set_visible(busy);
                if clear {
                    entry.set_text("");
                }
            }
        };
        set_busy(true, false);

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let outcome = worker::job(move || {
            let before: Vec<String> = read(&locale)
                .map(|loaded| loaded.scripts.into_iter().map(|s| s.id).collect())
                .unwrap_or_default();
            let mut refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
            // A typed address is not read first, so one that redirects — an
            // install counter, a short link — reaches AdGuard as it is, and
            // AdGuard follows no redirects (#29). Its refusal is the same
            // sentence for every cause, so on any refusal the address is
            // followed here, and tried once more if it led somewhere else.
            let mut fetched_from = url.clone();
            if refused.is_some() {
                let moved = preview::fetch(&url).ok().map(|fetched| fetched.url);
                if let Some(moved) = moved.filter(|moved| *moved != url) {
                    refused = cli.userscripts_install(&moved).err().map(|e| e.to_string());
                    fetched_from = moved;
                }
            }
            // The scripts this address installed, found by the address
            // AdGuard recorded for them.
            let mut refetched = Vec::new();
            if refused.is_none() {
                // An address already installed is updated in place, at a
                // version that may not have moved — and then the fresh copy
                // would read as the user's edit. It is AdGuard's copy, so it
                // is the pristine one.
                //
                // Found from the metadata alone. The page's own `read` would
                // compare the sources first, and file the fresh copy as an
                // edit before it could be recorded as pristine.
                if let Some(dir) = adguard_core::paths::userscripts_dir() {
                    refetched = userscripts::read(&dir, &[], &Locale::english())
                        .into_iter()
                        .filter(|script| {
                            script.download_url.as_deref() == Some(fetched_from.as_str())
                        })
                        .map(|script| script.id)
                        .collect();
                    for id in &refetched {
                        rebase(id, false);
                    }
                }
            }
            let after = read(&locale).ok().map(|loaded| loaded.scripts);
            (refused, before, after, refetched)
        })
        .await;

        let Some((refused, before, Some(after), refetched)) = outcome else {
            set_busy(false, false);
            let refused = outcome.and_then(|(refused, _, _, _)| refused);
            self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                "Could not re-read the userscripts to confirm the install".to_owned()
            })));
            self.reload();
            return;
        };

        match after.iter().find(|s| !before.contains(&s.id)) {
            Some(new) => {
                set_busy(false, true);
                let mut shown = new.display_name().to_owned();
                if let Some(alias) = alias.filter(|alias| alias.trim() != new.original_name()) {
                    match Aliases::locate().map(|aliases| aliases.set(&new.id, Some(&alias))) {
                        Some(Ok(())) => shown = alias.trim().to_owned(),
                        _ => self.toasts.add_toast(toast("Could not save the name you gave it")),
                    }
                }
                self.toasts.add_toast(toast(&format!("Added {shown}")));
                // The moment the helper is worth mentioning: someone has just
                // copied a script's address out of a browser and pasted it
                // here, which is exactly what it saves. Once per run, and only
                // while it is missing or off — a reminder, not a campaign. A banner
                // rather than the toast: a toast has one line, and a long
                // script name leaves no room for the explanation.
                let helper_missing = !userscripts::install_links(&after).works();
                if typed && helper_missing && !self.hinted.replace(true) {
                    self.banner.set_revealed(true);
                }
            }
            // Nothing new. Either it was refused, or the URL was one already
            // installed and this was an update in place — which the CLI
            // reports identically, so the row count is what tells them apart.
            None => {
                set_busy(false, false);
                // A name typed in the dialog is still the user's choice for
                // the script this updated — when it is clear which one that
                // is: exactly one installed script recorded this address.
                let updated = match refetched.as_slice() {
                    [id] if refused.is_none() => after.iter().find(|script| &script.id == id),
                    _ => None,
                };
                if let (Some(script), Some(alias)) = (updated, alias) {
                    if alias.trim() != script.original_name() {
                        let saved = Aliases::locate()
                            .map(|aliases| aliases.set(&script.id, Some(&alias)));
                        if !matches!(saved, Some(Ok(()))) {
                            self.toasts.add_toast(toast("Could not save the name you gave it"));
                        }
                    }
                }
                self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                    "That userscript was already installed; AdGuard updated it in place"
                        .to_owned()
                })));
            }
        }
        self.reload();
    }

    /// The display name for a row, for a message about it.
    fn name_of(&self, id: &str) -> String {
        self.rows
            .borrow()
            .get(id)
            .map(|row| row.script.borrow().display_name().to_owned())
            .unwrap_or_else(|| id.to_owned())
    }
}

/// Read both sources, the way the page renders them.
/// The line under the rename question. The name is ours alone, and saying so
/// here is what stops the CLI's own messages looking like a failed rename.
const RENAME_BODY: &str = "Only this application shows the new name. AdGuard and adguard-cli \
                           keep using the original one.";

/// How long a reload after an edit waits for another command of ours to
/// finish before giving up — long enough for a toggle or an install, short of
/// leaving a worker parked on a CLI that has hung.
const RELOAD_WAIT: Duration = Duration::from_secs(30);

/// How long the userscripts directory has to be quiet before the *Edited*
/// notes are re-checked: long enough for an editor's save — temporary file,
/// rename, attributes — to finish, short enough to look immediate.
const EDIT_SETTLE: Duration = Duration::from_millis(400);

/// The one line under an install link's question. Short, because the rows
/// below carry the specifics and a long warning is the paragraph people skip.
const INSTALL_BODY: &str = "Add only scripts from sources you trust. It can read and change \
                            pages on the sites it runs on.";

/// The rows under an install link's question (#29): what the script is, where
/// it comes from, where it runs, and the address itself.
///
/// Built once with every row in place and filled in twice — the decoded file
/// name while the script is read, then what it says about itself — so nothing
/// jumps when the details arrive. Laid out as a boxed list, the way GNOME
/// presents properties, and left-aligned: centred paragraphs are hard to scan,
/// and this is read to make a decision.
struct InstallDetails {
    list: gtk::ListBox,
    /// The name, with whatever short line goes under it.
    name: adw::ActionRow,
    /// The name, when the description is too long for two lines: it expands
    /// to the whole text.
    described: adw::ExpanderRow,
    description: gtk::Label,
    version: adw::ActionRow,
    version_value: gtk::Label,
    source_value: gtk::Label,
    /// The link's own host, when it redirected to the source.
    redirect: adw::ActionRow,
    /// "Runs on" with nothing to list: one site, every site, unknown, or
    /// still reading.
    reach: adw::ActionRow,
    reach_value: gtk::Label,
    reach_warning: gtk::Image,
    reach_spinner: adw::Spinner,
    /// "Runs on" with a list, which it expands to show in full.
    sites: adw::ExpanderRow,
    sites_value: gtk::Label,
    listed: RefCell<Vec<adw::ActionRow>>,
    /// The full address — the link's, until it turns out to redirect.
    address: RefCell<adw::ExpanderRow>,
}

impl InstallDetails {
    fn new(url: &str) -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");

        // Subtitles are never cut by the label (`subtitle-lines` 0): it cuts
        // through words, and every line here is shortened at a word boundary
        // by `install_link::shorten` instead.
        let name = static_row("");
        name.set_title_lines(3);
        list.append(&name);

        let described = adw::ExpanderRow::builder().build();
        described.set_use_markup(false);
        described.set_title_lines(3);
        let description = gtk::Label::builder()
            .use_markup(false)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        described.add_row(&description);
        // Collapsed, the two-line preview; expanded, only the full text under
        // the row, so the start of it is not on screen twice.
        described.connect_expanded_notify({
            let description = description.clone();
            move |row| {
                let preview = if row.is_expanded() {
                    String::new()
                } else {
                    install_link::shorten(&description.label(), DESCRIPTION_PREVIEW)
                };
                row.set_subtitle(&preview);
            }
        });
        list.append(&described);

        let version = static_row("Version");
        let version_value = value_label();
        version.add_suffix(&version_value);
        list.append(&version);

        let source = static_row("Source");
        let source_value = value_label();
        source_value.set_label(&install_link::summary(url, &Details::Reading).source);
        source.add_suffix(&source_value);
        list.append(&source);

        // A row of its own, not the source's subtitle: a subtitle is laid out
        // before the suffix, so beside "Redirected from userscript-install-
        // tracker.vercel.app" the source was cut to "raw.gi… nt.com". And the
        // link's host goes under the title rather than beside it, where a
        // name that long was cut the same way: there it has the whole width.
        let redirect = static_row("Redirected from");
        redirect.set_visible(false);
        list.append(&redirect);

        let reach = static_row("Runs on");
        let reach_spinner = adw::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .valign(gtk::Align::Center)
            .build();
        let reach_warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
        reach_warning.add_css_class("warning");
        reach_warning.set_tooltip_text(Some("Warning"));
        let reach_value = value_label();
        reach.add_suffix(&reach_spinner);
        reach.add_suffix(&reach_warning);
        reach.add_suffix(&reach_value);
        list.append(&reach);

        let sites = adw::ExpanderRow::builder().title("Runs on").build();
        sites.set_use_markup(false);
        let sites_value = value_label();
        sites.add_suffix(&sites_value);
        list.append(&sites);

        let address = address_row(url);
        list.append(&address);

        Self {
            list,
            name,
            described,
            description,
            version,
            version_value,
            source_value,
            redirect,
            reach,
            reach_value,
            reach_warning,
            reach_spinner,
            sites,
            sites_value,
            listed: RefCell::new(Vec::new()),
            address: RefCell::new(address),
        }
    }

    /// Show `url` as the full address instead: the one *Add* will install
    /// from. Still the last row.
    fn readdress(&self, url: &str) {
        let address = address_row(url);
        self.list.remove(&self.address.replace(address.clone()));
        self.list.append(&address);
    }

    fn show(&self, summary: &Summary) {
        let note = summary.note.as_deref().unwrap_or("");
        let long = note.chars().count() > DESCRIPTION_PREVIEW;
        self.name.set_visible(!long);
        self.described.set_visible(long);

        self.name.set_title(&summary.name);
        self.name.set_subtitle(note);
        self.described.set_title(&summary.name);
        self.description.set_label(note);
        let preview = if self.described.is_expanded() {
            String::new()
        } else {
            install_link::shorten(note, DESCRIPTION_PREVIEW)
        };
        self.described.set_subtitle(&preview);

        self.version.set_visible(summary.version.is_some());
        self.version_value
            .set_label(summary.version.as_deref().unwrap_or(""));
        self.source_value.set_label(&summary.source);
        self.redirect.set_visible(summary.via.is_some());
        self.redirect.set_subtitle(summary.via.as_deref().unwrap_or(""));

        let reach = &summary.reach;
        let listing = !reach.sites().is_empty();
        self.reach.set_visible(!listing);
        self.sites.set_visible(listing);

        self.reach_value.set_label(&reach.value());
        self.reach.set_subtitle(reach.note().as_deref().unwrap_or(""));
        self.reach_spinner.set_visible(matches!(reach, Reach::Reading));
        // Icon and text both, so the warning does not rest on colour.
        self.reach_warning.set_visible(reach.warns());
        if reach.warns() {
            self.reach_value.add_css_class("warning");
        } else {
            self.reach_value.remove_css_class("warning");
        }

        self.sites_value.set_label(&reach.value());
        self.sites.set_subtitle(reach.note().as_deref().unwrap_or(""));
        for row in self.listed.borrow_mut().drain(..) {
            self.sites.remove(&row);
        }
        for site in reach.sites() {
            let row = static_row(site);
            self.sites.add_row(&row);
            self.listed.borrow_mut().push(row);
        }
    }
}

/// The line under the removal question. The difference between *off* and
/// *gone* is the whole of what it has to say.
const REMOVE_BODY: &str = "It will be deleted from AdGuard, not just switched off. To stop it \
                           running but keep it, switch it off instead.";

/// The rows under the removal question: the script, and what getting it back
/// would take.
fn removal_details(name: &str, source: Option<&str>) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");

    // Not markup, as everywhere here: a script's name is text somebody else
    // wrote.
    let named = static_row(name);
    named.set_title_lines(3);
    list.append(&named);

    let origin = static_row("Source");
    let value = value_label();
    list.append(&origin);

    match source {
        // Just the host, whole. The body already says it is gone for good,
        // and a host is short enough to need no cutting with nothing beside
        // it under the title.
        Some(url) => {
            value.set_label(&install_link::summary(url, &Details::Reading).source);
            value.set_ellipsize(gtk::pango::EllipsizeMode::None);
            origin.add_suffix(&value);
            list.append(&address_row(url));
        }
        // Worth saying plainly rather than softening. A script with no
        // recorded source cannot be re-fetched by this application at all.
        // Icon and text both, so the warning does not rest on colour.
        None => {
            let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
            warning.add_css_class("warning");
            origin.add_suffix(&warning);
            value.set_label("Not recorded");
            value.add_css_class("warning");
            origin.add_suffix(&value);
            origin.set_subtitle("There is no undo, and it cannot be added back from here");
        }
    }
    list
}

/// A row that states something. Not activatable and not focusable, so the
/// keyboard passes straight over it to the rows that do something and to the
/// buttons — and author text in the title is never read as markup.
fn static_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.set_use_markup(false);
    row.set_activatable(false);
    row.set_focusable(false);
    row
}

/// How much description fits two lines under the name, measured on the
/// dialog at its usual width: some fifty characters a line, less where the
/// words break badly — Greasy Fork's 87-character preview for #29's example
/// took three. Longer ones get a row that expands to the whole text,
/// previewed up to the last whole word within this.
const DESCRIPTION_PREVIEW: usize = 80;

/// The value beside a row's title, on one line.
///
/// Never wrapped: "1.0 / .18" and "update.gre- / asyfork.org" broken across
/// lines are harder to read than anything they save. The titles beside them
/// are single words, so a value has room; one that still does not fit is cut
/// in the middle — a host keeps both ends, which are the parts that identify
/// it — and the whole of it is in the tooltip.
fn value_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .use_markup(false)
        .xalign(1.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .valign(gtk::Align::Center)
        .build();
    label.connect_label_notify(|label| {
        let text = label.label();
        label.set_tooltip_text((!text.is_empty()).then_some(text.as_str()));
    });
    label
}

/// The URL in full, folded away: the name and source are what a person
/// decides on, and the escaped URL is mostly noise — but it is there,
/// selectable, copyable and openable, for anyone checking exactly what AdGuard
/// fetches. Opened, it is the script's source in the browser, which is the
/// way to read it before saying yes.
fn address_row(url: &str) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder().title("Full address").build();
    row.set_use_markup(false);

    let address = adw::ActionRow::builder().title(url).build();
    address.set_use_markup(false);
    address.set_title_lines(0);
    address.set_title_selectable(true);
    address.add_css_class("caption");
    // The way the About page's links open, with the same icon.
    address.set_activatable(true);
    address.set_tooltip_text(Some("Open in the browser"));
    let uri = url.to_owned();
    address.connect_activated(move |row| {
        let launcher = gtk::UriLauncher::new(&uri);
        let window = row.root().and_downcast::<gtk::Window>();
        glib::spawn_future_local(async move {
            // Nothing to report on failure: the address is on screen, in
            // full, with a copy button beside it.
            let _ = launcher.launch_future(window.as_ref()).await;
        });
    });

    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.add_css_class("flat");
    copy.set_valign(gtk::Align::Center);
    copy.set_tooltip_text(Some("Copy address"));
    let text = url.to_owned();
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&text);
        button.set_tooltip_text(Some("Copied"));
    });
    address.add_suffix(&copy);
    address.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));

    row.add_row(&address);
    row
}

/// How long Add stays greyed out after an install link's dialog appears, or
/// after its window comes back into focus.
const ADD_DELAY: u32 = 2;

/// The greyed-out, counting-down state of one dialog response.
///
/// Holds a weak reference to the dialog, so a timer still ticking when the
/// dialog closes finds nothing to touch and stops.
#[derive(Clone)]
struct Countdown(Rc<CountdownInner>);

struct CountdownInner {
    dialog: glib::WeakRef<adw::AlertDialog>,
    response: &'static str,
    label: &'static str,
    remaining: Cell<u32>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Held off for a reason other than time — the script still being read.
    blocked: Cell<bool>,
}

impl Countdown {
    fn new(dialog: &adw::AlertDialog, response: &'static str, label: &'static str) -> Self {
        Self(Rc::new(CountdownInner {
            dialog: dialog.downgrade(),
            response,
            label,
            remaining: Cell::new(ADD_DELAY),
            timer: RefCell::new(None),
            blocked: Cell::new(false),
        }))
    }

    /// Keep the response greyed out whatever the count says, or stop doing so.
    fn set_blocked(&self, blocked: bool) {
        self.0.blocked.set(blocked);
        self.paint();
    }

    /// Grey the response out and count down from the top.
    fn start(&self) {
        self.hold();
        let this = self.clone();
        let timer = glib::timeout_add_local(Duration::from_secs(1), move || {
            let left = this.0.remaining.get().saturating_sub(1);
            this.0.remaining.set(left);
            this.paint();
            if left == 0 {
                this.0.timer.take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        self.0.timer.replace(Some(timer));
    }

    /// Grey the response out and stop counting, until [`Self::start`].
    fn hold(&self) {
        if let Some(timer) = self.0.timer.take() {
            timer.remove();
        }
        self.0.remaining.set(ADD_DELAY);
        self.paint();
    }

    fn paint(&self) {
        let Some(dialog) = self.0.dialog.upgrade() else {
            return;
        };
        let left = self.0.remaining.get();
        dialog.set_response_enabled(self.0.response, left == 0 && !self.0.blocked.get());
        let label = match left {
            0 => self.0.label.to_owned(),
            left => format!("{} ({left})", self.0.label),
        };
        dialog.set_response_label(self.0.response, &label);
    }
}

fn read(locale: &Locale) -> Result<Loaded, String> {
    let config = Config::load().map_err(|err| err.to_string())?;
    let dir = adguard_core::paths::userscripts_dir()
        .ok_or_else(|| "Could not locate AdGuard's data directory".to_owned())?;
    let mut scripts = userscripts::read(&dir, &config.enabled_userscripts(), locale);
    // This application's own layer over AdGuard's rows: the names the user
    // gave them, and whether the source still matches what AdGuard delivered.
    // Neither is fatal to read — without them the rows say what AdGuard says.
    if let Some(aliases) = Aliases::locate() {
        aliases.apply(&mut scripts);
    }
    let copies = Copies::locate();
    if let Some(copies) = &copies {
        copies.observe_all(&dir, &mut scripts);
    }
    // After the observation, which is what files an update that just landed:
    // only then does a replaced edit read as pending.
    let mut pending = HashMap::new();
    let mut bare = HashSet::new();
    for script in &scripts {
        if adguard_core::update_crash::is_bare(&dir, &script.id) {
            bare.insert(script.id.clone());
        }
        let Some(copies) = &copies else {
            continue;
        };
        let Ok(current) = std::fs::read(userscripts::source_path(&dir, &script.id)) else {
            continue;
        };
        if let Ok(Some(state)) = copies.pending(&script.id, script.version.as_deref(), &current) {
            pending.insert(script.id.clone(), state);
        }
    }
    let offered = userscripts::recommended(&scripts);
    let install_links = userscripts::install_links(&scripts);
    Ok(Loaded {
        scripts,
        offered,
        install_links,
        pending,
        bare,
    })
}

/// Take what is installed for `id` now as AdGuard's own copy. Run on the
/// worker after AdGuard has just fetched the script; a failure costs only the
/// accuracy of the *Edited* note, which the next comparison may then get wrong
/// in the harmless direction.
///
/// `discard_edit` only after a reinstall the user confirmed knowing their
/// changes would be replaced. Re-adding an installed address replaces the file
/// too, but nothing asked them about their edit, so it is kept.
fn rebase(id: &str, discard_edit: bool) {
    let (Some(dir), Some(copies)) = (adguard_core::paths::userscripts_dir(), Copies::locate()) else {
        return;
    };
    let Ok(current) = std::fs::read(userscripts::source_path(&dir, id)) else {
        return;
    };
    let version = userscripts::read(&dir, &[], &Locale::english())
        .into_iter()
        .find(|script| script.id == id)
        .and_then(|script| script.version);
    let _ = copies.rebase(id, version.as_deref(), &current);
    if discard_edit {
        let _ = copies.discard_edit(id);
    }
}

/// Drop this application's name and copies for a script that was removed.
/// Best effort: a leftover is a few files nobody reads, not a fault.
fn forget(id: &str) {
    if let Some(aliases) = Aliases::locate() {
        let _ = aliases.set(id, None);
    }
    if let Some(copies) = Copies::locate() {
        let _ = copies.forget(id);
    }
}

/// Is `id` among these `meta:` paths? The config holds a path and the page
/// holds an id, so the join is on the filename.
fn is_enabled(id: &str, enabled: &[&str]) -> bool {
    let wanted = format!("{id}.meta.json");
    enabled.iter().any(|meta| {
        std::path::Path::new(meta.trim())
            .file_name()
            .is_some_and(|name| name == wanted.as_str())
    })
}

/// What a row says under its name.
///
/// The version leads, because it is what #9 asks for and what tells one install
/// of a script from another. A script whose source carried no `@version` shows
/// its description alone rather than a stray dash — absence is a state here,
/// not a blank to print.
fn subtitle(script: &Userscript) -> String {
    if script.ambiguous {
        return AMBIGUOUS_SUBTITLE.to_owned();
    }
    let description = script.description.trim();
    match (&script.version, description.is_empty()) {
        (Some(version), false) => format!("{version} — {description}"),
        (Some(version), true) => format!("Version {version}"),
        (None, false) => description.to_owned(),
        (None, true) => "No description".to_owned(),
    }
}

/// The cog and the entries on it that a row shows and hides.
struct Menu {
    menu: gtk::MenuButton,
    revert: gtk::Button,
    update: gtk::Button,
    restore: gtk::Button,
}

/// How often the page checks for updates while the application runs.
const DAILY: u32 = 24 * 60 * 60;

/// What a row says instead of its description when `check-update` would
/// crash on it (contract §15). Displaces the description, as
/// [`AMBIGUOUS_SUBTITLE`] does and for its reason.
const BARE_SUBTITLE: &str = "AdGuard crashes when it updates this script, which names no address \
                             to update from — switch it off or remove it";

/// How carrying a kept edit into the installed source went ([`carry`]).
#[derive(Debug)]
enum Carry {
    /// Nothing was pending.
    Nothing,
    /// The edit is installed again.
    Kept,
    /// It clashes with the release; `marked` is the merge with git's markers,
    /// `None` when the copies could not be merged as text at all.
    Clash { marked: Option<String>, conflicts: usize },
    /// The source changed between the read and the write, and was left alone.
    Moved,
    Failed(String),
}

/// Carry `id`'s kept edit into its installed source, on the worker
/// ([`Copies::carry_over`], then [`Copies::put_source`]).
fn carry(id: &str) -> Carry {
    let (Some(dir), Some(copies)) = (adguard_core::paths::userscripts_dir(), Copies::locate()) else {
        return Carry::Failed("the copies cannot be found".to_owned());
    };
    let path = userscripts::source_path(&dir, id);
    let current = match std::fs::read(&path) {
        Ok(current) => current,
        Err(err) => return Carry::Failed(err.to_string()),
    };
    let version = userscripts::read(&dir, &[], &Locale::english())
        .into_iter()
        .find(|script| script.id == id)
        .and_then(|script| script.version);
    let label = version.clone().unwrap_or_else(|| "the update".to_owned());
    match copies.carry_over(id, version.as_deref(), &current, &label) {
        Ok(None) => Carry::Nothing,
        Ok(Some(Carried::Clean(merged))) => match copies.put_source(id, &path, &current, &merged) {
            Ok(true) => Carry::Kept,
            Ok(false) => Carry::Moved,
            Err(err) => Carry::Failed(err.to_string()),
        },
        Ok(Some(Carried::Conflict { marked, conflicts })) => Carry::Clash {
            marked: Some(marked),
            conflicts,
        },
        Ok(Some(Carried::Unmergeable)) => Carry::Clash {
            marked: None,
            conflicts: 0,
        },
        Err(err) => Carry::Failed(err.to_string()),
    }
}

/// *Keep my version*: the kept edit, installed as it was.
fn keep_mine(id: &str) -> Carry {
    let (Some(dir), Some(copies)) = (adguard_core::paths::userscripts_dir(), Copies::locate()) else {
        return Carry::Failed("the copies cannot be found".to_owned());
    };
    let path = userscripts::source_path(&dir, id);
    let (Ok(current), Ok(mine)) = (std::fs::read(&path), copies.kept_edit(id)) else {
        return Carry::Failed("your changes could not be read".to_owned());
    };
    match copies.put_source(id, &path, &current, &mine) {
        Ok(true) => Carry::Kept,
        Ok(false) => Carry::Moved,
        Err(err) => Carry::Failed(err.to_string()),
    }
}

/// A row's subtitle, with what the page knows beyond the script itself.
///
/// One sentence at most displaces the description, in the order a user needs
/// them: a control that cannot work, then a script that breaks AdGuard's
/// update, then changes an update replaced. A newer version joins the
/// description rather than displacing it, after the installed version.
fn row_subtitle(script: &Userscript, available: Option<&str>, pending: Option<Pending>, bare: bool) -> String {
    if script.ambiguous {
        return AMBIGUOUS_SUBTITLE.to_owned();
    }
    if bare && script.enabled {
        return BARE_SUBTITLE.to_owned();
    }
    match pending {
        Some(Pending::Overwritten) => {
            return "An update replaced your changes to the source — bring them back from the menu"
                .to_owned();
        }
        Some(Pending::SetAside) => {
            let version = script.version.as_deref().map_or_else(String::new, |v| format!(" to {v}"));
            return format!(
                "Updated{version}; your changes to the source are kept aside — bring them back from \
                 the menu"
            );
        }
        None => {}
    }
    match (available, &script.version) {
        (Some(available), Some(installed)) => {
            let description = script.description.trim();
            if description.is_empty() {
                format!("{installed} → {available} available")
            } else {
                format!("{installed} → {available} available — {description}")
            }
        }
        _ => subtitle(script),
    }
}

/// Relabel a [`menu_item`].
fn set_menu_label(button: &gtk::Button, label: &str) {
    if let Some(title) = button
        .child()
        .and_then(|content| content.first_child())
        .and_downcast::<gtk::Label>()
    {
        title.set_label(label);
    }
}

/// What *Check now* says when it is done. "Up to date" only when every
/// script answered: a check that could not reach a server verified nothing,
/// and offline, every one of them fails.
fn check_summary(found: usize, unknown: usize) -> String {
    let found = match found {
        0 => None,
        1 => Some("An update is available for one userscript".to_owned()),
        n => Some(format!("Updates are available for {n} userscripts")),
    };
    let missed = match unknown {
        0 => None,
        1 => Some("one could not be checked".to_owned()),
        n => Some(format!("{n} could not be checked")),
    };
    match (found, missed) {
        (None, None) => "Every userscript is up to date".to_owned(),
        (Some(found), None) => found,
        (Some(found), Some(missed)) => format!("{found}; {missed}"),
        (None, Some(missed)) => format!("No updates found, but {missed}"),
    }
}

/// The local time, for "Checked at 14:05".
fn clock() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|when| when.format("at %H:%M").ok())
        .map_or_else(|| "just now".to_owned(), |when| when.to_string())
}

/// What a catalogue row says under its name.
///
/// AdGuard's description, plus a word about where it will land when the default
/// is *off* — the button says "Add" either way, and a script that arrives
/// switched off would otherwise look like an install that half-worked.
///
/// No version here: nothing knows one until the script is fetched, and the URLs
/// are channels rather than pinned releases (`RECOMMENDED`). Printing the
/// version from the table would be quoting a number this application made up.
fn offered_subtitle(entry: &adguard_core::Recommended) -> String {
    if entry.enabled_by_default {
        return entry.description.to_owned();
    }
    // AdGuard writes some of these descriptions with a closing full stop and
    // some without, so the dash has to join onto either without leaving
    // "experience. — added" in the middle of a sentence.
    let description = entry.description.trim_end_matches('.');
    format!("{description} — added switched off, as AdGuard ships it")
}

/// A row that states something rather than doing anything.
fn inert_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.set_use_markup(false);
    row.set_subtitle_lines(2);
    row
}

/// One entry in the cog's popover, with an optional dim note at its end —
/// the host a link opens, so *Script page* says `greasyfork.org` before it is
/// clicked.
fn menu_item(label: &str, note: Option<&str>) -> gtk::Button {
    let content = gtk::Box::builder().spacing(18).build();
    // Left-aligned like a menu, rather than centred like a button.
    let title = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    content.append(&title);
    if let Some(note) = note {
        let note = gtk::Label::builder()
            .label(note)
            .xalign(1.0)
            .use_markup(false)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(28)
            .build();
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        content.append(&note);
    }
    let button = gtk::Button::builder()
        .child(&content)
        .halign(gtk::Align::Fill)
        .build();
    button.add_css_class("flat");
    button
}

/// A rule between groups of entries in the cog's popover.
fn menu_separator() -> gtk::Separator {
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(4);
    separator.set_margin_bottom(4);
    separator
}

/// The *Edited* note on a row: an icon and the word, so it does not rest on
/// the icon alone, dim so it does not compete with the name.
fn edited_note() -> gtk::Box {
    let note = gtk::Box::builder()
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    note.append(&gtk::Image::from_icon_name("document-edit-symbolic"));
    note.append(&gtk::Label::new(Some("Edited")));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_tooltip_text(Some(
        "The source differs from the copy AdGuard installed. Revert to original is in the menu.",
    ));
    note
}

/// Whether two addresses open the same page, ignoring the differences no
/// server cares about — the scheme's case, `www.` and a trailing slash — so
/// the cog does not offer the same page twice under two names.
fn same_address(a: &str, b: &str) -> bool {
    let normal = |url: &str| {
        let url = url.trim().trim_end_matches('/').to_ascii_lowercase();
        let url = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .map(str::to_owned)
            .unwrap_or(url);
        url.strip_prefix("www.").map(str::to_owned).unwrap_or(url)
    };
    normal(a) == normal(b)
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
        .title("Extensions unavailable")
        .description(message)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(id: &str) -> Userscript {
        Userscript {
            id: id.to_owned(),
            name: String::new(),
            description: String::new(),
            version: None,
            homepage: None,
            download_url: None,
            update_url: None,
            enabled: false,
            ambiguous: false,
            alias: None,
            edited: false,
        }
    }

    /// The version leads the subtitle, because that is what #9 asks to show.
    /// "Up to date" is said only when every check answered.
    #[test]
    fn a_failed_check_is_not_up_to_date() {
        assert_eq!(check_summary(0, 0), "Every userscript is up to date");
        assert_eq!(check_summary(0, 3), "No updates found, but 3 could not be checked");
        assert!(!check_summary(0, 1).contains("Every"));
        assert_eq!(check_summary(2, 1), "Updates are available for 2 userscripts; one could not be checked");
    }

    /// A newer version joins the description after the installed one.
    #[test]
    fn an_available_update_joins_the_description() {
        let mut s = script("x");
        s.version = Some("2.7.1".to_owned());
        s.description = "Downloads things".to_owned();
        assert_eq!(row_subtitle(&s, Some("2.7.2"), None, false), "2.7.1 → 2.7.2 available — Downloads things");
        assert_eq!(row_subtitle(&s, None, None, false), subtitle(&s));
    }

    /// The crash caveat shows only while the script is on, since a switched-off
    /// script is skipped by `check-update` and cannot crash it.
    #[test]
    fn the_crash_caveat_follows_the_switch() {
        let mut s = script("x");
        s.enabled = true;
        assert_eq!(row_subtitle(&s, Some("2"), None, true), BARE_SUBTITLE);
        s.enabled = false;
        assert_ne!(row_subtitle(&s, None, None, true), BARE_SUBTITLE);
    }

    /// What displaces the description, in order: an unusable control, a
    /// crashing update, then replaced changes.
    #[test]
    fn the_most_pressing_note_wins() {
        let mut s = script("x");
        s.enabled = true;
        s.version = Some("2.0".to_owned());
        assert!(row_subtitle(&s, None, Some(Pending::SetAside), false).starts_with("Updated to 2.0;"));
        assert!(row_subtitle(&s, None, Some(Pending::Overwritten), false).starts_with("An update replaced"));
        assert_eq!(row_subtitle(&s, None, Some(Pending::Overwritten), true), BARE_SUBTITLE);
        s.ambiguous = true;
        assert_eq!(row_subtitle(&s, None, Some(Pending::Overwritten), true), AMBIGUOUS_SUBTITLE);
    }

    #[test]
    fn the_subtitle_leads_with_the_version() {
        let mut s = script("x");
        s.version = Some("1.1.36".to_owned());
        s.description = "Blocks pop-up ads".to_owned();
        assert_eq!(subtitle(&s), "1.1.36 — Blocks pop-up ads");
    }

    /// A script with no `@version` shows its description alone. The issue asks
    /// for the version *when it is available*, and a leading dash over nothing
    /// would be worse than saying less.
    #[test]
    fn a_missing_version_leaves_no_stray_punctuation() {
        let mut s = script("x");
        s.description = "Blocks pop-up ads".to_owned();
        assert_eq!(subtitle(&s), "Blocks pop-up ads");
    }

    /// Neither field is guaranteed — nothing validates a userscript's metadata.
    #[test]
    fn a_bare_script_still_says_something() {
        assert_eq!(subtitle(&script("x")), "No description");

        let mut versioned = script("x");
        versioned.version = Some("2.0".to_owned());
        assert_eq!(subtitle(&versioned), "Version 2.0");
    }

    /// The caveat displaces the description rather than joining it, so the
    /// reason a row is inert cannot be the half that gets ellipsised away.
    #[test]
    fn an_ambiguous_row_says_why_instead_of_what() {
        let mut s = script("hello");
        s.version = Some("1.0".to_owned());
        s.description = "Something useful".to_owned();
        s.ambiguous = true;
        assert_eq!(subtitle(&s), AMBIGUOUS_SUBTITLE);
        assert!(!subtitle(&s).contains("Something useful"));
    }

    /// A catalogue row whose default is *off* says so, because the button says
    /// "Add" either way and a script arriving switched off would otherwise read
    /// as an install that half-worked.
    #[test]
    fn an_off_by_default_catalogue_row_says_where_it_lands() {
        let wot = adguard_core::RECOMMENDED
            .iter()
            .find(|entry| entry.id == "wot")
            .expect("Web of Trust is in the catalogue");
        let subtitle = offered_subtitle(wot);
        assert!(subtitle.contains("switched off"));
        // Its description ends in a full stop; the dash must not follow one.
        assert!(
            !subtitle.contains(". —"),
            "the note ran onto a finished sentence: {subtitle}"
        );

        let extra = adguard_core::RECOMMENDED
            .iter()
            .find(|entry| entry.id == "adguard-extra")
            .expect("AdGuard Extra is in the catalogue");
        assert_eq!(offered_subtitle(extra), extra.description);
    }

    /// The config holds paths; the page holds ids.
    #[test]
    fn enabled_matches_on_the_filename() {
        assert!(is_enabled("adguard-extra", &["userscripts/adguard-extra.meta.json"]));
        assert!(!is_enabled("adguard", &["userscripts/adguard-extra.meta.json"]));
        assert!(!is_enabled("x", &[]));
    }
}
