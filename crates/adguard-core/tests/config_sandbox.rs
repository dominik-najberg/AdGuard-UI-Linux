//! The config write path, run against a **throwaway copy** of `proxy.yaml`.
//!
//! The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`
//! (measured), so [`Cli::with_xdg_data_home`] gives the real binary a complete
//! AdGuard configuration that belongs to the test. That matters because the
//! interesting cases here are the ones nobody should run against a real
//! machine: they expose the proxy on `0.0.0.0`, blank the proxy password, and
//! set ports to values that would take the listener down.
//!
//! This is where the Advanced page's write path is actually covered.
//! `config_mutate.rs` still drives the machine's own config — it has to, since
//! only that proves our reads point at the file AdGuard really uses — but it is
//! kept to one boolean round-trip behind a restoring guard.
//!
//! ```text
//! cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
//! ```
//!
//! Still `#[ignore]`d, for the same reason as every other suite that shells
//! out: it needs `adguard-cli` installed, which a CI runner will not have. It
//! does **not** touch the machine's configuration, and
//! [`the_machine_config_was_not_touched`] asserts as much.

use std::path::{Path, PathBuf};

use adguard_core::config::{key, listen_address_plan};
use adguard_core::{AddressPlan, Cli, Config};

/// A scratch `$XDG_DATA_HOME` seeded with a copy of the machine's `proxy.yaml`.
///
/// The real file is the right seed rather than a synthetic fixture: it is the
/// shape the GUI will meet, comments and all, and it keeps these tests honest
/// about the config AdGuard actually ships.
///
/// # Only `config` subcommands work in here
///
/// A sandbox is an *unlicensed* install, and copying the licence database
/// across does not change that — measured, so the licence evidently lives
/// somewhere other than the data directory. `status`, `license` and `filters
/// list` therefore fail here with **exit 1 and output on stderr**:
///
/// ```text
/// You need to activate an AdGuard license to use this command
/// ```
///
/// The `config` family, and `--version`, need no licence and behave exactly as
/// they do against the real data directory — which is all this suite exercises.
/// Anything licence-gated belongs in `config_mutate.rs` or `filters_*.rs`,
/// against the real install.
struct Sandbox {
    root: PathBuf,
    cli: Cli,
}

impl Sandbox {
    /// `None` when the CLI or the reference config is missing, so the suite
    /// skips rather than fails on a machine without AdGuard.
    fn new(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };
        let seed = adguard_core::paths::config_file()?;
        if !seed.is_file() {
            eprintln!("skipping: {} not present", seed.display());
            return None;
        }

        // One directory per test: this binary's tests run concurrently and each
        // one writes its own config.
        let root = std::env::temp_dir().join(format!("adguard-ui-sandbox-{name}-{}", std::process::id()));
        let data = root.join("adguard-cli");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&data).expect("create sandbox");
        std::fs::copy(&seed, data.join("proxy.yaml")).expect("seed sandbox config");

        Some(Self {
            cli: cli.with_xdg_data_home(&root),
            root,
        })
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("adguard-cli").join("proxy.yaml")
    }

    /// Re-read the sandbox config. The only trustworthy witness to a write:
    /// `config set` prints `Config has been updated` for a no-op *and* for a
    /// change it silently declined to make.
    fn config(&self) -> Config {
        Config::read(&self.config_path()).expect("sandbox proxy.yaml should parse")
    }

    fn set(&self, key: &str, value: &str) {
        self.cli
            .config_set(key, value)
            .unwrap_or_else(|err| panic!("config set {key} {value} refused: {err}"));
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A sandbox is unlicensed by construction, which makes it the only place the
/// lapsed-licence path can be exercised on this machine — the real install is
/// `APP_ACTIVE`, so the bug this guards was unreachable here and shipped
/// anyway.
///
/// Measured: `status`, `license` and `filters list` each exit 1 with stdout
/// empty and one sentence on stderr. Before the mapping existed that became
/// `BadInvocation` — "adguard-cli rejected `status`" — blaming us for the
/// user's expired subscription.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn licence_gated_commands_name_the_licence() {
    let Some(sandbox) = Sandbox::new("unlicensed") else {
        return;
    };

    let err = sandbox.cli.status().expect_err("a sandbox is unlicensed");
    eprintln!("status in an unlicensed install -> {err:?}");
    assert!(
        matches!(err, adguard_core::Error::Unlicensed { .. }),
        "expected Unlicensed, got {err:?}"
    );
    assert!(
        !err.to_string().contains("rejected"),
        "must not read as our own malformed command line: {err}"
    );

    // The config family is what still works there, which is the premise the
    // rest of this suite rests on.
    sandbox
        .cli
        .config_set(key::LOG_LEVEL, "info")
        .expect("the config family is not licence-gated");
}

/// The sandbox is only useful if the CLI really does follow `$XDG_DATA_HOME`.
/// Prove it before trusting anything else here: write a value the machine's
/// config does not have, and check the machine's config did not get it.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_sandbox_is_a_different_file_from_the_machine_config() {
    let Some(sandbox) = Sandbox::new("isolation") else { return };
    let real_path = adguard_core::paths::config_file().expect("config path");
    let real_before = std::fs::read_to_string(&real_path).expect("read real config");

    sandbox.set(key::WORKER_THREADS, "17");
    assert_eq!(
        sandbox.config().int_at(key::WORKER_THREADS),
        Some(17),
        "the sandbox config did not take the write"
    );

    let real_after = std::fs::read_to_string(&real_path).expect("read real config");
    assert_eq!(
        real_before, real_after,
        "writing to the sandbox changed the machine's proxy.yaml"
    );
}

/// The whole reason `listen_address_plan` refuses instead of returning calls.
///
/// With `listen_auth.enabled` on but a credential empty, `config set
/// listen_address 0.0.0.0` prompts for a username, finds no stdin, keeps the
/// old address — and still reports success. An earlier version of the plan
/// enabled authentication and then wrote the address, which on a machine with a
/// blank password would have done exactly this.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn enabling_auth_is_not_enough_to_leave_loopback() {
    let Some(sandbox) = Sandbox::new("auth-insufficient") else { return };

    // Each row: which credential is blanked, with authentication already on.
    for blanked in [
        key::LISTEN_AUTH_USERNAME,
        key::LISTEN_AUTH_PASSWORD,
    ] {
        sandbox.set(key::LISTEN_ADDRESS, "127.0.0.1");
        sandbox.set(key::LISTEN_AUTH_USERNAME, "admin");
        sandbox.set(key::LISTEN_AUTH_PASSWORD, "admin");
        sandbox.set(key::LISTEN_AUTH_ENABLED, "true");
        sandbox.set(blanked, "");

        // The CLI accepts the command...
        let applied = sandbox.cli.config_set(key::LISTEN_ADDRESS, "0.0.0.0");
        assert!(
            applied.is_ok(),
            "the CLI is expected to *accept* this and do nothing: {applied:?}"
        );
        // ...and the file says it did nothing at all.
        assert_eq!(
            sandbox.config().str_at(key::LISTEN_ADDRESS),
            Some("127.0.0.1"),
            "with {blanked} empty the address must not have moved"
        );

        // Which is precisely what the plan predicts, without issuing a call.
        let plan = listen_address_plan("0.0.0.0", sandbox.config().listen_auth());
        assert!(
            matches!(plan, AddressPlan::NeedsCredentials { .. }),
            "the plan should refuse while {blanked} is empty, got {plan:?}"
        );
        assert!(plan.calls().is_empty());
        eprintln!("{blanked} empty: address held at 127.0.0.1, plan refused");
    }
}

/// The plan's happy path, executed. Authentication on with both credentials
/// present, then the address — and this time the file really changes.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_plan_moves_the_address_when_it_says_it_will() {
    let Some(sandbox) = Sandbox::new("plan-happy") else { return };

    sandbox.set(key::LISTEN_AUTH_USERNAME, "admin");
    sandbox.set(key::LISTEN_AUTH_PASSWORD, "admin");
    sandbox.set(key::LISTEN_AUTH_ENABLED, "false");
    sandbox.set(key::LISTEN_ADDRESS, "127.0.0.1");

    let plan = listen_address_plan("0.0.0.0", sandbox.config().listen_auth());
    let calls = plan.calls();
    assert_eq!(
        calls.len(),
        2,
        "auth is off but usable, so it needs enabling first: {plan:?}"
    );

    // Issue the plan exactly as the GUI does, in order.
    for (k, value) in calls {
        sandbox.set(k, value);
    }

    let config = sandbox.config();
    assert_eq!(config.str_at(key::LISTEN_ADDRESS), Some("0.0.0.0"));
    assert_eq!(config.listen_auth_enabled(), Some(true));
    assert!(config.listens_beyond_loopback());
    eprintln!("plan executed: exposed on 0.0.0.0 with authentication on");
}

/// Reversing the plan is the mistake it exists to prevent: writing the address
/// before enabling authentication leaves the address where it was, while both
/// commands report success.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_reversed_plan_silently_does_nothing() {
    let Some(sandbox) = Sandbox::new("plan-reversed") else { return };

    sandbox.set(key::LISTEN_AUTH_USERNAME, "admin");
    sandbox.set(key::LISTEN_AUTH_PASSWORD, "admin");
    sandbox.set(key::LISTEN_AUTH_ENABLED, "false");
    sandbox.set(key::LISTEN_ADDRESS, "127.0.0.1");

    // Address first — accepted, and ineffective.
    sandbox.set(key::LISTEN_ADDRESS, "0.0.0.0");
    assert_eq!(
        sandbox.config().str_at(key::LISTEN_ADDRESS),
        Some("127.0.0.1"),
        "the reversed order is supposed to fail silently; if this passes the \
         CLI changed and listen_address_plan can be simplified"
    );
}

/// The safety-critical direction. Measured from every broken starting state:
/// writing a loopback address always works and never prompts, so a user who is
/// exposed with unusable credentials can always be brought back.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_retreat_to_loopback_always_works() {
    let Some(sandbox) = Sandbox::new("retreat") else { return };

    // Get exposed the legitimate way first.
    sandbox.set(key::LISTEN_AUTH_USERNAME, "admin");
    sandbox.set(key::LISTEN_AUTH_PASSWORD, "admin");
    sandbox.set(key::LISTEN_AUTH_ENABLED, "true");
    sandbox.set(key::LISTEN_ADDRESS, "0.0.0.0");
    assert!(sandbox.config().listens_beyond_loopback(), "setup failed");

    // Now break authentication as thoroughly as possible.
    sandbox.set(key::LISTEN_AUTH_USERNAME, "");
    sandbox.set(key::LISTEN_AUTH_PASSWORD, "");
    sandbox.set(key::LISTEN_AUTH_ENABLED, "false");

    let auth = sandbox.config().listen_auth();
    assert!(!auth.is_complete(), "setup failed: auth should be unusable");

    let plan = listen_address_plan("127.0.0.1", auth);
    assert!(
        !plan.calls().is_empty(),
        "the retreat must never be blocked: {plan:?}"
    );
    for (k, value) in plan.calls() {
        sandbox.set(k, value);
    }

    let config = sandbox.config();
    assert_eq!(config.str_at(key::LISTEN_ADDRESS), Some("127.0.0.1"));
    assert!(
        !config.listens_beyond_loopback(),
        "the proxy is still exposed after a retreat to loopback"
    );
}

/// `listen_address` is one of the few settings the CLI does validate, and
/// `localhost` — which `is_loopback` accepts when *reading* — is refused when
/// writing. The entry row must not offer it.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_cli_validates_the_listen_address() {
    let Some(sandbox) = Sandbox::new("address-validation") else { return };
    sandbox.set(key::LISTEN_AUTH_USERNAME, "admin");
    sandbox.set(key::LISTEN_AUTH_PASSWORD, "admin");
    sandbox.set(key::LISTEN_AUTH_ENABLED, "true");

    for accepted in ["127.0.0.1", "0.0.0.0", "::1", "::", "192.168.1.10"] {
        sandbox.set(key::LISTEN_ADDRESS, accepted);
        assert_eq!(
            sandbox.config().str_at(key::LISTEN_ADDRESS),
            Some(accepted),
            "{accepted} should have been written"
        );
    }

    sandbox.set(key::LISTEN_ADDRESS, "127.0.0.1");
    for refused in ["localhost", "", "not an address", "1.2.3.4.5", "0.0.0.0:3128"] {
        let outcome = sandbox.cli.config_set(key::LISTEN_ADDRESS, refused);
        assert!(
            outcome.is_err(),
            "{refused:?} should be refused, got {outcome:?}"
        );
        assert_eq!(
            sandbox.config().str_at(key::LISTEN_ADDRESS),
            Some("127.0.0.1"),
            "a refusal changed the file"
        );
    }
}

/// Every `Number` setting's "off" and boundary values must survive a
/// round-trip, `-1` included: it is how both manual proxy ports are switched
/// off, and a leading `-` is exactly the shape an argument parser eats.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn negative_port_values_reach_the_file() {
    let Some(sandbox) = Sandbox::new("negative-ports") else { return };

    for port_key in [key::LISTEN_PORT_HTTP, key::LISTEN_PORT_SOCKS5] {
        for value in [-1, 1, 3129, 65535] {
            sandbox
                .cli
                .set_int(port_key, value)
                .unwrap_or_else(|err| panic!("set_int {port_key} {value}: {err}"));
            assert_eq!(
                sandbox.config().int_at(port_key),
                Some(value),
                "{port_key} did not round-trip {value}"
            );
        }
    }
}

/// The CLI type-checks an integer setting and stops there. These four are all
/// accepted, which is why [`adguard_core::Setting::permits_number`] exists —
/// and `3.5` is the worst of them, landing a float where every later read
/// expects an integer.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_cli_range_checks_nothing() {
    let Some(sandbox) = Sandbox::new("no-range-check") else { return };

    for value in ["65536", "99999", "-2", "0"] {
        sandbox.set(key::LISTEN_PORT_HTTP, value);
        assert_eq!(
            sandbox.config().int_at(key::LISTEN_PORT_HTTP).map(|v| v.to_string()),
            Some(value.to_owned()),
            "the CLI is expected to accept {value} for a port"
        );
    }

    // A float is accepted, and then reads back as *nothing* — `int_at` will not
    // pretend it is an integer, so the row renders as unavailable.
    sandbox.set(key::LISTEN_PORT_HTTP, "3.5");
    assert_eq!(
        sandbox.config().int_at(key::LISTEN_PORT_HTTP),
        None,
        "a float port should read as unavailable, not as some integer"
    );

    // Only a non-number is refused.
    for value in ["abc", ""] {
        assert!(
            sandbox.cli.config_set(key::LISTEN_PORT_HTTP, value).is_err(),
            "{value:?} should be refused for an integer setting"
        );
    }
}

/// Enum settings are written back verbatim, so the file can hold a spelling the
/// comment does not use. `choice_at` has to find it anyway.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn enum_settings_round_trip_in_any_case() {
    let Some(sandbox) = Sandbox::new("enums") else { return };

    for (k, options, values) in [
        (key::LOG_LEVEL, adguard_core::model::LOG_LEVELS, ["info", "DEBUG", "Trace"]),
        (
            key::OUTBOUND_MODE,
            adguard_core::model::OUTBOUND_MODES,
            ["HTTP", "socks5", "Https"],
        ),
    ] {
        for value in values {
            sandbox.set(k, value);
            let config = sandbox.config();
            assert_eq!(
                config.str_at(k),
                Some(value),
                "{k} should hold exactly what the CLI was given"
            );
            assert!(
                config.choice_at(k, options).is_some(),
                "choice_at failed to match {value:?} for {k}, which the CLI accepted"
            );
        }
    }

    for k in [key::LOG_LEVEL, key::OUTBOUND_MODE] {
        assert!(
            sandbox.cli.config_set(k, "bogus").is_err(),
            "{k} should refuse a value outside its enum"
        );
    }
}

/// Credentials go through `argv` and come back in the CLI's echo. The write has
/// to work for awkward values, and the error path must not leak them.
///
/// The two `-`-prefixed entries are the ones that matter: without the `--` guard
/// in `config_set` the CLI reads them as options and exits 1 with
/// *"\<value\> is required"*, writing nothing — and our own error type then
/// quotes the whole command line, password included.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn credentials_round_trip_without_shell_quoting() {
    let Some(sandbox) = Sandbox::new("credentials") else { return };

    // No shell is involved — `Command::args` passes these through untouched —
    // so the characters that would need escaping in a shell must survive.
    for password in [
        "hunter2",
        "p@ss \"w#rd's $HOME`x`",
        "; rm -rf /",
        "--flag-shaped",
        "-abc",
        "-1",
        "",
    ] {
        sandbox
            .cli
            .set_secret(key::LISTEN_AUTH_PASSWORD, password)
            .unwrap_or_else(|err| panic!("set_secret refused {password:?}: {err}"));
        assert_eq!(
            sandbox.config().str_at(key::LISTEN_AUTH_PASSWORD),
            Some(password),
            "password did not round-trip"
        );
    }
}

/// A refused secret write must not put the secret in the error, whichever way
/// it was refused. `listen_auth.password` accepts anything, so the refusal has
/// to be provoked with a key that will not take a string.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_refused_secret_write_does_not_leak_the_secret() {
    let Some(sandbox) = Sandbox::new("secret-leak") else { return };
    const SECRET: &str = "hunter2";

    // An integer setting refuses a non-numeric value (measured: "Invalid value
    // type: The value of the setting must be an integer").
    let err = sandbox
        .cli
        .set_secret(key::LISTEN_PORT_HTTP, SECRET)
        .expect_err("an integer setting should refuse a word");
    assert!(
        !err.to_string().contains(SECRET),
        "the secret leaked into the error: {err}"
    );

    // And an unknown key, which quotes the key rather than the value.
    let err = sandbox
        .cli
        .set_secret("bogus_key_xyz", SECRET)
        .expect_err("an unknown key should be refused");
    assert!(
        !err.to_string().contains(SECRET),
        "the secret leaked into the error: {err}"
    );
}

/// The one-line rule, re-asserted for the Advanced page's own settings. The
/// no-YAML-writes rule rests on it, and these keys live in four different
/// sections of the file — including two nested ones.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn advanced_writes_disturb_one_line_each_and_keep_every_comment() {
    let Some(sandbox) = Sandbox::new("surgical") else { return };
    let path = sandbox.config_path();
    let original = std::fs::read_to_string(&path).expect("read sandbox config");
    let comments = |text: &str| {
        text.lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .count()
    };

    // One representative write per group, each in a different section.
    let writes: [(&str, &str); 5] = [
        (key::LOG_LEVEL, "debug"),
        (key::LISTEN_PORT_HTTP, "-1"),
        (key::WORKER_THREADS, "8"),
        (key::OUTBOUND_MODE, "SOCKS5"),
        (key::LISTEN_AUTH_ENABLED, "true"),
    ];

    for (k, value) in writes {
        let before = std::fs::read_to_string(&path).expect("read sandbox config");
        sandbox.set(k, value);
        let after = std::fs::read_to_string(&path).expect("read sandbox config");

        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(
            before_lines.len(),
            after_lines.len(),
            "{k}: the line count changed — the file was rewritten, not edited"
        );

        let changed: Vec<usize> = before_lines
            .iter()
            .zip(&after_lines)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(changed.len(), 1, "{k}: expected one changed line, got {changed:?}");
        eprintln!("{k} = {value}: only line {} changed", changed[0]);
    }

    let final_text = std::fs::read_to_string(&path).expect("read sandbox config");
    assert_eq!(
        comments(&original),
        comments(&final_text),
        "comment lines were lost across five writes"
    );
}

/// Every key the Advanced page reads must still be one `config get` resolves.
/// The semantic failure is `'<key>' not found` at exit 0 (contract §3), so a
/// renamed key would otherwise show up only as a permanently blank row.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn every_advanced_key_is_one_the_cli_knows() {
    let Some(sandbox) = Sandbox::new("keys-known") else { return };

    for group in &adguard_core::ADVANCED {
        for setting in group.settings {
            let out = sandbox
                .cli
                .run(&["config", "get", setting.key])
                .expect("config get should not be a malformed command line");
            assert!(
                out.stdout.contains(&format!("{} = ", setting.key)),
                "`config get {}` did not recognise the key: {:?}",
                setting.key,
                out.stdout,
            );
        }
    }
}

/// **Every invocation rewrites `proxy.yaml`, including a read-only one.**
///
/// Measured for `--version`, `config get`, `config show`, `status` and
/// `license`: all of them write the file back and touch its mtime even when not
/// one byte changes. `--version` is the striking case — it has no reason to open
/// the config at all.
///
/// That is not a curiosity. The GUI polls `status` on a ~2 s timer, so a
/// `gio::FileMonitor` on `proxy.yaml` — which `architecture.md` §3 plans for, to
/// pick up hand edits — would fire every couple of seconds at idle, triggered by
/// nothing but our own polling. A monitor has to compare content rather than
/// trust the event.
///
/// If this test ever fails because the file stopped being touched, that is good
/// news and the monitor can be simplified.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn any_invocation_rewrites_the_config_and_touches_its_mtime() {
    let Some(sandbox) = Sandbox::new("mtime-churn") else { return };
    let path = sandbox.config_path();

    // Normalise first, so this measures a steady state rather than a repair.
    sandbox.cli.version().expect("--version should run unlicensed");

    // Licence-gated commands cannot run in a sandbox, so this uses the ones that
    // can. `status` and `license` were measured to behave identically.
    for command in [
        vec!["--version"],
        vec!["config", "get", "worker_threads"],
        vec!["config", "show", "listen_auth"],
    ] {
        let before = std::fs::read_to_string(&path).expect("read config");
        let mtime_before = std::fs::metadata(&path).and_then(|m| m.modified()).expect("mtime");

        // Filesystem timestamps need a moment to be distinguishable.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        sandbox.cli.run(&command).expect("command should run");

        let after = std::fs::read_to_string(&path).expect("read config");
        let mtime_after = std::fs::metadata(&path).and_then(|m| m.modified()).expect("mtime");

        assert_eq!(
            before, after,
            "`{}` changed the contents of an already-normalised config",
            command.join(" ")
        );
        assert_ne!(
            mtime_before, mtime_after,
            "`{}` left the mtime alone — a FileMonitor would no longer churn, and \
             this test's premise is out of date",
            command.join(" ")
        );
    }
}

/// The rewrite **adds a missing key with its default**, and leaves an invalid
/// value alone.
///
/// So the two "unavailable" rows the Advanced page can show are not the same
/// kind of thing: a missing key heals itself the next time anything runs the
/// CLI, while a value of the wrong type persists until someone edits the file.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_rewrite_restores_missing_keys_but_not_invalid_values() {
    let Some(sandbox) = Sandbox::new("self-heal") else { return };
    let path = sandbox.config_path();

    // Remove a key outright, and type-pun another.
    let doctored = std::fs::read_to_string(&path)
        .expect("read config")
        .lines()
        .filter(|line| !line.starts_with("  host: "))
        .map(|line| {
            if line.starts_with("worker_threads:") {
                "worker_threads: 128".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{doctored}\n")).expect("write doctored config");

    assert_eq!(
        sandbox.config().str_at(key::OUTBOUND_HOST),
        None,
        "setup failed: the host key should be gone"
    );

    // Any invocation triggers the rewrite; this one needs no licence.
    sandbox.cli.version().expect("--version should run");

    let config = sandbox.config();
    assert!(
        config.str_at(key::OUTBOUND_HOST).is_some(),
        "a missing key should have been restored with its default"
    );
    assert_eq!(
        config.int_at(key::WORKER_THREADS),
        Some(128),
        "an out-of-range value should have been left alone, not corrected"
    );
}

/// A belt-and-braces check that nothing in this file leaked out of its sandbox.
/// Named last so its failure reads as "the suite escaped", not "a write broke".
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_machine_config_was_not_touched() {
    let Some(path) = adguard_core::paths::config_file() else { return };
    if !path.is_file() {
        return;
    }
    let before = std::fs::read_to_string(&path).expect("read real config");

    {
        let Some(sandbox) = Sandbox::new("no-escape") else { return };
        sandbox.set(key::LISTEN_ADDRESS, "0.0.0.0");
        sandbox.set(key::LISTEN_AUTH_PASSWORD, "");
        sandbox.set(key::LISTEN_PORT_HTTP, "-1");
        assert!(!sandbox.config_path().starts_with(
            path.parent().unwrap_or(Path::new("/"))
        ));
    }

    assert_eq!(
        before,
        std::fs::read_to_string(&path).expect("read real config"),
        "the sandbox suite modified the machine's proxy.yaml"
    );
}
