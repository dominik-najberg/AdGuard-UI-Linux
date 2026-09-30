//! The diagnostics report, read from this machine's real install.
//!
//! The unit tests in `diagnostics.rs` build reports from hand-made inputs, which
//! proves the rendering but not that the gathering reads what it should. This
//! one runs [`Inputs::collect`] for real — the same three commands, `/proc` walk
//! and file reads the page performs.
//!
//! **Read-only, and safe to run by default.** `version`, `status` and `license`
//! mutate nothing beyond the config rewrite every invocation performs. Skips
//! when AdGuard CLI is not installed.
//!
//! # The report must not carry the licence
//!
//! The report is written for a public issue tracker, and the page's copy button
//! is one click from pasting it into one. So the test reads the licence itself
//! and asserts neither the key nor the owner appears anywhere in the text. No
//! assertion message quotes either of them, for the reason `license_live.rs`
//! gives.

use adguard_core::diagnostics::{self, Inputs, Report};
use adguard_core::Cli;

#[test]
fn the_real_report_carries_no_licence_and_no_home_directory() {
    let cli = match Cli::discover() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("skipping: {err}");
            return;
        }
    };

    // One after the other, never together — contract §3.
    let licence = cli.license().ok();
    let report = Report::build(&Inputs::collect(&cli));

    let home = std::env::var("HOME").ok();
    let text = diagnostics::redact_home(&report.text("test"), home.as_deref());

    for section in ["AdGuard CLI", "Proxy", "Root helper", "HTTPS filtering", "DNS"] {
        assert!(text.contains(&format!("\n{section}\n")), "no {section} section");
    }

    if let Some(licence) = licence {
        assert!(
            !licence.key.is_empty() && !text.contains(&licence.key),
            "the report carries the licence key"
        );
        assert!(
            !licence.owner.is_empty() && !text.contains(&licence.owner),
            "the report carries the licence owner"
        );
    }
    if let Some(home) = home.filter(|home| home.len() > 1) {
        assert!(!text.contains(&home), "the report carries the home directory");
    }
}
