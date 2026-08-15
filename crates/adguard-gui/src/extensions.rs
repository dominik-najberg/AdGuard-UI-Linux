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
use std::collections::HashMap;
use std::rc::Rc;

use adguard_core::{userscripts, Cli, Config, Locale, Userscript};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{toast, worker};

/// One read of everything the page renders.
struct Loaded {
    scripts: Vec<Userscript>,
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

/// The placeholder on the add row.
///
/// It says http(s) because `userscripts install` refuses everything else —
/// measured, a local path and a `file://` URL are both rejected with the same
/// unhelpful sentence (contract §15). Saying so here is cheaper than letting a
/// user discover it by having a paste fail.
const ADD_PLACEHOLDER: &str = "https://example.org/script.user.js";

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
            .description(
                "AdGuard fetches userscripts over the web, so this takes an http or https \
                 address — a file on this computer cannot be installed.",
            )
            .build();

        let entry = adw::EntryRow::builder()
            .title("Userscript URL")
            .show_apply_button(true)
            .build();
        // The one place this page shows a string the user is about to act on;
        // `&` in a URL is ordinary and markup would mangle it.
        entry.set_use_markup(false);
        // `AdwEntryRow` has no placeholder property of its own; the inner
        // editable does, and this is how the DNS page sets one too.
        entry.set_property("placeholder-text", ADD_PLACEHOLDER);

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
            this.install(url, entry.clone(), spinner.clone());
        });

        group.add(&entry);
        group
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

    /// Fetch and install a userscript, then confirm it against the directory.
    ///
    /// The id is assigned by AdGuard from the filename, so — as with a custom
    /// filter — it cannot be known in advance: the scripts are read before and
    /// after, and one that was not there before is the evidence. An id that was
    /// *already* there is the reinstall case, which is a legitimate outcome of
    /// pasting a URL twice and is reported as such rather than as a failure.
    fn install(self: &Rc<Self>, url: String, entry: adw::EntryRow, spinner: adw::Spinner) {
        entry.set_sensitive(false);
        spinner.set_visible(true);

        let cli = self.cli.clone();
        let locale = self.locale.clone();
        let this = self.clone();
        worker::run(
            move || {
                let before: Vec<String> = read(&locale)
                    .map(|loaded| loaded.scripts.into_iter().map(|s| s.id).collect())
                    .unwrap_or_default();
                let refused = cli.userscripts_install(&url).err().map(|e| e.to_string());
                let after = read(&locale).ok().map(|loaded| loaded.scripts);
                (refused, before, after)
            },
            move |(refused, before, after)| {
                entry.set_sensitive(true);
                spinner.set_visible(false);

                let Some(after) = after else {
                    this.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                        "Could not re-read the userscripts to confirm the install".to_owned()
                    })));
                    this.reload();
                    return;
                };

                match after.iter().find(|s| !before.contains(&s.id)) {
                    Some(new) => {
                        entry.set_text("");
                        this.toasts
                            .add_toast(toast(&format!("Added {}", new.display_name())));
                        this.reload();
                    }
                    // Nothing new. Either it was refused, or the URL was one
                    // already installed and this was an update in place — which
                    // the CLI reports identically, so the row count is what
                    // tells them apart.
                    None => {
                        this.toasts.add_toast(toast(&refused.unwrap_or_else(|| {
                            "That userscript was already installed; AdGuard updated it in place"
                                .to_owned()
                        })));
                        this.reload();
                    }
                }
            },
        );
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
fn read(locale: &Locale) -> Result<Loaded, String> {
    let config = Config::load().map_err(|err| err.to_string())?;
    let dir = adguard_core::paths::userscripts_dir()
        .ok_or_else(|| "Could not locate AdGuard's data directory".to_owned())?;
    let scripts = userscripts::read(&dir, &config.enabled_userscripts(), locale);
    Ok(Loaded { scripts })
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

    /// The config holds paths; the page holds ids.
    #[test]
    fn enabled_matches_on_the_filename() {
        assert!(is_enabled("adguard-extra", &["userscripts/adguard-extra.meta.json"]));
        assert!(!is_enabled("adguard", &["userscripts/adguard-extra.meta.json"]));
        assert!(!is_enabled("x", &[]));
    }
}
