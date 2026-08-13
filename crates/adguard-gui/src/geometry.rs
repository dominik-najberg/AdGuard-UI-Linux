//! The window half of remembering a window: what to ask GTK, and when to write
//! the answer down.
//!
//! [`adguard_core::window_state`] owns the file and every judgement about what
//! is in it. This is the part that cannot be tested without a display: which
//! property carries the size, which signals say it moved, and how not to write
//! a file forty times while a window is being dragged by its corner.
//!
//! **The size comes from `default-width`/`default-height`, never from
//! `width()`/`height()`.** The two are different numbers and only one of them
//! is the one to save. The allocation is what the window *is* right now,
//! including a maximized or tiled size the user never chose and, under some
//! compositors, the client-side shadow around it — save that and a window
//! maximized once comes back maximized-sized but not maximized, or creeps by
//! the width of its own shadow every launch. GTK keeps the default size as the
//! size the window would return to, updates it as the user resizes, and hands
//! it back after the window has been hidden or even destroyed. It is the number
//! GNOME's own applications persist, and it is the one that survives the tray.
//!
//! **Saving is on change, not on the way out.** Measured 14 August 2026 with a
//! window on screen and all three handlers connected: a `SIGTERM` — a logout, a
//! session ending, an `OOM` kill — emits **no `shutdown`, no `close-request`
//! and no `unmap`**. The process is simply gone. An application that wrote only
//! on the way out would therefore discard a whole session's resizing at exactly
//! the moment the user is least able to say what happened to it. Writing as it
//! changes bounds that loss to the settle interval below, and the exit hooks
//! stay for the resize-then-immediately-close case the settle would otherwise
//! still be holding.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use adguard_core::{Geometry, WindowState};
use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

/// How long a resize has to stop for before it is worth a file.
///
/// A window dragged by its corner notifies on `default-width` about once a
/// frame, and each save is a directory check, a write and a rename. This is
/// long enough that a drag costs one of them rather than a hundred, and short
/// enough to be over before anyone reaches the close button — and the two
/// flushes cover the case where it is not.
const SETTLE: Duration = Duration::from_millis(750);

/// The sizes of the monitors this session can see, for
/// [`adguard_core::Geometry::fitted`].
///
/// Plain `(width, height)` pairs, in the logical pixels GTK sizes windows in,
/// because that is the whole of what the clamp needs and `adguard-core` does
/// not link GTK.
///
/// **Asked before the window exists, deliberately.** The obvious call is
/// `Display::monitor_at_surface`, which would name the display the window is
/// actually on — but a window that has not been presented yet has no surface,
/// and the whole point is to size it before it is on screen. The monitor list
/// is populated from the moment the display is opened, so this can be asked at
/// the top of `start` and the clamp is against every display rather than the
/// right one, which is the trade [`adguard_core::Geometry::fitted`] documents.
///
/// An empty list — no display, or one that lists nothing — means no clamp
/// rather than a default size.
pub fn monitors() -> Vec<(i32, i32)> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };

    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|object| object.downcast::<gdk::Monitor>().ok())
        .map(|monitor| {
            let geometry = monitor.geometry();
            (geometry.width(), geometry.height())
        })
        .collect()
}

/// Writes the window's size out, once it has stopped changing.
///
/// Held by the [`crate::Instance`] so it lives as long as the window does, and
/// so the two places that have to write *now* rather than in three quarters of
/// a second can reach it.
pub struct Saver {
    window: adw::ApplicationWindow,
    /// `None` in a session with neither `$XDG_STATE_HOME` nor `$HOME`. The
    /// window then works exactly as it did before this file existed: it opens
    /// at the default size and forgets whatever it is moved to.
    state: Option<WindowState>,
    /// The last geometry written, so a save that would not change the file is
    /// not made. It starts as what was *restored* — which is what makes the
    /// `--background` case free rather than dangerous: a window that is built
    /// and never presented reports back exactly the size it was built with, so
    /// there is nothing to write and no way for it to overwrite a real one.
    written: Cell<Geometry>,
    /// Bumped by every change, so a settled write can tell whether it is still
    /// the latest one. The same shape as the Advanced page's spin rows, and for
    /// the same reason: one counter is less to get wrong than a source id that
    /// has to be cancelled from three places.
    generation: Cell<u64>,
}

/// Start saving this window's size, and hand back the handle that can force it.
///
/// `restored` is what the window was just built with — see [`Saver::written`]
/// for why that, and not a fresh reading, is the right starting point.
pub fn connect_saving(window: &adw::ApplicationWindow, restored: Geometry) -> Rc<Saver> {
    let saver = Rc::new(Saver {
        window: window.clone(),
        state: WindowState::locate(),
        written: Cell::new(restored),
        generation: Cell::new(0),
    });

    // Weak, and it has to be: the saver holds the window, so a strong capture
    // in a handler the window owns would close the loop and neither would ever
    // be freed. It is also what makes the failure paths in `start` — the ones
    // that destroy the window and return before anything holds an `Instance` —
    // cost nothing: the saver is dropped, and these handlers stop finding one.
    let on_change = {
        let saver = Rc::downgrade(&saver);
        move |_: &adw::ApplicationWindow| {
            if let Some(saver) = saver.upgrade() {
                saver.schedule();
            }
        }
    };

    window.connect_default_width_notify(on_change.clone());
    window.connect_default_height_notify(on_change.clone());
    // Maximizing changes no default size at all — it is a separate property and
    // a separate notification, and without this one a window maximized and then
    // quit would come back merely large.
    window.connect_maximized_notify(on_change);

    // The window is going away — to the tray, or for good — so a settle still
    // in flight would never land. Covers both, and it is the only hook that
    // covers the second: with no tray there is no close handler at all and the
    // window is destroyed where it stands.
    //
    // **It must be connected here, before the tray's.** `close-request` stops
    // at the first handler that returns `Stop`, and the tray's does exactly
    // that (see [`crate::connect_tray`]) — so a flush wired up after it would
    // be dead code on every machine that has a tray, which is most of them.
    // `Proceed` because this handler decides nothing; whether closing hides or
    // quits is the tray's question, and it is still the one being asked.
    window.connect_close_request({
        let saver = Rc::downgrade(&saver);
        move |_| {
            if let Some(saver) = saver.upgrade() {
                saver.flush();
            }
            glib::Propagation::Proceed
        }
    });

    saver
}

impl Saver {
    /// Note that something moved, and write it once it stops.
    fn schedule(self: &Rc<Self>) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);

        let saver = Rc::downgrade(self);
        glib::timeout_add_local_once(SETTLE, move || {
            let Some(saver) = saver.upgrade() else { return };
            // Superseded by a later change, or by a flush that already wrote.
            if saver.generation.get() != generation {
                return;
            }
            saver.write();
        });
    }

    /// Write now, if there is anything to write.
    ///
    /// For the two moments a settle cannot survive: the window being hidden to
    /// the tray, and the application being told to quit. Bumps the generation
    /// on the way through, so a settle still in flight finds itself superseded
    /// rather than writing the same bytes a moment later.
    pub fn flush(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.write();
    }

    /// The window's current size, or nothing if it is not worth recording.
    fn write(&self) {
        let Some(state) = &self.state else { return };

        // A window that was never put on screen has nothing to say about how
        // big the user wants it. This is the `--background` gate: under that
        // flag the window is built and deliberately never presented, and a
        // realized window is one that has been.
        if !self.window.is_realized() {
            return;
        }

        let (width, height) = self.window.default_size();
        // GTK reads a zero as "ask the widgets", so it is not a size and would
        // not survive a reload anyway. Seeing one here would mean the window
        // never had a size to begin with.
        if width <= 0 || height <= 0 {
            return;
        }

        let geometry = Geometry {
            width,
            height,
            maximized: self.window.is_maximized(),
        };
        if geometry == self.written.get() {
            return;
        }

        // Recorded as written either way. A disk that refused this write will
        // refuse the next one for the same reason, and retrying at every frame
        // of the next drag would turn a failure into a stall.
        self.written.set(geometry);
        if let Err(err) = state.save(&geometry) {
            // Not a toast. The user did not ask for this and cannot act on it,
            // and a window that opens at 880×720 next time is a smaller
            // surprise than a message about a file they have never heard of.
            eprintln!(
                "adguard-ui: could not save the window size to {}: {err}",
                state.path().display()
            );
        }
    }
}
