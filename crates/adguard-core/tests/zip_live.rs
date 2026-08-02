//! The zip reader against archives **AdGuard actually wrote**.
//!
//! `src/zip.rs`'s own tests use fixtures written by python's `zipfile`, which
//! proves the parser against an independent implementation of the format but
//! not against this particular producer. These close that gap, and they are
//! `#[ignore]`d because they need a real export to exist first:
//!
//! ```console
//! $ XDG_DATA_HOME=/tmp/sandbox adguard-cli export-settings -o /tmp/exp/
//! $ XDG_DATA_HOME=/tmp/sandbox adguard-cli export-logs -o /tmp/exp/logs.zip
//! $ ADGUARD_SETTINGS_ZIP=/tmp/exp/adguard-cli_*.zip \
//!   ADGUARD_LOGS_ZIP=/tmp/exp/logs.zip \
//!   cargo test -p adguard-core --test zip_live -- --ignored
//! ```
//!
//! Sandbox it. `export-settings` only reads, but it reads the whole data
//! directory, and pointing it at the real one writes a 14.9 MB zip of the
//! user's configuration somewhere.

use std::path::PathBuf;

use adguard_core::zip::{classify, entries, Bundle};

fn from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|p| p.exists())
}

/// A real `export-settings` archive classifies as settings, and carries the
/// entries contract §13 measured.
#[test]
#[ignore = "needs a real export; see the module docs"]
fn a_real_settings_export_is_read_and_classified() {
    let Some(path) = from_env("ADGUARD_SETTINGS_ZIP") else {
        eprintln!("ADGUARD_SETTINGS_ZIP unset or missing — asserting nothing");
        return;
    };
    let names = entries(&path).expect("a real export must parse");
    eprintln!("settings zip: {} entries: {names:?}", names.len());
    assert_eq!(classify(&names), Bundle::Settings);
    for expected in ["proxy.yaml", "filters.yaml", "config.txt"] {
        assert!(names.iter().any(|n| n == expected), "{expected} missing");
    }
    // The measured shape. Not asserted as an exact count: it is a fact about
    // one CLI version, and a future one adding a file should not fail here.
    assert!(names.len() >= 8, "fewer entries than §13 measured: {names:?}");
}

/// The one that matters. A logs archive must **not** read as settings, because
/// `import-settings` accepts it at exit 0 and leaves a partial install.
#[test]
#[ignore = "needs a real export; see the module docs"]
fn a_real_logs_export_is_never_mistaken_for_settings() {
    let Some(path) = from_env("ADGUARD_LOGS_ZIP") else {
        eprintln!("ADGUARD_LOGS_ZIP unset or missing — asserting nothing");
        return;
    };
    let names = entries(&path).expect("a real export must parse");
    eprintln!("logs zip: {} entries: {names:?}", names.len());
    assert_eq!(classify(&names), Bundle::Logs);
    // Contract §13's two surprises, re-checked against the live artifact: the
    // logs bundle *does* carry `proxy.yaml`, and does *not* carry `access.log`.
    assert!(names.iter().any(|n| n == "proxy.yaml"), "§13 said logs carry proxy.yaml");
    assert!(
        !names.iter().any(|n| n == "access.log"),
        "§13 measured access.log absent from the logs bundle, twice"
    );
}
