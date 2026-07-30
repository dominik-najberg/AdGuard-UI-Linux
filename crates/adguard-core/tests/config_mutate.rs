//! The config write path: a real `adguard-cli config set`, verified against
//! the real `proxy.yaml` — the same act -> re-read -> reconcile sequence a
//! Protection switch performs.
//!
//! **`#[ignore]`d on purpose.** This mutates the machine's actual AdGuard
//! configuration, so it must never run as part of a plain `cargo test`:
//!
//! ```text
//! cargo test -p adguard-core --test config_mutate -- --ignored --nocapture
//! ```
//!
//! It toggles stealth mode and puts it back, whatever it started as. That
//! target is deliberate: of the six Protection switches it is the one whose
//! sub-settings are inert while it is off, so a failure part-way through
//! leaves nothing half-configured.

use std::sync::{Mutex, MutexGuard};

use adguard_core::config::key;
use adguard_core::{AddressPlan, Cli, Config, Toggle};

/// The switch under test. Restored to its original value before returning.
const SUBJECT: Toggle = Toggle::StealthMode;

/// Cargo runs a test binary's tests on several threads at once, and every test
/// here shares one mutable resource: the machine's `proxy.yaml`. Two of them
/// drive the same key and a third asserts the file is byte-identical across a
/// span of time, so unsynchronised they interleave and fail at random.
static CONFIG: Mutex<()> = Mutex::new(());

/// Take exclusive use of the machine's config for the duration of a test.
///
/// A panic in one test poisons the mutex; recovering from that is right here,
/// since the guarded state lives on disk and the [`Restore`] guard puts it
/// back regardless. Failing every later test with a poison error would only
/// hide the original failure.
fn exclusive() -> MutexGuard<'static, ()> {
    CONFIG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Puts a toggle back the way it was found, **including when the test panics**.
///
/// Without this, an assertion that fires between the two halves of a
/// round-trip leaves the machine's protection settings inverted — a test that
/// silently changes the user's security posture on failure is much worse than
/// one that simply fails.
struct Restore<'a> {
    cli: &'a Cli,
    toggle: Toggle,
    original: bool,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        // Unconditional, deliberately — no "is it already right?" shortcut.
        // `bool_at` reads `0` and `false` as the same value, so a semantic
        // comparison would skip the write and leave a type-punned literal
        // sitting in the user's file. Rewriting always restores the exact
        // spelling as well as the value, and a redundant `config set` costs
        // about 20 ms.
        match self.cli.set_bool(self.toggle.key(), self.original) {
            Ok(_) => eprintln!("restored {} = {}", self.toggle.key(), self.original),
            // Already unwinding, most likely — report loudly and do not panic
            // again, which would abort the process and lose the real failure.
            Err(err) => eprintln!(
                "WARNING: could not restore {} to {}: {err}",
                self.toggle.key(),
                self.original
            ),
        }
    }
}

/// Read the flag straight from `proxy.yaml` — the only trustworthy witness,
/// since `config set` prints "Config has been updated" for a no-op and even
/// for a change it declined to make.
fn current(toggle: Toggle) -> Option<bool> {
    Config::load().expect("proxy.yaml should be readable").toggle(toggle)
}

#[test]
#[ignore = "mutates the machine's AdGuard configuration"]
fn toggling_a_protection_switch_round_trips() {
    let _guard = exclusive();
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let Some(original) = current(SUBJECT) else {
        eprintln!("skipping: {} not present in proxy.yaml", SUBJECT.key());
        return;
    };
    eprintln!("{} starts = {original}", SUBJECT.key());
    let _restore = Restore {
        cli: &cli,
        toggle: SUBJECT,
        original,
    };

    // Away from the starting state, then back to it.
    for target in [!original, original] {
        let applied = cli
            .set_bool(SUBJECT.key(), target)
            .unwrap_or_else(|err| panic!("config set {} {target} failed: {err}", SUBJECT.key()));

        assert_eq!(
            current(SUBJECT),
            Some(target),
            "config set reported success but proxy.yaml still disagrees"
        );
        eprintln!("set {target} confirmed (restart_required = {})", applied.restart_required);
    }

    assert_eq!(
        current(SUBJECT),
        Some(original),
        "test did not restore the original value"
    );
}

/// `config set` writes a single line and leaves every comment alone. This is
/// the load-bearing claim behind "never serialise proxy.yaml ourselves" — if
/// it were false, the GUI would be silently shredding the file's 100-odd lines
/// of upstream documentation on every switch flip.
#[test]
#[ignore = "mutates the machine's AdGuard configuration"]
fn a_write_disturbs_exactly_one_line() {
    let _guard = exclusive();
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let path = adguard_core::paths::config_file().expect("config path");
    let Some(original) = current(SUBJECT) else {
        eprintln!("skipping: {} not present in proxy.yaml", SUBJECT.key());
        return;
    };

    let _restore = Restore {
        cli: &cli,
        toggle: SUBJECT,
        original,
    };

    let before = std::fs::read_to_string(&path).expect("read proxy.yaml");
    cli.set_bool(SUBJECT.key(), !original).expect("config set should be accepted");
    let after = std::fs::read_to_string(&path).expect("read proxy.yaml");

    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    assert_eq!(
        before_lines.len(),
        after_lines.len(),
        "the line count changed — the file was rewritten, not edited"
    );

    let changed: Vec<usize> = before_lines
        .iter()
        .zip(&after_lines)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, _)| index + 1)
        .collect();
    assert_eq!(changed.len(), 1, "expected one changed line, got {changed:?}");
    let written = after_lines[changed[0] - 1];
    eprintln!("only line {} changed: {written:?}", changed[0]);

    // `set_bool` must emit lowercase `true`/`false`. The CLI also accepts
    // `1`/`0`, which land in the YAML as *integers* — legal to the CLI, a
    // type-pun to everyone else. Nothing else catches this: `bool_at` reads
    // both spellings by design, so a regression to `1`/`0` would round-trip
    // perfectly and leave the file quietly wrong.
    let value = written
        .split_once(':')
        .map(|(_, value)| value.trim())
        .unwrap_or_default();
    assert!(
        value == "true" || value == "false",
        "set_bool wrote {value:?}; it must write lowercase true/false, not 1/0",
    );

    let comments = |text: &str| text.lines().filter(|line| line.trim_start().starts_with('#')).count();
    assert_eq!(
        comments(&before),
        comments(&after),
        "comment lines were lost — the whole no-YAML-writes rule rests on this"
    );
}

/// The keys the Protection page reads must be the same strings the CLI writes,
/// since one [`Toggle::key`] constant drives both directions. `config get`
/// answering `<key> = <value>` proves the CLI still recognises the path; the
/// semantic failure would be `'<key>' not found`, at exit 0 (contract §3).
///
/// Read-only, but it shells out, so it belongs behind `--ignored` with the
/// rest rather than firing on every `cargo test`.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn toggle_keys_are_the_keys_the_cli_knows() {
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };

    for toggle in Toggle::ALL {
        let key = toggle.key();
        let out = cli
            .run(&["config", "get", key])
            .expect("config get should not be a malformed command line");
        assert!(
            out.stdout.contains(&format!("{key} = ")),
            "`config get {key}` did not recognise the key: {:?}",
            out.stdout,
        );
        eprintln!("{key}: recognised");
    }
}

/// Every measured refusal shape, all of which exit 0 and change nothing.
/// Read-only in effect, but it invokes the real binary, so it stays behind
/// `--ignored` with the rest.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn refusals_are_reported_as_failure() {
    let _guard = exclusive();
    let Ok(cli) = Cli::discover() else {
        eprintln!("skipping: adguard-cli not installed");
        return;
    };
    let before = std::fs::read_to_string(
        adguard_core::paths::config_file().expect("config path"),
    )
    .expect("read proxy.yaml");

    let cases: [(&str, &str, &str); 4] = [
        ("unknown key", "bogus_key_xyz", "true"),
        ("wrong value type", key::STEALTH_MODE, "bogus"),
        ("value outside the enum", "https_filtering.filter_secure_dns_mode", "nope"),
        ("a list, not a setting", "filters", "something"),
    ];

    for (label, k, value) in cases {
        match cli.config_set(k, value) {
            Ok(applied) => panic!("{label}: `{k} = {value}` was accepted ({applied:?})"),
            Err(err) => eprintln!("{label}: refused with {err}"),
        }
    }

    let after = std::fs::read_to_string(
        adguard_core::paths::config_file().expect("config path"),
    )
    .expect("read proxy.yaml");
    assert_eq!(before, after, "a refusal still modified proxy.yaml");
}

/// Leaving loopback needs `listen_auth` fully configured **first**: the CLI
/// otherwise tries to collect a username interactively, finds no TTY, and
/// silently keeps the old address while still printing "Config has been
/// updated". This is the trap [`config::listen_address_plan`] exists to avoid.
///
/// Deliberately *not* executed against the machine — it would briefly expose
/// the proxy. It asserts only that the plan suits the config actually present.
/// The behaviour itself is exercised in `config_sandbox.rs`, against a copy.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_listen_address_plan_matches_the_machine() {
    // Reads the same file the mutating tests write, so it queues behind them
    // rather than catching a half-written config.
    let _guard = exclusive();
    let Ok(config) = Config::load() else {
        eprintln!("skipping: proxy.yaml not readable");
        return;
    };
    let auth = config.listen_auth();

    let plan = adguard_core::config::listen_address_plan("0.0.0.0", auth);
    eprintln!("plan for 0.0.0.0 with {auth:?}: {plan:?}");

    match &plan {
        AddressPlan::NeedsCredentials { .. } => {
            assert!(
                !auth.username_set || !auth.password_set,
                "refused with both credentials present"
            );
            assert!(plan.calls().is_empty(), "a refusal must issue nothing");
        }
        AddressPlan::Calls(calls) => {
            assert!(auth.username_set && auth.password_set);
            assert_eq!(
                calls.last().map(|(k, _)| *k),
                Some(key::LISTEN_ADDRESS),
                "the address must be written last"
            );
            if auth.enabled {
                assert_eq!(calls.len(), 1, "auth already on: only the address needs writing");
            } else {
                assert_eq!(
                    calls.first().map(|(k, _)| *k),
                    Some(key::LISTEN_AUTH_ENABLED),
                    "authentication must be enabled before the address leaves loopback"
                );
                assert_eq!(calls.len(), 2);
            }
        }
    }
}
