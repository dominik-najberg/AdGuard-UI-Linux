//! The Filters page's settings half: one switch, above the catalogue.
//!
//! `auto_enable_language_filters` is a **writer of the catalogue below it**.
//! It runs in the daemon whether or not this application renders anything, and
//! measurement (`cli-contract.md` §6) says what it does: the automatic add keys
//! on `is_installed`, so a list the user *disabled* survives it while a list the
//! user *removed* comes back switched on. This row is the only brake, and it
//! belongs here rather than on Advanced because a user who finds a filter on
//! that they never switched on is looking at this page.
//!
//! **Why a wrapper exists at all.** The catalogue owns the scrolling page, so
//! settings above it have to arrive as a [`Host`] prelude — the shape
//! [`crate::dns::DnsPage`] already uses. What this module deliberately does not
//! copy from that page is the *contents*: DnsPage hand-builds its prelude and
//! therefore carries its own paint, its own write-then-re-read and its own
//! reconcile. Here the prelude is [`AdvancedPage::host_groups`], so the switch
//! is written, verified and reconciled by the same code as the other forty rows.
//! This file is only the part that cannot be shared — owning a reading of
//! `proxy.yaml` and rebuilding the catalogue when it arrives.

use std::cell::RefCell;
use std::rc::Rc;

use adguard_core::{Cli, Config, FilterSet, FILTER_SETTINGS};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::advanced::AdvancedPage;
use crate::filters::{FiltersPage, Host};
use crate::worker;

pub struct FilterSettingsPage {
    /// The settings half. Table-driven, and the reason this module is short.
    settings: Rc<AdvancedPage>,
    /// The catalogue, which owns the page's actual widget.
    catalogue: RefCell<Option<Rc<FiltersPage>>>,
    /// The reading the prelude paints from. The prelude is called
    /// synchronously during a catalogue rebuild, and `Config::load` does not
    /// belong on the main loop, so the reading has to be here before the
    /// rebuild rather than fetched during it.
    last: RefCell<Option<Config>>,
}

impl FilterSettingsPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let this = Rc::new(Self {
            settings: AdvancedPage::new(cli.clone(), toasts.clone(), &FILTER_SETTINGS),
            catalogue: RefCell::new(None),
            last: RefCell::new(None),
        });

        let prelude = {
            let this = Rc::downgrade(&this);
            Box::new(move || {
                let Some(this) = this.upgrade() else {
                    return Vec::new();
                };
                // No reading yet means the catalogue is painting before the
                // first `Config::load` came back. Returning nothing is right:
                // `refresh_config` rebuilds the catalogue when it lands, and a
                // group painted from a guess would have to be corrected on
                // screen a moment later.
                let Some(config) = this.last.borrow().clone() else {
                    return Vec::new();
                };
                this.settings.host_groups(&config)
            }) as Box<dyn Fn() -> Vec<adw::PreferencesGroup>>
        };

        let catalogue = FiltersPage::hosted(
            cli,
            toasts,
            FilterSet::Http,
            Some(Host {
                prelude,
                // The HTTP catalogue's own user-rules row is correct — the
                // reasons DNS takes its over are all `dns_filtering`'s.
                owns_user_rules: false,
            }),
        );
        *this.catalogue.borrow_mut() = Some(catalogue);

        this.refresh_config();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.catalogue
            .borrow()
            .as_ref()
            .map(|catalogue| catalogue.widget().clone().upcast())
            .unwrap_or_else(|| adw::Bin::new().upcast())
    }

    /// Re-read `proxy.yaml` and the catalogue, and rebuild.
    pub fn reload(self: &Rc<Self>) {
        self.refresh_config();
    }

    /// Where a Status-page link meaning "show me the filters" lands.
    ///
    /// Delegated rather than reimplemented, and it matters more now than it
    /// did: the catalogue deliberately anchors this at its **first list group**
    /// rather than at the top of the page, precisely so a settings prelude
    /// above it does not swallow the link. That comment was written for the DNS
    /// page and this is the second page to rely on it.
    pub fn scroll_to_lists(&self) {
        if let Some(catalogue) = self.catalogue.borrow().as_ref() {
            catalogue.scroll_to_lists();
        }
    }

    /// Repaint from a reading this page did not ask for — the external-edit
    /// entry point, driven by [`crate::watch`].
    ///
    /// Returns how many rows the user could have been looking at moved. The
    /// unbuilt case rebuilds through the **catalogue** rather than through
    /// `AdvancedPage::reconcile`, because that would rebuild into the settings
    /// page's own bin, which is not the widget anyone is looking at.
    pub fn reconcile(self: &Rc<Self>, config: &Config) -> usize {
        *self.last.borrow_mut() = Some(config.clone());
        if self.settings.is_built() {
            self.settings.reconcile(config)
        } else {
            if let Some(catalogue) = self.catalogue.borrow().as_ref() {
                catalogue.reload();
            }
            0
        }
    }

    fn refresh_config(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(
            || Config::load().ok(),
            move |config: Option<Config>| {
                if let Some(config) = config {
                    *this.last.borrow_mut() = Some(config);
                }
                // Rebuilds the catalogue, and with it the prelude, which paints
                // itself from the reading just stored.
                if let Some(catalogue) = this.catalogue.borrow().as_ref() {
                    catalogue.reload();
                }
            },
        );
    }
}
