//! The export/import wrappers against the real CLI, in a sandbox.
//!
//! `#[ignore]`d: these shell out, and `export-settings` writes ~15 MB. They
//! need a sandbox data directory seeded from a real install — copy `proxy.yaml`
//! and the `.db` files in, never point `$ADGUARD_SANDBOX` at the real one:
//!
//! ```console
//! $ ADGUARD_SANDBOX=/tmp/sb cargo test -p adguard-core --test export_sandbox -- --ignored
//! ```
//!
//! The import leg is deliberately **not** exercised against a seeded sandbox
//! here. `import-settings` rewrites `proxy.yaml` wholesale, and a test that
//! does that has to own a directory it created itself — which is what the
//! round-trip test below does, into a second scratch directory.

use std::path::PathBuf;

use adguard_core::zip::{classify, entries, Bundle};
use adguard_core::Cli;

fn sandbox() -> Option<PathBuf> {
    std::env::var_os("ADGUARD_SANDBOX")
        .map(PathBuf::from)
        .filter(|p| p.join("adguard-cli/proxy.yaml").exists())
}

/// The path comes back from the **confirmation line**, and it has to, because
/// `-o` writes *into* an existing directory and *as* any other path
/// (contract §13). Both shapes are driven here, since a wrapper that only ever
/// saw one of them would look correct.
#[test]
#[ignore = "shells out and writes ~15 MB; see the module docs"]
fn an_export_reports_where_it_actually_went() {
    let Some(sb) = sandbox() else {
        eprintln!("ADGUARD_SANDBOX unset or not seeded — asserting nothing");
        return;
    };
    let cli = Cli::discover().expect("adguard-cli").with_xdg_data_home(&sb);

    // Shape one: an existing directory. The CLI picks the filename.
    let dir = sb.join("out-dir");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let into_dir = cli.export_settings(&dir).expect("export into a directory");
    assert!(into_dir.starts_with(&dir), "{into_dir:?} is not inside {dir:?}");
    assert!(into_dir.exists(), "the reported path does not exist");
    assert_eq!(
        classify(&entries(&into_dir).expect("a real export parses")),
        Bundle::Settings
    );

    // Shape two: a path that does not exist. It becomes the archive itself,
    // at exactly that name — note the deliberate absence of a `.zip` suffix,
    // which is the half a caller would get wrong.
    let as_file = sb.join("named-export");
    let _ = std::fs::remove_file(&as_file);
    let reported = cli.export_logs(&as_file).expect("export as a file");
    assert_eq!(reported, as_file, "the CLI did not use the name it was given");
    assert_eq!(
        classify(&entries(&reported).expect("a real export parses")),
        Bundle::Logs,
        "a logs bundle must never read as settings"
    );
}

/// **Two exports into one directory inside the same second collide**, and the
/// second fails at **exit 0** with `Failed to export logs to zip: <path>` —
/// the generated name is `adguard-cli_<date>_<time>.zip` at one-second
/// resolution and the CLI will not overwrite. Measured 2 August 2026.
///
/// This is the regression test for a real bug in the wrapper: that failure
/// line contains `zip: ` and the same path as the success line, so the first
/// version of `exported` — which split on `zip: ` — returned the archive it
/// had just failed to write. Matching the **success** prefix is the fix, and
/// this asserts the failure is reported as one.
#[test]
#[ignore = "shells out; see the module docs"]
fn a_second_export_in_the_same_second_is_reported_as_a_failure() {
    let Some(sb) = sandbox() else {
        eprintln!("ADGUARD_SANDBOX unset or not seeded — asserting nothing");
        return;
    };
    let cli = Cli::discover().expect("adguard-cli").with_xdg_data_home(&sb);
    let dir = sb.join("collide-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let first = cli.export_logs(&dir).expect("the first export succeeds");
    assert!(first.exists());
    // Back to back, deliberately with no delay.
    match cli.export_logs(&dir) {
        Err(_) => {}
        Ok(path) => panic!("the colliding export was reported as success at {path:?}"),
    }
    let written = std::fs::read_dir(&dir).expect("readable").count();
    assert_eq!(written, 1, "a second archive appeared where the CLI wrote none");
}

/// The round trip, into a directory this test creates and owns.
///
/// Proves the two halves compose: what `export_settings` writes,
/// `import_settings` accepts. It does not assert what survives — contract §13
/// already measures that the DNS catalogue and `dns_user.txt` do not.
#[test]
#[ignore = "shells out, writes ~15 MB, and rewrites a proxy.yaml"]
fn a_settings_export_imports_back() {
    let Some(sb) = sandbox() else {
        eprintln!("ADGUARD_SANDBOX unset or not seeded — asserting nothing");
        return;
    };
    let cli = Cli::discover().expect("adguard-cli").with_xdg_data_home(&sb);
    let dir = sb.join("roundtrip");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let zip = cli.export_settings(&dir).expect("export");

    // A directory of its own, created here, so nothing seeded is overwritten.
    let target = sb.join("imported");
    std::fs::create_dir_all(&target).expect("scratch dir");
    let into = Cli::discover().expect("adguard-cli").with_xdg_data_home(&target);
    into.import_settings(&zip).expect("import what we just exported");
    assert!(
        target.join("adguard-cli/proxy.yaml").exists(),
        "the import did not produce a proxy.yaml"
    );
}
