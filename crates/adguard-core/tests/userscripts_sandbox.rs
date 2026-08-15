//! `userscripts` against a **throwaway** data directory.
//!
//! ```text
//! cargo test -p adguard-core --test userscripts_sandbox -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d like every suite that shells out — it needs `adguard-cli`, and
//! a licence. It never touches the machine's own install, and
//! [`the_machine_install_was_not_touched`] asserts that rather than leaving it
//! to be believed.
//!
//! # Why this one serves HTTP where `filters_sandbox.rs` serves files
//!
//! The two commands do not accept the same things, which is measured rather
//! than assumed (contract §15). `filters install` takes a local path through
//! the same positional as a URL, so that suite stays hermetic by pointing at a
//! file it wrote. `userscripts install` **refuses a path and a `file://` URL
//! alike** — both answer `Failed to install userscript` — and accepts only
//! http(s).
//!
//! So hermeticity here means a loopback HTTP server rather than no server at
//! all: [`Server`] binds `127.0.0.1:0`, hands out its ephemeral port, and
//! serves bodies this file wrote. Nothing reaches the network, nothing depends
//! on a third party's script staying up, and [`a_local_path_and_a_file_url_are_refused`]
//! pins the boundary that forces the arrangement.
//!
//! Userscripts are generated inline rather than kept in `tests/fixtures/`,
//! where the zips live: they are three lines of text, and
//! [`reinstalling_updates_in_place_and_re_enables`] needs the *same* script at
//! two different `@version`s, which a file on disk cannot be.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use adguard_core::{userscripts, Cli, Config, Error, Locale, Userscript};

/// A scratch `$XDG_DATA_HOME` seeded from the machine's install.
///
/// Unlike `filters_sandbox.rs` this needs `proxy.yaml` **and** the
/// `userscripts/` directory: enabled state lives in the config and the scripts
/// themselves live in the directory, so a sandbox missing either would be
/// testing a state no install is ever in.
struct Sandbox {
    root: PathBuf,
    cli: Cli,
}

impl Sandbox {
    /// `None` — with a printed reason — whenever the machine cannot host the
    /// test, so this skips rather than fails on a box without AdGuard.
    fn new(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };

        // One directory per test: the tests in this binary run concurrently,
        // and two `adguard-cli` invocations racing each other's initialisation
        // of the *same* fresh directory is a measured failure (contract §3).
        let root = std::env::temp_dir().join(format!(
            "adguard-ui-userscripts-{name}-{}",
            std::process::id()
        ));
        let data = root.join("adguard-cli");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&data).expect("create sandbox");

        let real = adguard_core::paths::data_dir()?;
        let licence = real.join("adguard.conf");
        if !licence.is_file() {
            eprintln!("skipping: no licence on this machine to lend the sandbox");
            return None;
        }
        std::fs::copy(&licence, data.join("adguard.conf")).expect("lend the licence");

        // `config set` refuses every real key until this exists (contract §5),
        // and `userscripts enable` writes into it.
        std::fs::copy(real.join("proxy.yaml"), data.join("proxy.yaml")).expect("seed proxy.yaml");

        // The scripts themselves, so the sandbox starts where a real install
        // does: AdGuard Extra present and switched on.
        let scripts = data.join("userscripts");
        std::fs::create_dir_all(&scripts).expect("create userscripts dir");
        if let Ok(entries) = std::fs::read_dir(real.join("userscripts")) {
            for entry in entries.flatten() {
                let _ = std::fs::copy(entry.path(), scripts.join(entry.file_name()));
            }
        }

        let sandbox = Self {
            cli: cli.with_xdg_data_home(&root),
            root,
        };

        if sandbox.cli.license().is_err() {
            eprintln!("skipping: the sandbox did not come up licensed");
            return None;
        }
        Some(sandbox)
    }

    fn data(&self) -> PathBuf {
        self.root.join("adguard-cli")
    }

    fn scripts_dir(&self) -> PathBuf {
        adguard_core::paths::userscripts_dir_under(&self.root)
    }

    /// Everything the Extensions page would render, read the way it reads it.
    ///
    /// By explicit path rather than through `Config::load`, for the reason
    /// `filters_sandbox.rs` gives about `Catalogue::open_set`: the sandbox sets
    /// `$XDG_DATA_HOME` on the *child* only, so anything resolving it from this
    /// process's environment would read the machine's own install and these
    /// tests would assert against it.
    fn read(&self) -> Vec<Userscript> {
        let config = Config::read(&adguard_core::paths::config_file_under(&self.root))
            .expect("sandbox proxy.yaml should read");
        let enabled = config.enabled_userscripts();
        userscripts::read(&self.scripts_dir(), &enabled, &Locale::english())
    }

    fn find(&self, id: &str) -> Option<Userscript> {
        self.read().into_iter().find(|script| script.id == id)
    }

    fn pair_exists(&self, id: &str) -> (bool, bool) {
        let dir = self.scripts_dir();
        (
            dir.join(format!("{id}.meta.json")).is_file(),
            dir.join(format!("{id}.user.js")).is_file(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The `@homepage` every generated script carries, so the row's link has a
/// value to assert against.
const HOMEPAGE: &str = "https://example.org/probe";

/// A userscript with the metadata block AdGuard reads.
fn script(name: &str, version: &str) -> String {
    format!(
        "// ==UserScript==\n\
         // @name         {name}\n\
         // @namespace    adguard-ui-tests\n\
         // @version      {version}\n\
         // @description  Written by userscripts_sandbox.rs\n\
         // @homepage     {HOMEPAGE}\n\
         // @match        *://example.org/*\n\
         // ==/UserScript==\n\
         (function () {{ /* nothing */ }})();\n"
    )
}

/// A loopback HTTP server, because `userscripts install` takes nothing else.
///
/// Hand-rolled over `TcpListener` rather than taking a dependency: the whole
/// protocol needed is one request line to route on and one response with a
/// `Content-Length`. Routes are mutable so that the same URL can serve two
/// different versions of a script, which is what the reinstall case needs.
struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("non-blocking");

        let routes: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread = std::thread::spawn({
            let routes = routes.clone();
            let shutdown = shutdown.clone();
            move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &routes),
                        // Non-blocking: nothing waiting is the ordinary case.
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        Self {
            port,
            routes,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Publish `body` at `path`, replacing whatever was there.
    fn serve(&self, path: &str, body: String) {
        self.routes
            .lock()
            .expect("routes")
            .insert(path.to_owned(), body);
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One request: route on the path, answer 200 or 404, and close.
///
/// `Connection: close` with a `Content-Length` so the client is never left
/// waiting on a body that has already arrived — a hang here would surface as
/// the CLI's own 60-second deadline and read as a broken test rather than a
/// broken server.
fn serve(mut stream: TcpStream, routes: &Arc<Mutex<HashMap<String, String>>>) {
    let mut buffer = [0_u8; 4096];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();

    let body = routes.lock().expect("routes").get(&path).cloned();
    let response = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        None => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        }
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// The switch, decided by re-reading rather than by what the CLI printed.
///
/// Enabled state is presence in `proxy.yaml`'s list, so this is also the test
/// that the read path and the write path agree about where that state lives.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn enabling_and_disabling_moves_the_config() {
    let Some(sandbox) = Sandbox::new("toggle") else {
        return;
    };

    let before = sandbox.find("adguard-extra").expect("AdGuard Extra ships installed");
    assert!(before.enabled, "a stock install has it switched on");

    sandbox.cli.userscripts_disable("adguard-extra").expect("disable");
    assert!(
        !sandbox.find("adguard-extra").expect("still installed").enabled,
        "the re-read must show it off"
    );

    sandbox.cli.userscripts_enable("adguard-extra").expect("enable");
    assert!(
        sandbox.find("adguard-extra").expect("still installed").enabled,
        "and back on again"
    );
}

/// Disabling is not removing, and the difference is the files.
///
/// The distinction the whole page rests on: a disabled script is still
/// installed and still a row. A reader that keyed on the directory alone would
/// call this uninstalled; one that keyed on the config alone would not see it
/// at all.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn disabling_keeps_the_files_and_removing_deletes_them() {
    let Some(sandbox) = Sandbox::new("remove") else {
        return;
    };

    sandbox.cli.userscripts_disable("adguard-extra").expect("disable");
    assert_eq!(
        sandbox.pair_exists("adguard-extra"),
        (true, true),
        "disable leaves both files on disk"
    );
    assert!(
        sandbox.find("adguard-extra").is_some(),
        "and it is still a row on the page"
    );

    sandbox.cli.userscripts_remove("adguard-extra").expect("remove");
    assert_eq!(
        sandbox.pair_exists("adguard-extra"),
        (false, false),
        "remove deletes the metadata and the script"
    );
    assert!(sandbox.find("adguard-extra").is_none(), "and the row is gone");
}

/// Installing over loopback, verified by the pair landing and the entry
/// appearing — never by the confirmation.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn installing_from_a_url_adds_the_pair_and_switches_it_on() {
    let Some(sandbox) = Sandbox::new("install") else {
        return;
    };
    let server = Server::start();
    server.serve("/probe.user.js", script("Sandbox Probe", "1.2.3"));

    sandbox
        .cli
        .userscripts_install(&server.url("/probe.user.js"))
        .expect("a served userscript installs");

    let installed = sandbox.find("probe").expect("the id is the filename stem");
    assert_eq!(installed.name, "Sandbox Probe");
    assert_eq!(installed.version.as_deref(), Some("1.2.3"), "#9 asks for this");
    assert_eq!(installed.homepage.as_deref(), Some(HOMEPAGE));
    assert!(installed.enabled, "the confirmation says 'installed and enabled'");
    assert_eq!(sandbox.pair_exists("probe"), (true, true));
    assert!(
        installed.download_url.is_some(),
        "the URL is recorded, which is what makes a reinstall possible"
    );
}

/// The trap: an id contained in another script's id cannot be named at all,
/// even when it is passed exactly.
///
/// The measurement `Userscript::ambiguous` predicts and
/// `Error::AmbiguousUserscript` reports. Both ends are asserted here, because
/// a prediction that disagreed with the CLI would be worse than no prediction:
/// it would grey out a working control, or offer a broken one.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_colliding_id_cannot_be_named() {
    let Some(sandbox) = Sandbox::new("collide") else {
        return;
    };
    let server = Server::start();
    server.serve("/hello.user.js", script("Hello Sandbox", "1.0"));
    server.serve("/hello-world.user.js", script("Hello World", "1.0"));

    sandbox.cli.userscripts_install(&server.url("/hello.user.js")).expect("install hello");
    sandbox
        .cli
        .userscripts_install(&server.url("/hello-world.user.js"))
        .expect("install hello-world");

    // The prediction, from the files alone.
    let hello = sandbox.find("hello").expect("hello is installed");
    let world = sandbox.find("hello-world").expect("hello-world is installed");
    assert!(hello.ambiguous, "`hello` is inside `hello-world`");
    assert!(!hello.actionable(), "so the page must not offer its controls");
    assert!(!world.ambiguous, "nothing contains `hello-world`");

    // The CLI, agreeing — with the exact id, which is the part that surprises.
    match sandbox.cli.userscripts_disable("hello") {
        Err(Error::AmbiguousUserscript { name, candidates }) => {
            assert_eq!(name, "hello");
            assert_eq!(candidates.len(), 2, "both scripts were named: {candidates:?}");
        }
        other => panic!("expected an ambiguity refusal, got {other:?}"),
    }

    // And it changed nothing, so there is nothing to undo.
    assert!(
        sandbox.find("hello").expect("still there").enabled,
        "the refused disable left it switched on"
    );

    // Removing the collision makes the other reachable again — the condition is
    // about the pair, not about the script.
    sandbox.cli.userscripts_remove("hello-world").expect("remove the collision");
    assert!(
        !sandbox.find("hello").expect("still there").ambiguous,
        "with nothing to collide with, `hello` is nameable again"
    );
    sandbox.cli.userscripts_disable("hello").expect("and now it can be switched");
}

/// Re-installing is the update path — and it turns a disabled script back on.
///
/// The second half is why *Reinstall* has to disclose what it does: a user who
/// switched a script off did not ask for updating it to start it running again.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn reinstalling_updates_in_place_and_re_enables() {
    let Some(sandbox) = Sandbox::new("reinstall") else {
        return;
    };
    let server = Server::start();
    server.serve("/probe.user.js", script("Sandbox Probe", "0.2.1"));
    sandbox.cli.userscripts_install(&server.url("/probe.user.js")).expect("install");
    assert_eq!(sandbox.find("probe").expect("installed").version.as_deref(), Some("0.2.1"));

    sandbox.cli.userscripts_disable("probe").expect("disable");
    assert!(!sandbox.find("probe").expect("installed").enabled);

    // The same URL, a new version behind it.
    server.serve("/probe.user.js", script("Sandbox Probe", "0.9.9"));
    sandbox.cli.userscripts_install(&server.url("/probe.user.js")).expect("reinstall");

    let after = sandbox.find("probe").expect("still installed");
    assert_eq!(after.version.as_deref(), Some("0.9.9"), "updated in place");
    assert!(
        after.enabled,
        "and silently re-enabled — the measurement the Reinstall row must disclose"
    );
}

/// A 404 and a body that is not a userscript are both refused, and neither is
/// distinguishable from the other.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_missing_url_and_a_non_userscript_are_both_refused() {
    let Some(sandbox) = Sandbox::new("badbody") else {
        return;
    };
    let server = Server::start();
    server.serve("/plain.txt", "this is not a userscript\n".to_owned());

    for url in [server.url("/nothing-here.user.js"), server.url("/plain.txt")] {
        match sandbox.cli.userscripts_install(&url) {
            Err(Error::Refused { message }) => assert_eq!(
                message, "Failed to install userscript",
                "one sentence covers every failure"
            ),
            other => panic!("{url} should have been refused, got {other:?}"),
        }
    }
    assert_eq!(sandbox.read().len(), 1, "only AdGuard Extra is installed");
}

/// The boundary that forces this suite to run a server: a local file is not an
/// acceptable source, where for `filters install` it is.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_local_path_and_a_file_url_are_refused() {
    let Some(sandbox) = Sandbox::new("localfile") else {
        return;
    };
    let path = sandbox.root.join("local.user.js");
    std::fs::write(&path, script("Local Probe", "1.0")).expect("write");

    for source in [
        path.display().to_string(),
        format!("file://{}", path.display()),
    ] {
        match sandbox.cli.userscripts_install(&source) {
            Err(Error::Refused { message }) => {
                assert_eq!(message, "Failed to install userscript", "{source}")
            }
            other => panic!("{source} should have been refused, got {other:?}"),
        }
    }
    assert!(sandbox.find("local").is_none(), "nothing was installed");
}

/// A blank name never reaches the CLI, where it would be a wildcard.
///
/// Covered by a unit test against the guard as well; this is the same claim
/// made against the real binary, since the consequence of being wrong — the
/// user's only userscript switched off by a name that names nothing — is worth
/// pinning at both levels.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_blank_name_is_refused_and_changes_nothing() {
    let Some(sandbox) = Sandbox::new("blank") else {
        return;
    };
    assert!(matches!(
        sandbox.cli.userscripts_disable(""),
        Err(Error::UnnamedUserscript)
    ));
    assert!(
        sandbox.find("adguard-extra").expect("installed").enabled,
        "the one script on the install is untouched"
    );
}

/// AdGuard's four bundled scripts install from the URLs the catalogue carries,
/// and land in the state its Windows and Mac applications ship them in.
///
/// **The one test here that reaches the network**, and the only one in this
/// repository that does by choice — `RECOMMENDED` names four addresses on
/// AdGuard's own CDN, and a table of URLs nobody fetches is a table of guesses.
/// It is what would catch AdGuard retiring a path, which is the failure the
/// catalogue cannot survive and cannot detect any other way.
///
/// It also pins the thing a catalogue offering four scripts together may not
/// assume: that none of them collides with another by substring, leaving one
/// installed and unswitchable (contract §15).
#[test]
#[ignore = "invokes the real adguard-cli and reaches AdGuard's CDN"]
fn the_recommended_scripts_install_in_the_state_adguard_ships_them() {
    let Some(sandbox) = Sandbox::new("recommended") else {
        return;
    };

    for entry in &adguard_core::RECOMMENDED {
        // AdGuard Extra is already installed on a seeded sandbox; installing it
        // again is the reinstall path and is just as valid a check of the URL.
        sandbox
            .cli
            .userscripts_install(entry.url)
            .unwrap_or_else(|err| panic!("{} would not install from {}: {err}", entry.name, entry.url));
        if !entry.enabled_by_default {
            sandbox
                .cli
                .userscripts_disable(entry.id)
                .unwrap_or_else(|err| panic!("{} would not switch off: {err}", entry.name));
        }
    }

    let installed = sandbox.read();
    assert_eq!(installed.len(), 4, "expected all four, got {installed:?}");

    for entry in &adguard_core::RECOMMENDED {
        let script = installed
            .iter()
            .find(|s| s.id == entry.id)
            .unwrap_or_else(|| panic!("{} did not land under id {}", entry.name, entry.id));

        assert_eq!(script.name, entry.name, "the catalogue's name matches AdGuard's");
        assert_eq!(
            script.enabled, entry.enabled_by_default,
            "{} should be {} after adding",
            entry.name,
            if entry.enabled_by_default { "on" } else { "off" }
        );
        assert!(
            script.version.is_some(),
            "{} carried no version to render",
            entry.name
        );
        assert!(
            script.actionable(),
            "{} cannot be switched while the other three are installed",
            entry.name
        );
    }

    // With all four present the catalogue is empty, which is what tells the user
    // there is nothing left to add.
    assert!(
        adguard_core::userscripts::recommended(&installed).is_empty(),
        "the catalogue should be empty once all four are installed"
    );
}

/// The machine's own install is never written to.
///
/// `filters_sandbox.rs` asserts the same thing about the catalogue, and for the
/// same reason: a suite that edits the developer's real configuration would be
/// a worse bug than anything it could catch.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_machine_install_was_not_touched() {
    let Some(real) = adguard_core::paths::data_dir() else {
        return;
    };
    let config = real.join("proxy.yaml");
    let scripts = real.join("userscripts");
    if !config.is_file() {
        eprintln!("skipping: no install on this machine");
        return;
    }

    let before = std::fs::read(&config).expect("read the real config");
    let listing = |dir: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        names
    };
    let scripts_before = listing(&scripts);

    {
        let Some(sandbox) = Sandbox::new("isolation") else {
            return;
        };
        let server = Server::start();
        server.serve("/probe.user.js", script("Isolation Probe", "1.0"));
        sandbox.cli.userscripts_install(&server.url("/probe.user.js")).expect("install");
        sandbox.cli.userscripts_disable("adguard-extra").expect("disable");
        sandbox.cli.userscripts_remove("probe").expect("remove");
        assert!(sandbox.data().join("proxy.yaml").is_file(), "the sandbox has its own");
    }

    assert_eq!(
        before,
        std::fs::read(&config).expect("re-read the real config"),
        "the machine's proxy.yaml was modified"
    );
    assert_eq!(
        scripts_before,
        listing(&scripts),
        "the machine's userscripts directory was modified"
    );
}
