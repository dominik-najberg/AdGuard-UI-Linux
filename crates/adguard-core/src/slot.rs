//! One `adguard-cli` at a time per data directory, from this process.
//!
//! # Why
//!
//! Every invocation truncates `adguard.conf`, where the licence and the CA's
//! private key live, and writes it back, even when no byte changes (contract
//! §5). Another invocation that reads the file inside that window sees an
//! empty config, logs `Upgrading config from revision 0 to 1`, and seeds a
//! new one. That loses the licence, the device identity and the CA.
//!
//! The window is microseconds on a healthy machine. On 18 October 2026 it was
//! not: swap was full and the disk briefly ran out of space, the desktop froze
//! for seconds at a time, and two `adguard-cli` processes hung for more than
//! fifteen seconds each. A config reset followed each episode. Two of this
//! application's own invocations overlapping is the one share of that this
//! application controls, so it no longer allows it.
//!
//! # A command that times out is not killed
//!
//! It used to be, with `SIGKILL`, after fifteen seconds for a local command.
//! A process killed between its truncate and its write leaves the file empty,
//! and the next reader seeds a new one. So a command that outlives its
//! deadline is now reported as [`crate::Error::TimedOut`] and **left to
//! finish**. Its slot stays taken until it exits, so nothing of ours runs
//! beside it while it does.
//!
//! # Bounded waits
//!
//! A caller waits for the slot no longer than its own deadline, and then gets
//! [`crate::Error::Busy`] without anything having run. A CLI that hangs for
//! good would otherwise hang every page with it.
//!
//! Keyed by `$XDG_DATA_HOME` (`None` for the inherited one), because two data
//! directories do not share a config file and must not wait on each other.
//! The test suites rely on that.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

type Key = Option<PathBuf>;

fn table() -> &'static (Mutex<HashSet<Key>>, Condvar) {
    static TABLE: OnceLock<(Mutex<HashSet<Key>>, Condvar)> = OnceLock::new();
    TABLE.get_or_init(|| (Mutex::new(HashSet::new()), Condvar::new()))
}

thread_local! {
    /// Data directories this thread holds through [`exclusively`], inside
    /// which an invocation runs without taking the slot again.
    static HELD: RefCell<HashSet<Key>> = RefCell::new(HashSet::new());
}

/// The right to run `adguard-cli` against one data directory. Released when
/// dropped, on whichever thread drops it.
#[must_use]
pub(crate) struct Slot {
    key: Option<Key>,
}

impl Slot {
    /// Take the slot for `key`, waiting until `deadline` at most.
    ///
    /// A thread inside [`exclusively`] for the same key already holds it and
    /// gets a token that releases nothing.
    pub(crate) fn acquire(key: &Key, deadline: Instant) -> Option<Self> {
        if HELD.with(|held| held.borrow().contains(key)) {
            return Some(Self { key: None });
        }
        let (lock, freed) = table();
        let mut busy = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while busy.contains(key) {
            let left = deadline.checked_duration_since(Instant::now())?;
            busy = freed
                .wait_timeout(busy, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        busy.insert(key.clone());
        Some(Self { key: Some(key.clone()) })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        let (lock, freed) = table();
        lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&key);
        freed.notify_all();
    }
}

/// Run `body` holding the slot for `key`, so that its own invocations and any
/// file work between them cannot interleave with another of ours.
///
/// `None` when the slot could not be had by `deadline`; `body` did not run.
pub(crate) fn exclusively<T>(key: &Key, deadline: Instant, body: impl FnOnce() -> T) -> Option<T> {
    let slot = Slot::acquire(key, deadline)?;
    let nested = slot.key.is_none();
    if !nested {
        HELD.with(|held| held.borrow_mut().insert(key.clone()));
    }
    // Released even if `body` panics, so a bug cannot wedge every page.
    struct Unmark<'a>(&'a Key, bool);
    impl Drop for Unmark<'_> {
        fn drop(&mut self) {
            if !self.1 {
                HELD.with(|held| held.borrow_mut().remove(self.0));
            }
        }
    }
    let _unmark = Unmark(key, nested);
    let value = body();
    drop(slot);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn key(name: &str) -> Key {
        Some(PathBuf::from(format!("/slot-test/{name}")))
    }

    #[test]
    fn a_second_taker_waits_and_then_gives_up() {
        let k = key("wait");
        let first = Slot::acquire(&k, Instant::now() + Duration::from_secs(1)).expect("free");
        let started = Instant::now();
        let second = Slot::acquire(&k, Instant::now() + Duration::from_millis(150));
        assert!(second.is_none(), "the slot was handed out twice");
        assert!(started.elapsed() >= Duration::from_millis(140));
        drop(first);
        assert!(Slot::acquire(&k, Instant::now() + Duration::from_millis(50)).is_some());
    }

    #[test]
    fn a_release_on_another_thread_wakes_the_waiter() {
        let k = key("handover");
        let first = Slot::acquire(&k, Instant::now() + Duration::from_secs(1)).expect("free");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(first);
        });
        let second = Slot::acquire(&k, Instant::now() + Duration::from_secs(5));
        assert!(second.is_some(), "the waiter was not woken by the release");
        releaser.join().unwrap();
    }

    #[test]
    fn directories_do_not_wait_on_each_other() {
        let _a = Slot::acquire(&key("a"), Instant::now() + Duration::from_secs(1)).expect("free");
        let b = Slot::acquire(&key("b"), Instant::now() + Duration::from_millis(10));
        assert!(b.is_some(), "a different data directory was made to wait");
    }

    #[test]
    fn inside_exclusively_a_nested_taker_does_not_deadlock() {
        let k = key("nested");
        let ran = exclusively(&k, Instant::now() + Duration::from_secs(1), || {
            Slot::acquire(&k, Instant::now() + Duration::from_millis(10)).is_some()
        });
        assert_eq!(ran, Some(true));
        // And the slot is free again afterwards.
        assert!(Slot::acquire(&k, Instant::now() + Duration::from_millis(10)).is_some());
    }
}
