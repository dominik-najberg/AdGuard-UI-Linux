//! The write path: a real `adguard-cli` invocation, verified against the real
//! database — the same act -> re-read -> reconcile sequence a switch performs.
//!
//! **`#[ignore]`d on purpose.** This mutates the machine's actual AdGuard
//! configuration, so it must never run as part of a plain `cargo test`:
//!
//! ```text
//! cargo test -p adguard-core --test filters_mutate -- --ignored --nocapture
//! ```
//!
//! It toggles the user-rules pseudo-filter and puts it back, whatever it
//! started as. That target is deliberate: it needs no `add`/`remove` (so the
//! machine's subscriptions are never touched) and it exercises the sharpest
//! edge in the write path — the `-2147483648` sentinel reaching a CLI whose
//! argument parser could plausibly read it as a flag.

use adguard_core::filters::Catalogue;
use adguard_core::{Cli, Filter, FilterAction, FilterSet};

const SET: FilterSet = FilterSet::Http;
const ID: i64 = Filter::USER_RULES_ID;

/// Read the flags straight from the database — the only trustworthy witness,
/// since the CLI reports semantic failures at exit 0.
fn enabled(catalogue: &Catalogue) -> bool {
    catalogue
        .state(ID)
        .expect("state query should not error")
        .expect("user-rules row should exist")
        .enabled
}

#[test]
#[ignore = "mutates the machine's AdGuard configuration"]
fn toggling_user_rules_round_trips() {
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let Ok(catalogue) = Catalogue::open_set(SET) else {
        eprintln!("skipping: filter database not present");
        return;
    };

    let original = enabled(&catalogue);
    eprintln!("user rules start enabled = {original}");

    // Away from the starting state, then back to it.
    for target in [!original, original] {
        let action = if target {
            FilterAction::Enable
        } else {
            FilterAction::Disable
        };

        cli.filter_action(SET, action, ID)
            .unwrap_or_else(|err| panic!("{action:?} on the user-rules sentinel failed: {err}"));

        assert_eq!(
            enabled(&catalogue),
            target,
            "{action:?} reported success but the database still disagrees"
        );
        eprintln!("{action:?} confirmed: enabled = {target}");
    }

    assert_eq!(
        enabled(&catalogue),
        original,
        "test did not restore the original state"
    );
}

/// The failure the switch logic exists to avoid: `enable` on a filter nobody
/// added is refused at exit 0 and changes nothing, which is why
/// [`Filter::action_for`] reaches for `add` instead.
///
/// Read-only in effect — the CLI refuses, so nothing is written — but it does
/// invoke the real binary, so it stays behind `--ignored` with the rest.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn enabling_an_uninstalled_filter_is_refused() {
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let Ok(catalogue) = Catalogue::open_set(SET) else {
        eprintln!("skipping: filter database not present");
        return;
    };
    let locale = adguard_core::Locale::english();

    let filters = catalogue.filters(&locale).expect("should read catalogue");
    let Some(uninstalled) = filters.iter().find(|filter| !filter.installed) else {
        eprintln!("skipping: every filter is installed on this machine");
        return;
    };

    let err = cli
        .filter_action(SET, FilterAction::Enable, uninstalled.id)
        .expect_err("enabling an uninstalled filter should be reported as failure");
    eprintln!("refused with: {err}");

    // And it really did nothing.
    let state = catalogue
        .state(uninstalled.id)
        .expect("state query")
        .expect("row exists");
    assert!(!state.installed, "refusal still installed the filter");
    assert!(!state.enabled, "refusal still enabled the filter");
}
