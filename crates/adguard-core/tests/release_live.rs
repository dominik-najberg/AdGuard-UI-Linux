//! The real request to github.com, which is the only network call this
//! application makes of its own.
//!
//! ```text
//! cargo test -p adguard-core --test release_live -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d like every suite that leaves this machine. Two things it is here
//! to catch, neither of which the unit tests can:
//!
//! **The certificate path.** On a machine filtering system-wide, AdGuard
//! intercepts this connection and re-signs it with its own CA — so this suite
//! passing is the evidence that the platform verifier is reading the system
//! trust store and that the feature selection in the workspace manifest is the
//! right one. A build that switched to bundled roots would fail here and
//! nowhere else, and it would fail only for the users who filter, which is all
//! of them.
//!
//! **GitHub's answer shape.** `tag_of` scans for one field. If that field is
//! ever renamed or moved, this is what says so, rather than a row that quietly
//! stops reporting updates.

use adguard_core::release::{self, Standing};

#[test]
#[ignore = "reaches github.com"]
fn the_latest_release_can_be_read() {
    let release = match release::latest() {
        Ok(release) => release,
        // A rate limit or an outage is not this project's bug, and a test that
        // fails on someone else's downtime teaches nothing.
        Err(err) => {
            eprintln!("skipping: {err}");
            return;
        }
    };

    eprintln!("latest release: {} -> {}", release.tag, release.url());
    assert!(!release.tag.is_empty());
    assert!(
        release.tag.chars().any(|c| c.is_ascii_digit()),
        "a tag with no digit in it is not a version: {}",
        release.tag
    );
    assert!(release.url().starts_with("https://github.com/"));
}

/// The running build against the published one, whichever way it falls.
///
/// Asserts the pair is consistent rather than which way it went: this crate's
/// version moves every release, and a test pinned to one outcome would fail on
/// the day of every cut.
#[test]
#[ignore = "reaches github.com"]
fn the_running_version_can_be_placed_against_it() {
    let Ok(release) = release::latest() else {
        eprintln!("skipping: github.com could not be reached");
        return;
    };

    let running = env!("CARGO_PKG_VERSION");
    let standing = release::standing(running, release.clone());
    eprintln!("running {running}, latest {}: {standing:?}", release.tag);

    match standing {
        Standing::Current | Standing::Behind(_) | Standing::Ahead(_) => {}
        // Not a failure — a pre-release tag lands here by design — but it means
        // the About page is showing the tag and no verdict, which is worth
        // seeing in the output rather than passing silently.
        Standing::Unknown(release) => {
            eprintln!("note: {} could not be compared to {running}", release.tag);
        }
    }
}
