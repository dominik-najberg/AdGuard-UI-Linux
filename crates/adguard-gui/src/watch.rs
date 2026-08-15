//! Noticing edits to `proxy.yaml` that were not made here.
//!
//! The CLI tells the user to hand-edit this file, and until now the only way
//! that reached the UI was the refresh button. A `gio::FileMonitor` closes
//! that, but it cannot be wired straight to a repaint:
//!
//! **Every `adguard-cli` invocation rewrites `proxy.yaml` and touches its
//! mtime, even `--version`, and even when no byte changes** (contract §5). The
//! Status page polls every 2 s, so the monitor fires against the app's own
//! traffic for the whole life of the session. Debouncing does not help — the
//! churn never stops, so there is no quiet period to debounce to.
//!
//! So an event only means *look again*. [`adguard_core::Watch`] holds the text
//! behind the last reading and answers whether anything actually moved; that
//! answer, not the event, is what drives a repaint. The filtering lives in
//! `adguard-core` because it is the part worth unit-testing, and this module is
//! only the GTK plumbing around it.
//!
//! **A content change is still not news.** The file moving is not evidence that
//! anything the user can see moved, and it is not evidence the change came from
//! anywhere in particular: `Watch::prime` runs once at install and nothing
//! re-primes after the app's own `config set`, so a switch flipped on the
//! Protection page produces a perfectly genuine content change here. Re-priming
//! after each write is not the fix — our write and the re-prime are not atomic,
//! and losing that race either announces a change that was ours or misses one
//! that was not (`architecture.md` §3).
//!
//! What is reportable is narrower and needs no provenance: **a row the user can
//! see moved**. Every `reconcile` below returns how many of its rows differed,
//! and only a non-zero total raises a toast. Our own writes then suppress
//! themselves for free — the page that issued one has already rendered it, and
//! its row is skipped while the write is in flight — and an edit to a key no
//! page displays stays silent, which is right, because nothing on screen
//! changed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{Config, Watch};
use adw::prelude::*;
use gtk::gio;
use gtk4 as gtk;
use libadwaita as adw;

use crate::advanced::AdvancedPage;
use crate::dns::DnsPage;
use crate::extensions::ExtensionsPage;
use crate::filter_settings::FilterSettingsPage;
use crate::protection::ProtectionPage;
use crate::status::StatusPage;
use crate::{toast, worker};

/// A live subscription to `proxy.yaml`.
///
/// Dropping this ends it, so it has to be held for as long as the pages are —
/// see `MainView`. There is nothing to call on it; its existence is the point.
pub struct ConfigWatch {
    _monitor: gio::FileMonitor,
}

struct State {
    /// `None` only while a read is in flight on a worker thread, which owns it
    /// for the duration — the file read and the YAML parse do not belong on the
    /// main loop, and every other read in this app goes the same way.
    watch: RefCell<Option<Watch>>,
    /// A read is outstanding.
    busy: Cell<bool>,
    /// An event arrived while one was outstanding. The file may have moved
    /// again since that read started, so exactly one more look is owed —
    /// however many events arrived, since each look reads the current file.
    dirty: Cell<bool>,
    /// Only its module count is reconciled from here — the rest of that page is
    /// `status` output, which this file says nothing about.
    status: Rc<StatusPage>,
    protection: Rc<ProtectionPage>,
    /// Every table-driven page. Both render from `proxy.yaml`, so both are
    /// reconciled from one reading of it.
    tables: Vec<Rc<AdvancedPage>>,
    /// Its settings half renders from `proxy.yaml` too; its catalogue half
    /// does not, and is left alone by this.
    dns: Rc<DnsPage>,
    /// The Filters page, for the same reason as `dns` and no other: one
    /// `proxy.yaml` switch above a catalogue this file says nothing about.
    /// Without this entry the row would contribute 0 to `moved` and an edit
    /// made in a terminal would never raise the toast — which is exactly what
    /// `handoff.md` §3 item 12 warned would happen if the page were left out.
    filters: Rc<FilterSettingsPage>,
    /// The Extensions page, and it belongs here more literally than any other
    /// entry: for the settings pages `proxy.yaml` holds a value a row renders,
    /// but for this one the file **is** the state. A userscript is enabled
    /// exactly when `userscripts:` names it (contract §15), so
    /// `adguard-cli userscripts disable` typed in a terminal moves a switch
    /// here and nothing else in the application would ever notice.
    extensions: Rc<ExtensionsPage>,
    /// Where the one toast goes. The window's overlay, so it appears over
    /// whichever page is showing — including a page that is not the one whose
    /// row moved, which is the common case: the user is looking at Status while
    /// a terminal edits a Protection key.
    toasts: adw::ToastOverlay,
}

/// Start watching, returning `None` if the file cannot be located or the
/// monitor cannot be established.
///
/// Failure is not fatal and is not reported: the refresh button still works,
/// which is exactly where the app was before this existed.
pub fn install(
    status: &Rc<StatusPage>,
    protection: &Rc<ProtectionPage>,
    tables: &[Rc<AdvancedPage>],
    dns: &Rc<DnsPage>,
    filters: &Rc<FilterSettingsPage>,
    extensions: &Rc<ExtensionsPage>,
    toasts: &adw::ToastOverlay,
) -> Option<ConfigWatch> {
    let mut watch = Watch::on_config()?;
    let file = gio::File::for_path(watch.path());

    // WATCH_MOVES because the file may be replaced rather than rewritten. The
    // default rate limit is left alone: it collapses bursts, and the content
    // check behind it makes the exact event rate uninteresting anyway.
    let monitor = file
        .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        .ok()?;

    // Prime here rather than through `look`, so the state the pages have just
    // rendered from does not come straight back as a change — one redundant
    // repaint, and a startup that looks exactly like an edit in the log. One
    // ~9 KB read on the main thread, once, while the window is being built.
    watch.prime();

    let state = Rc::new(State {
        watch: RefCell::new(Some(watch)),
        busy: Cell::new(false),
        dirty: Cell::new(false),
        status: status.clone(),
        protection: protection.clone(),
        tables: tables.to_vec(),
        dns: dns.clone(),
        filters: filters.clone(),
        extensions: extensions.clone(),
        toasts: toasts.clone(),
    });

    monitor.connect_changed({
        let state = state.clone();
        move |_, _, _, event| {
            // The event kind is not consulted beyond discarding the ones that
            // cannot mean a content change. Everything else is "look again",
            // and the file itself decides.
            if matches!(
                event,
                gio::FileMonitorEvent::AttributeChanged
                    | gio::FileMonitorEvent::PreUnmount
                    | gio::FileMonitorEvent::Unmounted
            ) {
                return;
            }
            look(&state);
        }
    });

    Some(ConfigWatch { _monitor: monitor })
}

/// Read the file off the main thread and repaint if — and only if — it moved.
fn look(state: &Rc<State>) {
    if state.busy.get() {
        state.dirty.set(true);
        return;
    }
    let Some(mut watch) = state.watch.borrow_mut().take() else {
        return;
    };
    state.busy.set(true);

    let state = state.clone();
    worker::run(
        move || {
            let changed = watch.changed();
            (watch, changed)
        },
        move |(watch, changed): (Watch, Option<Config>)| {
            state.watch.replace(Some(watch));
            state.busy.set(false);

            if let Some(config) = changed {
                // Repainted but never counted — its one figure is derived from
                // the six keys Protection owns, so it moves for our own writes
                // too and has no pending flag to tell them apart. See
                // `StatusPage::reconcile`.
                state.status.reconcile(&config);

                let mut moved = state.protection.reconcile(&config);
                for page in &state.tables {
                    moved += page.reconcile(&config);
                }
                moved += state.dns.reconcile(&config);
                moved += state.filters.reconcile(&config);
                moved += state.extensions.reconcile(&config);

                // The only headless evidence that the churn filter works: this
                // line appears for a real edit and not for the app's own
                // traffic. It is also a permanent diagnostic for the next
                // person who wonders whether the monitor is doing anything.
                //
                // It used to say the change came from "outside the app", which
                // it has no way to know — see this module's header. What it can
                // say is what it did: the file moved, and this many rows moved
                // with it. Zero is the interesting reading, not a boring one:
                // it is what the app's own writes and the 2 s status poll both
                // look like.
                eprintln!("adguard-ui: proxy.yaml changed, {moved} displayed row(s) differed");

                // One toast for the whole reading, not one per page. The count
                // is only a gate: a single key can legitimately move rows on
                // two pages — `dns_filtering.enabled` is shown on Protection
                // and read by the DNS page's mode row — so quoting the number
                // back at the user would be arithmetic about widgets, not about
                // settings.
                if moved > 0 {
                    state
                        .toasts
                        .add_toast(toast("Settings reloaded — proxy.yaml changed"));
                }
            }

            // Something arrived mid-read; the file may have moved again since.
            if state.dirty.take() {
                look(&state);
            }
        },
    );
}
