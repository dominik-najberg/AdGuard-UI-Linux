//! `check-update` against a **throwaway** `$XDG_DATA_HOME`, as a canary for the
//! one thing the unit tests cannot cover: AdGuard rewording its own output.
//!
//! Everything in `cli::tests` runs against captures taken on 9 August 2026. They
//! prove the parser reads what the CLI said *then*, and nothing about what it
//! says now. This suite is the other half — the same argument `license_live.rs`
//! makes for the licence lines, which is that a rewording upstream should show
//! up as a failing test rather than as a blank row in front of a user.
//!
//! ```text
//! cargo test -p adguard-core --test update_sandbox -- --ignored --nocapture
//! ```
//!
//! # This one reaches the network, and no other suite here does
//!
//! `check-update` downloads filter lists — about 1.4 MB in one measured run. It
//! is `#[ignore]`d like every suite that shells out, and it is worth knowing
//! that running it is a real fetch from `filters.adtidy.org` rather than a local
//! command against a scratch directory.
//!
//! **A sandbox rather than the real install**, so a test run never touches the
//! machine's own filter catalogue. That is possible here only because
//! `check-update` needs no licence — measured, contract §14, and unlike
//! `status`, `license`, `filters list` and every `filters` write subcommand.
//! That property is itself asserted below, because the control in the UI is
//! built on it.

use std::path::PathBuf;

use adguard_core::{Cli, UpdatePart, UpdateReport, Verdict};

/// A scratch `$XDG_DATA_HOME` with nothing in it — the CLI creates its own data
/// directory on first use, which is the run that prints the extra line the
/// parser has to skip.
struct Sandbox {
    root: PathBuf,
    cli: Cli,
}

impl Sandbox {
    /// `None` when `adguard-cli` is absent, so the suite skips rather than fails
    /// on a machine without AdGuard.
    fn new(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };

        let root = std::env::temp_dir()
            .join(format!("adguard-ui-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create sandbox");

        Some(Self {
            cli: cli.with_xdg_data_home(&root),
            root,
        })
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The six components, their spellings, and the fact that a first run parses.
///
/// The order is AdGuard's and is asserted as a whole: a component appearing,
/// disappearing or being renamed all land here, and any of the three is
/// something a person should look at before the About page starts describing it
/// to users.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn a_first_run_reports_all_six_components() {
    let Some(sandbox) = Sandbox::new("first-run") else {
        return;
    };

    // The first invocation in a virgin directory, which is the one that prints
    // `Created data directory …` ahead of the pairs. If that line were being
    // read as a verdict, every component below would be shifted onto the wrong
    // header and this assertion would be the one that caught it.
    let report: UpdateReport = sandbox
        .cli
        .check_update()
        .expect("check-update should run, and should run unlicensed");

    let parts: Vec<_> = report.components.iter().map(|c| c.part.clone()).collect();
    assert_eq!(
        parts,
        vec![
            UpdatePart::Filters,
            UpdatePart::DnsFilters,
            UpdatePart::Userscripts,
            UpdatePart::SafeBrowsing,
            UpdatePart::CrLite,
            UpdatePart::App,
        ],
        "the component list or its spellings moved upstream; \
         `UpdatePart::from_header` and contract §14 both need re-measuring"
    );

    for component in &report.components {
        eprintln!("{}: {}", component.part.title(), component.said);
        assert!(
            !component.said.is_empty(),
            "{} was announced and never answered",
            component.part.title()
        );
    }
}

/// Every verdict the five content components give must be one this build
/// recognises.
///
/// **A failure is not a test failure.** Five of fourteen measured runs failed a
/// component and the next run of that component succeeded every time, so
/// `Failed` is an expected outcome here and asserting its absence would make
/// this suite flaky for a reason that has nothing to do with the code.
/// `Unrecognised` is the interesting one: it is what a reworded verdict looks
/// like.
///
/// **The application line is exempt**, and deliberately. What it says when an
/// update exists has never been observed, so an unrecognised verdict there is
/// the very case the UI is built to pass through verbatim — a new AdGuard
/// release should not turn this suite red.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn every_content_verdict_is_one_we_recognise() {
    let Some(sandbox) = Sandbox::new("verdicts") else {
        return;
    };

    let report = sandbox.cli.check_update().expect("check-update should run");

    for component in report.components.iter().filter(|c| c.part != UpdatePart::App) {
        assert_ne!(
            component.verdict,
            Verdict::Unrecognised,
            "{} answered {:?}, which no rule in `Verdict::classify` matches",
            component.part.title(),
            component.said
        );
    }

    // Not an assertion about this run's outcome — only that the accessor agrees
    // with the verdicts it is derived from, whichever way they fell.
    let failed: Vec<_> = report.failures().map(|c| c.part.title().to_owned()).collect();
    eprintln!("failed this run: {failed:?}");
    assert_eq!(
        failed.len(),
        report.components.iter().filter(|c| c.verdict == Verdict::Failed).count()
    );
}

/// The app line, and the notice built on it.
///
/// On every measured run it has said `Up to date`, so `app_notice` is `None` and
/// the About page shows nothing. This asserts the pair stays consistent rather
/// than asserting which way it fell — and prints the sentence, because the day
/// it is not `Up to date` is the day this project finally sees the wording it
/// has never been able to measure.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn the_app_notice_agrees_with_the_app_verdict() {
    let Some(sandbox) = Sandbox::new("app") else {
        return;
    };

    let report = sandbox.cli.check_update().expect("check-update should run");
    let app = report
        .part(&UpdatePart::App)
        .expect("the app component should be reported");

    eprintln!("app: {:?} — {}", app.verdict, app.said);
    match app.verdict {
        // The only outcome ever observed, and the only one that says nothing.
        Verdict::UpToDate => assert_eq!(report.app_notice(), None),
        // A failed *check* is not a release. It belongs with the failures, and
        // showing it as a notice would recommend a command on the strength of a
        // check that did not finish.
        Verdict::Failed => {
            assert_eq!(report.app_notice(), None);
            assert!(report.failures().any(|c| c.part == UpdatePart::App));
        }
        _ => assert_eq!(
            report.app_notice(),
            Some(app.said.as_str()),
            "an app verdict that is not `Up to date` must reach the user verbatim"
        ),
    }
}
