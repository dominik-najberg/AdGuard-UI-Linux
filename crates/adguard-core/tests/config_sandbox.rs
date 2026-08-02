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
/// # Only `config` subcommands work in here, unless it is made licensed
///
/// A sandbox is an *unlicensed* install by default. `status`, `license` and
/// `filters list` fail here with **exit 1 and output on stderr**:
///
/// ```text
/// You need to activate an AdGuard license to use this command
/// ```
///
/// The `config` family, `--version` and — measured since — `activate` need no
/// licence and behave exactly as they do against the real data directory.
/// `activate` is the interesting one: it is the command that exists to *fix* an
/// unlicensed install, so an unlicensed sandbox is the only honest place to
/// exercise it.
///
/// **The licence lives in `adguard.conf`**, and it travels — measured, a
/// sandbox holding a copy of that one file reads back `APP_ACTIVE`. An earlier
/// revision of this comment said the licence "evidently lives somewhere other
/// than the data directory", inferred from copying `gm.db` and seeing no
/// change; that was the wrong file, not the wrong directory. [`Sandbox::licensed`]
/// is what that buys — the licence-gated commands become measurable against a
/// throwaway config, which is the only way [`Cli::configure`] could be covered
/// at all without resetting the author's own install.
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

    /// A sandbox with **no `proxy.yaml`** — a data directory as it is before
    /// `configure` has ever run.
    ///
    /// The state the first-run assistant exists for, and the one that is
    /// impossible to reach by deleting things from a normal sandbox without
    /// also deciding what else to delete. Nothing is copied in at all: the CLI
    /// creates the directory itself on first use.
    fn virgin(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };

        let root = std::env::temp_dir()
            .join(format!("adguard-ui-sandbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create sandbox");

        Some(Self {
            cli: cli.with_xdg_data_home(&root),
            root,
        })
    }

    /// Carry the machine's licence into this sandbox.
    ///
    /// Measured: the licence lives in `adguard.conf`, and a copy of that file is
    /// enough for `license` to answer `APP_ACTIVE` here. `None` when the machine
    /// has no licence to lend, so the licence-gated tests skip rather than fail
    /// on an unlicensed install.
    ///
    /// It copies a file holding the owner's e-mail and licence key into a temp
    /// directory that [`Drop`] removes. Nothing in these tests prints it —
    /// `License`'s `Debug` masks both fields — but it is worth knowing it is
    /// briefly on disk.
    fn licensed(self) -> Option<Self> {
        let source = adguard_core::paths::data_dir()?.join("adguard.conf");
        if !source.is_file() {
            eprintln!("skipping: no licence on this machine to lend the sandbox");
            return None;
        }
        let data = self.root.join("adguard-cli");
        std::fs::create_dir_all(&data).expect("create sandbox data dir");
        std::fs::copy(&source, data.join("adguard.conf")).expect("lend the licence");

        // Prove it took, rather than assuming the copy was sufficient.
        if self.cli.license().is_err() {
            eprintln!("skipping: the sandbox did not come up licensed");
            return None;
        }
        Some(self)
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

    // `license` is here beside `status` because it is the one the activation
    // flow rests on: it is itself licence-gated, which is precisely why nothing
    // can poll it while waiting for an activation to land (contract §7).
    for (command, err) in [
        ("status", sandbox.cli.status().err()),
        ("license", sandbox.cli.license().err()),
    ] {
        let err = err.unwrap_or_else(|| panic!("`{command}` should be refused: a sandbox is unlicensed"));
        eprintln!("{command} in an unlicensed install -> {err:?}");
        assert!(
            matches!(err, adguard_core::Error::Unlicensed { .. }),
            "expected Unlicensed from `{command}`, got {err:?}"
        );
        assert!(
            !err.to_string().contains("rejected"),
            "must not read as our own malformed command line: {err}"
        );
    }

    // The config family is what still works there, which is the premise the
    // rest of this suite rests on.
    sandbox
        .cli
        .config_set(key::LOG_LEVEL, "info")
        .expect("the config family is not licence-gated");
}

/// The first half of licence activation, against the only kind of install where
/// it means anything.
///
/// Everything up to the browser log-in is provable here. The half that is not
/// is the success leg: it needs a real account, and completing an activation
/// spends a device slot, so it ships as a stated claim rather than a
/// measurement (`handoff.md` §3).
///
/// Nothing is consumed by this test. `activate` hands back a log-in link and
/// stops; no login happens, and the link belongs to a data directory that is
/// deleted when the test ends.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn activate_offers_a_login_link_in_an_unlicensed_install() {
    let Some(sandbox) = Sandbox::new("activate") else { return };

    let activation = sandbox.cli.activate().expect("activate should not fail");
    eprintln!("activate in an unlicensed install -> {activation:?}");

    let adguard_core::Activation::NeedsLogin { url } = activation else {
        panic!("expected a log-in link, got {activation:?}");
    };
    // The scheme is the part that must not come from parsed text: this string
    // is handed to `gtk::UriLauncher`, which hands it to the desktop's handler
    // for whatever scheme it names.
    assert!(url.starts_with("https://"), "{url}");
    // Cut short at the first `&`, this would be a link that could not say which
    // install is asking.
    assert!(url.contains("appid="), "the link does not identify this install: {url}");

    // The measured claim the *finish* button rests on: running `activate` again
    // asks after the same pending activation rather than starting a new one, so
    // a user who logs in and comes back is answering the question they were
    // asked. A second link here would make the whole flow unwinnable.
    let again = sandbox.cli.activate().expect("activate should not fail");
    assert_eq!(
        again,
        adguard_core::Activation::NeedsLogin { url },
        "the log-in link moved between invocations"
    );
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

/// The measurement the first-run assistant is built on.
///
/// `architecture.md` §5 described that assistant as "discrete `config set`
/// calls". Against a directory that has never been configured, there is no such
/// thing: every real key is refused, because there is no file to write into.
/// Only `log_level` and `update_channel` are accepted, and they go somewhere
/// else entirely.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_virgin_directory_refuses_every_real_key() {
    let Some(sandbox) = Sandbox::virgin("virgin") else { return };

    // Any invocation creates the data directory and its databases — but not the
    // config. That absence is the first-run signal the GUI keys off.
    let _ = sandbox.cli.config_set(key::LOG_LEVEL, "info");
    assert!(
        !sandbox.config_path().is_file(),
        "nothing but `configure` should create proxy.yaml"
    );

    // Type-appropriate values throughout, because the CLI type-checks *before*
    // it notices the missing file: `config set listen_ports.http_proxy true`
    // answers "the value of the setting must be an integer" even here, so it
    // evidently knows every key's type from a built-in default rather than from
    // the config. Testing with the wrong type would pass for the wrong reason.
    for (real_key, value) in [
        (key::HTTPS_FILTERING, "true"),
        (key::LISTEN_PORT_HTTP, "3128"),
        (key::PROXY_MODE, "manual"),
    ] {
        let refusal = sandbox
            .cli
            .config_set(real_key, value)
            .expect_err("a real key must be refused before the config exists");
        assert!(
            refusal.to_string().contains("No configuration YAML file"),
            "{real_key} was refused, but not for the reason we depend on: {refusal}"
        );
    }

    // The trap that makes this worth a test rather than a comment: the two keys
    // that *are* accepted report `Config has been updated` and persist into
    // `adguard.conf`, so the confirmation is perfectly truthful about a file
    // that is not the one anything reads.
    sandbox
        .cli
        .config_set(key::LOG_LEVEL, "debug")
        .expect("log_level is accepted with no config file");
    assert!(
        !sandbox.config_path().is_file(),
        "`Config has been updated` for log_level must not be read as a config file appearing"
    );
}

/// The guard that stands between `configure` and a user's whole configuration.
///
/// Run against a directory that already holds a `proxy.yaml`, the wizard takes
/// its reconfigure branch — *"the configuration will be reset"* — and with stdin
/// closed there is no prompt at which to decline. [`Cli::configure`] therefore
/// refuses before spawning anything, and this asserts it refuses for that reason
/// rather than incidentally.
///
/// Deliberately **not** `#[ignore]`d beyond the usual: it spawns no process at
/// all, so the only thing it needs is the binary to be locatable.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn configure_refuses_a_directory_that_already_has_a_config() {
    let Some(sandbox) = Sandbox::new("already-configured") else { return };
    let before = std::fs::read_to_string(sandbox.config_path()).expect("read sandbox config");

    let refusal = sandbox
        .cli
        .configure()
        .expect_err("configure must refuse an existing configuration");

    assert!(
        matches!(refusal, adguard_core::Error::AlreadyConfigured { .. }),
        "expected the guard, got {refusal:?}"
    );
    assert_eq!(
        before,
        std::fs::read_to_string(sandbox.config_path()).expect("read sandbox config"),
        "the guard let something touch the file"
    );
}

/// `configure` seeds a complete configuration, and every question the assistant
/// asks can be answered from it.
///
/// Licence-gated, so it needs [`Sandbox::licensed`] — which is the whole reason
/// that helper exists. Measured shape: exit 0, a 220-line file with all 105 of
/// its upstream comments, and ordinary `config set` working immediately after.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn configure_seeds_a_config_the_assistant_can_work_from() {
    let Some(sandbox) = Sandbox::virgin("seed").and_then(Sandbox::licensed) else {
        return;
    };

    sandbox.cli.configure().expect("configure should seed a fresh directory");
    assert!(sandbox.config_path().is_file(), "configure must create proxy.yaml");

    let text = std::fs::read_to_string(sandbox.config_path()).expect("read seeded config");
    let comments = text.lines().filter(|line| line.trim_start().starts_with('#')).count();
    assert!(
        comments > 90,
        "the seeded file should carry AdGuard's own documentation, found {comments} comments"
    );

    // Every question the first-run assistant asks must be answerable from the
    // file it pre-fills from. A key that reads `None` here would render as a
    // control with no honest default behind it.
    let config = sandbox.config();
    for group in &adguard_core::SETUP {
        for setting in group.settings {
            let present = config.resolves(*setting);
            assert!(present, "{} is missing from a freshly seeded config", setting.key);
        }
    }

    // And the second movement works: once seeded, the ordinary write path the
    // rest of the app uses is open.
    sandbox.set(key::LISTEN_PORT_HTTP, "3128");
    assert_eq!(sandbox.config().int_at(key::LISTEN_PORT_HTTP), Some(3128));

    // The guard holds against the directory it just seeded.
    assert!(
        matches!(
            sandbox.cli.configure(),
            Err(adguard_core::Error::AlreadyConfigured { .. })
        ),
        "configure must refuse the configuration it has just created"
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

/// The list-write half of the surgical-write claim.
///
/// `advanced_writes_disturb_one_line_each_and_keep_every_comment` asserts the
/// line count does **not** move, which is the wrong shape for a sequence: a
/// `list-add` is supposed to add one. What has to hold is that it adds exactly
/// one and leaves every comment alone, because the whole never-write-YAML rule
/// rests on these commands being as surgical as `config set`.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_list_add_adds_exactly_one_line_and_keeps_every_comment() {
    let Some(sandbox) = Sandbox::new("list-surgical") else { return };
    let path = sandbox.config_path();
    let comments = |text: &str| {
        text.lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .count()
    };

    let before = std::fs::read_to_string(&path).expect("read sandbox config");
    sandbox
        .cli
        .list_add(key::DNS_FILTERS, "adguard-ui-probe.txt")
        .expect("list-add should be accepted");
    let after = std::fs::read_to_string(&path).expect("read sandbox config");

    assert_eq!(
        after.lines().count(),
        before.lines().count() + 1,
        "a list-add should add exactly one line"
    );
    assert_eq!(
        comments(&after),
        comments(&before),
        "a list-add lost comment lines"
    );
    assert_eq!(
        sandbox.config().lists(key::DNS_FILTERS, "adguard-ui-probe.txt"),
        Some(true)
    );
}

/// Measured, and the reason the DNS user-rules toggle reads before it writes:
/// adding a value the list already holds appends it a **second time** and
/// reports success like any other write. A toggle driven off a stale read would
/// corrupt the list rather than no-opping, and nothing in the output says so.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn list_add_does_not_deduplicate() {
    let Some(sandbox) = Sandbox::new("list-dup") else { return };

    sandbox
        .cli
        .list_add(key::DNS_FILTERS, "dupe.txt")
        .expect("first add");
    sandbox
        .cli
        .list_add(key::DNS_FILTERS, "dupe.txt")
        .expect("second add is accepted too — that is the point");

    let config = sandbox.config();
    let entries = config.list_at(key::DNS_FILTERS).expect("the list should read");
    let count = entries.iter().filter(|entry| **entry == "dupe.txt").count();
    assert_eq!(count, 2, "expected the duplicate the CLI happily writes, got {entries:?}");
}

/// The other direction is safe to issue speculatively: removing something that
/// is not there changes nothing and still succeeds.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn list_remove_of_an_absent_value_is_a_silent_success() {
    let Some(sandbox) = Sandbox::new("list-absent") else { return };
    let path = sandbox.config_path();
    let before = std::fs::read_to_string(&path).expect("read sandbox config");

    sandbox
        .cli
        .list_remove(key::DNS_FILTERS, "was-never-there.txt")
        .expect("removing an absent value should still be accepted");

    assert_eq!(
        before,
        std::fs::read_to_string(&path).expect("read sandbox config"),
        "removing an absent value changed the file"
    );
}

/// Emptying a list writes `filters: []`, which reads back cleanly — despite an
/// echo that prints a bare `filters:` and looks like a null. This asserts the
/// bytes rather than the message, which is the whole point: an earlier version
/// of this suite recorded the echo and got the claim backwards.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn emptying_a_list_leaves_a_readable_empty_list() {
    let Some(sandbox) = Sandbox::new("list-empty") else { return };

    let seeded = sandbox.config();
    let Some(entries) = seeded.list_at(key::DNS_FILTERS) else {
        eprintln!("skipping: the seed config has no readable dns_filtering.filters");
        return;
    };
    let entries: Vec<String> = entries.iter().map(|entry| (*entry).to_owned()).collect();

    for entry in &entries {
        sandbox
            .cli
            .list_remove(key::DNS_FILTERS, entry)
            .unwrap_or_else(|err| panic!("list-remove {entry} refused: {err}"));
    }

    let text = std::fs::read_to_string(sandbox.config_path()).expect("read sandbox config");
    assert!(
        text.contains("filters: []"),
        "an emptied list should be written as `[]`; the file says: {:?}",
        text.lines().find(|line| line.contains("filters:")),
    );

    let config = sandbox.config();
    assert_eq!(
        config.list_at(key::DNS_FILTERS).as_deref(),
        Some(&[][..]),
        "an emptied list must still read as a list"
    );
    assert_eq!(
        config.lists(key::DNS_FILTERS, "dns_user.txt"),
        Some(false),
        "an emptied list must read as empty, never as unreadable"
    );
}

/// The three DNS server settings look like lists in the file's own comments
/// ("space-separated list of DNS servers") and are not. `list-add` refuses them
/// and names the remedy, which is how their class was settled — and this test
/// is what catches the day upstream turns one of them into a real sequence.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_dns_server_settings_are_scalars_not_lists() {
    let Some(sandbox) = Sandbox::new("dns-scalars") else { return };

    for scalar in [key::DNS_UPSTREAM, key::DNS_FALLBACKS, key::DNS_BOOTSTRAPS] {
        let err = sandbox
            .cli
            .list_add(scalar, "1.1.1.1")
            .expect_err("list-add on a scalar key must be refused");
        let message = err.to_string();
        assert!(
            message.contains("not a list setting"),
            "{scalar}: unexpected refusal wording: {message}"
        );
    }

    // And the write that does work is the ordinary one, space-separated value
    // and all.
    sandbox.set(key::DNS_FALLBACKS, "default 1.1.1.1");
    assert_eq!(
        sandbox.config().str_at(key::DNS_FALLBACKS),
        Some("default 1.1.1.1")
    );
}

/// Unlike most string settings these three are validated, so the CLI's own
/// sentence is the one worth showing rather than a weaker rule of ours.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn an_empty_dns_server_value_is_refused_with_the_valid_values() {
    let Some(sandbox) = Sandbox::new("dns-empty") else { return };

    let err = sandbox
        .cli
        .config_set(key::DNS_BOOTSTRAPS, "")
        .expect_err("an empty bootstraps value must be refused");
    let message = err.to_string();
    assert!(
        message.contains("Invalid value") && message.contains("default"),
        "unexpected refusal wording: {message}"
    );
    assert_eq!(
        sandbox.config().str_at(key::DNS_BOOTSTRAPS),
        Some("default"),
        "a refused write must leave the value alone"
    );
}

/// `config set` type-checks and never range-checks — this key included. The
/// bound lives in `DnsListenPort`, and a value outside it must read as
/// unavailable rather than as one of the three states.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_cli_does_not_range_check_the_dns_listen_port() {
    let Some(sandbox) = Sandbox::new("dns-port-range") else { return };

    sandbox.set(key::DNS_LISTEN_PORT, "70000");
    assert_eq!(sandbox.config().int_at(key::DNS_LISTEN_PORT), Some(70000));
    assert_eq!(
        sandbox.config().dns_listen_port(),
        None,
        "a port no listener could use must not render as one of the three states"
    );

    for (written, expected) in [("-1", -1), ("0", 0), ("5353", 5353)] {
        sandbox.set(key::DNS_LISTEN_PORT, written);
        assert_eq!(sandbox.config().int_at(key::DNS_LISTEN_PORT), Some(expected));
        assert!(sandbox.config().dns_listen_port().is_some(), "{written} should read");
    }
}
