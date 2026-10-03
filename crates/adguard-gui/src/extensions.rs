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
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use adguard_core::{userscripts, Cli, Config, Locale, Userscript, INSTALL_LINKS};
use adw::prelude::*;
use gtk::glib;
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
    /// This application's own script for browser install links, while it is
    /// not installed.
    install_links: Option<&'static adguard_core::Recommended>,
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
    /// The cog, on the rows that have anything to put in it.
    menu: Option<gtk::MenuButton>,
}

impl Row {
    /// Grey out, or restore, everything on this row that writes.
    ///
    /// One place rather than three call sites, because a row that is half
    /// fenced is worse than one that is not fenced at all: the switch would be
    /// safe and the trash would not.
    fn set_busy(&self, busy: bool) {
        self.switch.set_sensitive(!busy);
        self.remove.set_sensitive(!busy);
        if let Some(menu) = &self.menu {
            menu.set_sensitive(!busy);
        }
    }
}

/// What an unreachable row says instead of its description.
///
/// It **displaces** the script's own description rather than joining it, for
/// the reason the Filters page displaces one with its trusted caveat: two
/// sentences sharing a two-line ellipsised subtitle leave whichever came second
/// truncated, and the fact that a control does nothing outranks the script's
/// account of itself.
const AMBIGUOUS_SUBTITLE: &str =
    "AdGuard cannot tell this apart from another installed script, so it cannot be \
     switched or removed — rename or remove the other one";

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

pub struct ExtensionsPage {
    /// Swapped wholesale — spinner, error, or the list — which is simpler and
    /// less error-prone than reconciling child lists.
    bin: adw::Bin,
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
}

impl ExtensionsPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            bin: adw::Bin::new(),
            cli,
            toasts,
            locale: Locale::from_env(),
            rows: RefCell::new(HashMap::new()),
            reconciling: Cell::new(false),
            offers: RefCell::new(VecDeque::new()),
            asking: RefCell::new(None),
            hinted: Cell::new(false),
        });
        this.reload();
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.bin
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
                }
                Err(err) => {
                    this.rows.borrow_mut().clear();
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
                moved += 1;
            }
        }
        self.reconciling.set(false);
        moved
    }

    /// The add row, then one group holding every script.
    fn view(self: &Rc<Self>, loaded: &Loaded) -> gtk::Widget {
        self.rows.borrow_mut().clear();

        let page = adw::PreferencesPage::new();
        page.add(&self.add_group());
        // Directly under the address field: the two are ways of doing the same
        // thing, and the helper is the one that saves typing the address.
        if let Some(group) = self.install_links_group(loaded.install_links) {
            page.add(&group);
        }
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
            glib::spawn_future_local(async move { this.install(url, field).await });
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
    fn install_links_group(
        self: &Rc<Self>,
        entry: Option<&'static adguard_core::Recommended>,
    ) -> Option<adw::PreferencesGroup> {
        let entry = entry?;
        let group = adw::PreferencesGroup::builder()
            .title("From your browser")
            .description(
                "With this added, the Install button on Greasy Fork, Sleazy Fork, OpenUserJS \
                 and GitHub brings the script here to be added, instead of opening it as text. \
                 It needs HTTPS filtering for your browser, and the browser asks once whether \
                 to open AdGuard UI.",
            )
            .build();
        group.add(&self.offer_row(entry));
        Some(group)
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
        let switch = adw::SwitchRow::builder()
            .title(script.display_name())
            .subtitle(subtitle(script))
            .active(script.enabled)
            .build();
        // A userscript's name and description are written by whoever wrote the
        // script. `&` is ordinary in both, and `use-markup` defaults to true —
        // left on, Pango fails the whole string and the row renders mangled.
        switch.set_use_markup(false);
        switch.set_subtitle_lines(2);

        // The same glyph the Protection, DNS and Filters rows use for a
        // consequence the row would not otherwise disclose. Undecorated for
        // accessibility, as those are: the subtitle says it in words, and
        // labelling the image would announce it twice.
        let caveat = gtk::Image::from_icon_name("dialog-warning-symbolic");
        caveat.set_visible(script.ambiguous);
        switch.add_prefix(&caveat);

        let remove = self.remove_button(script);
        let menu = self.menu_button(script);

        // Order matters: the trash goes last so it is furthest from the switch,
        // the two controls that must never be confused sitting at opposite ends
        // of the row's suffix area.
        if let Some(menu) = &menu {
            switch.add_suffix(menu);
        }
        switch.add_suffix(&remove);

        if script.ambiguous {
            // Everything that writes is unreachable for this script, so nothing
            // that writes is offered. The row still reads — its name, version
            // and state are all true, and hiding it would be a worse lie than
            // showing a control that does not work.
            switch.set_sensitive(false);
            remove.set_sensitive(false);
            if let Some(menu) = &menu {
                // The homepage would be safe, but a half-live cog invites a
                // second look for the entry that is missing. The link is not
                // worth that.
                menu.set_sensitive(false);
            }
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

        self.rows.borrow_mut().insert(
            script.id.clone(),
            Row {
                switch: switch.clone(),
                script: RefCell::new(script.clone()),
                remove,
                menu,
            },
        );

        switch
    }

    /// The cog: a homepage to open, and a reinstall.
    ///
    /// `None` when the script offers neither, which is the ordinary case for
    /// one installed from a bare URL with no `@homepage`. An empty menu button
    /// is a control that opens onto nothing.
    ///
    /// *Edit* and *Storage*, which AdGuard for Windows also offers here, are
    /// out by decision rather than provisionally — `architecture.md` §7.
    fn menu_button(self: &Rc<Self>, script: &Userscript) -> Option<gtk::MenuButton> {
        let has_home = script.homepage.is_some();
        let has_source = script.download_url.is_some();
        if !has_home && !has_source {
            return None;
        }

        let items = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let popover = gtk::Popover::builder().child(&items).build();

        if let Some(homepage) = &script.homepage {
            let button = menu_item("Homepage");
            let uri = homepage.clone();
            let popover_ = popover.clone();
            button.connect_clicked(move |button| {
                popover_.popdown();
                // The launcher, not a hand-rolled `xdg-open`: it is what the
                // About page's links already use and it respects the portal.
                gtk::UriLauncher::new(&uri).launch(
                    button.root().and_downcast::<gtk::Window>().as_ref(),
                    gtk::gio::Cancellable::NONE,
                    |_| {},
                );
            });
            items.append(&button);
        }

        if script.download_url.is_some() {
            let button = menu_item("Reinstall");
            let this = Rc::downgrade(self);
            let id = script.id.clone();
            let popover_ = popover.clone();
            button.connect_clicked(move |_| {
                popover_.popdown();
                let Some(this) = this.upgrade() else {
                    return;
                };
                this.begin_reinstall(&id);
            });
            items.append(&button);
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
        Some(menu)
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
    /// The wording names the source URL when there is one, because that is the
    /// only thing that can bring the script back — and points at the switch,
    /// because "stop it running" is what most people reaching for this actually
    /// want.
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

        let undo = match &source {
            Some(url) => format!("There is no undo: getting it back means adding {url} again."),
            // Worth saying plainly rather than softening. A script with no
            // recorded source cannot be re-fetched by this application at all.
            None => "There is no undo, and AdGuard did not record where this one came from — \
                     it cannot be reinstalled from here."
                .to_owned(),
        };

        let dialog = adw::AlertDialog::new(
            Some("Remove this userscript?"),
            Some(&format!(
                "{name} will be deleted from AdGuard, not just switched off. {undo}\n\n\
                 To stop it running without losing it, switch it off instead."
            )),
        );
        // Not markup: a script's name is text somebody else wrote.
        dialog.set_body_use_markup(false);
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
        let (name, url, enabled) = {
            let rows = self.rows.borrow();
            let Some(row) = rows.get(id) else {
                return false;
            };
            let script = row.script.borrow();
            let Some(url) = script.download_url.clone() else {
                return false;
            };
            (script.display_name().to_owned(), url, script.enabled)
        };

        let mut body = format!(
            "AdGuard will fetch {name} from {url} again and replace the installed copy \
             with whatever is there now."
        );
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
        worker::run(
            move || {
                let refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
                let observed = read(&locale)
                    .ok()
                    .and_then(|loaded| loaded.scripts.into_iter().find(|s| s.id == wanted));
                (refused, observed)
            },
            move |(refused, observed)| {
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
                if this.confirm_install(&url).await {
                    this.install(url, None).await;
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
    async fn confirm_install(&self, url: &str) -> bool {
        let dialog = adw::AlertDialog::new(Some("Add this userscript?"), Some(INSTALL_BODY));
        dialog.set_body_use_markup(false);
        // Room for the rows to read as rows rather than as a column of
        // wrapped fragments.
        dialog.set_prefer_wide_layout(true);
        let details = Rc::new(InstallDetails::new(url));
        details.show(&install_link::summary(url, &Details::Reading));
        dialog.set_extra_child(Some(&details.list));
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
        {
            let url = url.to_owned();
            let countdown = countdown.clone();
            let details_view = details.clone();
            glib::spawn_future_local(async move {
                let fetched = {
                    let url = url.clone();
                    worker::job(move || preview::fetch(&url)).await
                };
                let details = match fetched {
                    Some(Ok(preview)) => Details::Read(preview),
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
        answer == "install"
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
    async fn install(self: &Rc<Self>, url: String, field: Option<(adw::EntryRow, adw::Spinner)>) {
        let typed = field.is_some();
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
            let refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
            let after = read(&locale).ok().map(|loaded| loaded.scripts);
            (refused, before, after)
        })
        .await;

        let Some((refused, before, Some(after))) = outcome else {
            set_busy(false, false);
            let refused = outcome.and_then(|(refused, _, _)| refused);
            self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                "Could not re-read the userscripts to confirm the install".to_owned()
            })));
            self.reload();
            return;
        };

        match after.iter().find(|s| !before.contains(&s.id)) {
            Some(new) => {
                set_busy(false, true);
                let added = format!("Added {}", new.display_name());
                let helper_missing = userscripts::install_links(&after).is_some();
                if typed && helper_missing && !self.hinted.replace(true) {
                    self.toasts.add_toast(self.helper_hint(&added));
                } else {
                    self.toasts.add_toast(toast(&added));
                }
            }
            // Nothing new. Either it was refused, or the URL was one already
            // installed and this was an update in place — which the CLI
            // reports identically, so the row count is what tells them apart.
            None => {
                set_busy(false, false);
                self.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                    "That userscript was already installed; AdGuard updated it in place"
                        .to_owned()
                })));
            }
        }
        self.reload();
    }

    /// The toast after an address was typed in, telling the user the browser
    /// could have done it (#29).
    ///
    /// This is the moment the helper is worth mentioning: someone has just
    /// copied a script's address out of a browser and pasted it here, which is
    /// exactly what it saves. Once per run of the application, and only while
    /// the helper is not installed — a reminder, not a campaign. The button
    /// adds it the way the *From your browser* row does.
    fn helper_hint(self: &Rc<Self>, added: &str) -> adw::Toast {
        let hint = adw::Toast::builder()
            .use_markup(false)
            .title(format!("{added}. Next time, add scripts straight from your browser"))
            .button_label("Add Helper")
            .timeout(10)
            .build();
        let this = Rc::downgrade(self);
        hint.connect_button_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.add_recommended(&INSTALL_LINKS);
            }
        });
        hint
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

        list.append(&address_row(url));

        Self {
            list,
            name,
            described,
            description,
            version,
            version_value,
            source_value,
            reach,
            reach_value,
            reach_warning,
            reach_spinner,
            sites,
            sites_value,
            listed: RefCell::new(Vec::new()),
        }
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
/// selectable and copyable, for anyone checking exactly what AdGuard fetches.
fn address_row(url: &str) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder().title("Full address").build();
    row.set_use_markup(false);

    let address = adw::ActionRow::builder().title(url).build();
    address.set_use_markup(false);
    address.set_title_lines(0);
    address.set_title_selectable(true);
    address.set_activatable(false);
    address.add_css_class("caption");

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
    let scripts = userscripts::read(&dir, &config.enabled_userscripts(), locale);
    let offered = userscripts::recommended(&scripts);
    let install_links = userscripts::install_links(&scripts);
    Ok(Loaded {
        scripts,
        offered,
        install_links,
    })
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

/// One entry in the cog's popover.
fn menu_item(label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Fill)
        .build();
    button.add_css_class("flat");
    // Left-aligned like a menu, rather than centred like a button.
    if let Some(child) = button.child().and_downcast::<gtk::Label>() {
        child.set_xalign(0.0);
    }
    button
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
            enabled: false,
            ambiguous: false,
        }
    }

    /// The version leads the subtitle, because that is what #9 asks to show.
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
