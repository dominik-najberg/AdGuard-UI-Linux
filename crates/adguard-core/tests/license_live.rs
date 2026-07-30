//! `adguard-cli license` against the real binary on this machine.
//!
//! The unit tests in `cli.rs` parse a recorded three-line reading, which proves
//! the parser but not that it matches what AdGuard prints today. This one asks
//! the binary — the only way to catch a rewording before a user does, since the
//! licensed shape cannot be captured in a sandbox: a sandbox is unlicensed by
//! construction and `license` is refused there (contract §5).
//!
//! **Read-only, and safe to run by default.** `license` mutates nothing; the
//! only side effect is the config rewrite *every* invocation performs, which
//! the app already does on every start. The suite skips when AdGuard CLI is not
//! installed, and skips again when the install is not licensed — an unlicensed
//! machine is a legitimate state, not a failing test.
//!
//! # Nothing here may print the key
//!
//! This is the one test file holding a real licence key at runtime. Every
//! assertion message is written to carry [`License::masked_key`] or nothing at
//! all, because a failure message is exactly the kind of place a secret leaks
//! from — into a terminal, a CI log, or a bug report.
//!
//! [`License::masked_key`]: adguard_core::License::masked_key

use std::sync::OnceLock;

use adguard_core::{Cli, Error, License};

/// The reading, taken **once** for the whole suite.
///
/// Not once per test, which is what a plain helper would do: libtest runs these
/// three tests on three threads, and three `adguard-cli` invocations arriving
/// together in one data directory are the race contract §3 measures — against a
/// directory that has never been used, one of them exits 1 with `Filter manager
/// initialization failed`. A suite that manufactures the very failure it is
/// written to notice would fail on a fresh machine and pass on this one, which
/// is the definition of a flake.
static LICENCE: OnceLock<Option<License>> = OnceLock::new();

/// `None` when there is no CLI, when this install is not licensed, or when the
/// CLI refused for a reason of its own.
fn licence() -> Option<&'static License> {
    LICENCE
        .get_or_init(|| {
            let cli = match Cli::discover() {
                Ok(cli) => cli,
                Err(err) => {
                    eprintln!("skipping: {err}");
                    return None;
                }
            };

            match cli.license() {
                Ok(licence) => Some(licence),
                Err(Error::Unlicensed { .. }) => {
                    eprintln!("skipping: this install is not licensed");
                    None
                }
                // The CLI's own refusal — an uninitialised data directory says
                // `Filter manager initialization failed` here. A legitimate
                // state of the machine, not a change in the output shape, which
                // is what this suite is for.
                Err(err @ Error::Refused { .. }) => {
                    eprintln!("skipping: {err}");
                    None
                }
                // Anything else is the failure this suite exists to notice: an
                // output shape we could not parse, or a command that did not
                // come back.
                Err(err) => panic!("`adguard-cli license` failed: {err}"),
            }
        })
        .as_ref()
}

/// The reading parses, and it says something.
///
/// A status is the field the parser is defined on, so reaching here at all
/// means the three-line shape survived. The rest is what the Status page shows.
#[test]
fn the_licence_reading_still_parses() {
    let Some(licence) = licence() else { return };

    eprintln!("license -> status {}, key {}", licence.status, licence.masked_key());

    assert!(!licence.status.is_empty(), "a parsed reading has a status");
    if licence.is_active() {
        assert!(
            !licence.owner.is_empty(),
            "an active licence should name its owner"
        );
        assert!(
            !licence.key.is_empty(),
            "an active licence should carry a key"
        );
    }
}

/// The masking rule, applied to a real key rather than a made-up one.
#[test]
fn the_real_key_is_masked_to_its_last_four_characters() {
    let Some(licence) = licence() else { return };
    if licence.key.is_empty() {
        return;
    }

    let masked = licence.masked_key();
    let visible = 4.min(licence.key.chars().count());

    assert_eq!(
        masked.chars().filter(|c| *c == '•').count(),
        licence.key.chars().count() - visible,
        "wrong amount of the key was hidden",
    );
    assert!(
        !masked.contains(&licence.key),
        "the whole key survived masking",
    );
    // The one thing a mask is for: enough to recognise, not enough to use.
    assert!(
        licence.key.ends_with(masked.trim_start_matches('•')),
        "the visible tail is not the end of the key",
    );
}

/// The accident this guards is one line of debugging left in a page.
#[test]
fn a_debug_print_of_the_real_licence_leaks_nothing() {
    let Some(licence) = licence() else { return };

    let printed = format!("{licence:?}");
    if !licence.key.is_empty() {
        assert!(
            !printed.contains(&licence.key),
            "the licence key reached a Debug print",
        );
    }
    if !licence.owner.is_empty() {
        assert!(
            !printed.contains(&licence.owner),
            "the licence owner reached a Debug print",
        );
    }
}
