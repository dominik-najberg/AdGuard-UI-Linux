//! Off-main-thread work.
//!
//! Every request the UI makes of AdGuard — CLI invocations and SQLite reads —
//! goes through here. Even the fast commands cost 10–30 ms, which is a visible
//! stutter at 60 Hz, and the network ones (`filters update`) can hang for
//! seconds (`docs/architecture.md` §4).

use gtk::glib;
use gtk4 as gtk;

/// Run `job` on a worker thread and hand its result to `finish` back on the
/// main thread, where touching widgets is legal.
///
/// One thread per call is deliberate: these jobs are short and infrequent
/// (a 2 s poll, a click), so a pool would be machinery without a payoff.
pub fn run<T, Job, Finish>(job: Job, finish: Finish)
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
    Finish: FnOnce(T) + 'static,
{
    let (tx, rx) = async_channel::bounded(1);

    std::thread::spawn(move || {
        // A send failure means the receiver is gone — the window closed while
        // the job ran. Nothing to report to.
        let _ = tx.send_blocking(job());
    });

    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            finish(result);
        }
    });
}
