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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{Config, Watch};
use gtk::gio;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::advanced::AdvancedPage;
use crate::dns::DnsPage;
use crate::protection::ProtectionPage;
use crate::status::StatusPage;
use crate::worker;

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
                // The only headless evidence that the churn filter works: this
                // line appears for a real edit and not for the app's own
                // traffic. It is also a permanent diagnostic for the next
                // person who wonders whether the monitor is doing anything.
                eprintln!("adguard-ui: proxy.yaml changed outside the app, reconciling");
                state.status.reconcile(&config);
                state.protection.reconcile(&config);
                for page in &state.tables {
                    page.reconcile(&config);
                }
                state.dns.reconcile(&config);
            }

            // Something arrived mid-read; the file may have moved again since.
            if state.dirty.take() {
                look(&state);
            }
        },
    );
}
