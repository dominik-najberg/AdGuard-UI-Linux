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
use adguard_core::{Cli, Consent, Filter, FilterAction, FilterSet};

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

        cli.filter_action(SET, action, ID, Consent::Withheld)
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
        .filter_action(SET, FilterAction::Enable, uninstalled.id, Consent::Withheld)
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

/// The consent gate, both ways round, against the real binary — for both sets.
///
/// **This one does touch a subscription**, unlike the user-rules test above,
/// and there is no way around it: the gate is a property of a catalogue group
/// and the user-rules pseudo-filter is in neither. It picks a list from the
/// gated group which the machine has **not** added, adds and removes it, and
/// skips entirely if every one of them is already installed — so it never
/// disturbs a list the user chose to have. A failure mid-way leaves the list
/// added and disabled, which `filters remove` undoes.
///
/// What it pins is the pair of behaviours the fix rests on: withheld consent is
/// reported as a *failure* even though `filters add` opened with its own
/// success line, and granted consent actually gets the list switched on.
///
/// **Both sets, because assuming one of them was ungated is the whole of issue
/// #13.** The HTTP half passed for a release while the DNS Security group could
/// not be switched on at all, and nothing here was looking.
#[test]
#[ignore = "mutates the machine's AdGuard configuration"]
fn the_consent_gate_refuses_without_consent_and_yields_with_it() {
    for set in [FilterSet::Http, FilterSet::Dns] {
        eprintln!("--- {set:?} ---");
        check_consent_gate(set);
    }
}

fn check_consent_gate(set: FilterSet) {
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let Ok(catalogue) = Catalogue::open_set(set) else {
        eprintln!("skipping: filter database not present");
        return;
    };
    let locale = adguard_core::Locale::english();

    let filters = catalogue.filters(&locale).expect("should read catalogue");
    let Some(target) = filters
        .iter()
        .find(|filter| !filter.installed && filter.needs_consent(set, FilterAction::Add))
    else {
        eprintln!("skipping: every gated list is already added on this machine");
        return;
    };
    let id = target.id;
    eprintln!("using {} (id {id})", target.display_name());

    let state = |what: &str| {
        catalogue
            .state(id)
            .unwrap_or_else(|err| panic!("state query after {what}: {err}"))
            .unwrap_or_else(|| panic!("row {id} vanished after {what}"))
    };

    // Withheld: `add` prints `Filter [...] added` and then refuses to enable.
    // The success line must not carry the day.
    let err = cli
        .filter_action(set, FilterAction::Add, id, Consent::Withheld)
        .expect_err("an unanswered agreement must be reported as failure");
    eprintln!("withheld consent refused with: {err}");
    let after = state("the withheld attempt");
    assert!(!after.enabled, "the list was enabled without consent");

    // Granted: whichever action the observed state now calls for.
    let action = target.clone().action_for(true);
    let action = if after.installed { FilterAction::Enable } else { action };
    cli.filter_action(set, action, id, Consent::Granted)
        .unwrap_or_else(|err| panic!("{action:?} with consent granted failed: {err}"));
    assert!(state("consent granted").enabled, "consent granted but the list is still off");
    eprintln!("consent granted: enabled");

    // Put the machine back: `remove`, not `disable` — it was not added before.
    cli.filter_action(set, FilterAction::Remove, id, Consent::Withheld)
        .expect("removal should succeed");
    let restored = state("removal");
    assert!(!restored.installed, "test did not restore the original state");
    assert!(!restored.enabled, "test left the list enabled");
}
