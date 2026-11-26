//! What `check-update` does to an installed userscript, against a **throwaway**
//! data directory.
//!
//! ```text
//! cargo test -p adguard-core --test userscripts_update_sandbox -- --ignored --nocapture
//! ```
//!
//! The measurements behind contract §15's *What `check-update` does to a
//! userscript*, kept runnable. They decide how the Extensions page keeps a
//! user's edits: whether an update can arrive behind its back, which address
//! it comes from, and which scripts it skips.
//!
//! # This one reaches the network
//!
//! `check-update` refreshes filters, Safe Browsing and CRLite as well as
//! userscripts, and nothing can ask it for userscripts alone — so every run here
//! is also a real fetch from AdGuard's servers, as `update_sandbox.rs` is. The
//! probe scripts themselves are served from loopback, by the same hand-rolled
//! server `userscripts_sandbox.rs` uses and for its reason: `userscripts
//! install` takes http(s) and nothing else.
//!
//! **The sandbox starts with no script files**, only the machine's licence and
//! `proxy.yaml`. AdGuard seeds AdGuard Extra into an empty directory on its own,
//! and every other entry in the copied config names a file that is not there
//! and is ignored. Copying the real scripts instead would put every one of them
//! on the network in every run — and one of them lacking an update address
//! would crash every test here, which is the last finding below.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use adguard_core::{userscripts, Cli, Config, Locale, Userscript};

struct Sandbox {
    root: PathBuf,
    cli: Cli,
}

impl Sandbox {
    /// `None`, with a reason, whenever the machine cannot host the test.
    fn new(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };
        // One directory per test: they run concurrently (contract §3).
        let root = std::env::temp_dir().join(format!(
            "adguard-ui-userscripts-update-{name}-{}",
            std::process::id()
        ));
        let data = root.join("adguard-cli");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(data.join("userscripts")).expect("create sandbox");

        let real = adguard_core::paths::data_dir()?;
        if !real.join("adguard.conf").is_file() {
            eprintln!("skipping: no licence on this machine to lend the sandbox");
            return None;
        }
        std::fs::copy(real.join("adguard.conf"), data.join("adguard.conf")).expect("lend the licence");
        std::fs::copy(real.join("proxy.yaml"), data.join("proxy.yaml")).expect("seed proxy.yaml");

        Some(Self {
            cli: cli.with_xdg_data_home(&root),
            root,
        })
    }

    fn scripts_dir(&self) -> PathBuf {
        adguard_core::paths::userscripts_dir_under(&self.root)
    }

    /// By explicit path, never through this process's `$XDG_DATA_HOME`, which
    /// is the machine's own install (see `userscripts_sandbox.rs`).
    fn find(&self, id: &str) -> Userscript {
        let config = Config::read(&adguard_core::paths::config_file_under(&self.root))
            .expect("sandbox proxy.yaml should read");
        let enabled = config.enabled_userscripts();
        userscripts::read(&self.scripts_dir(), &enabled, &Locale::english())
            .into_iter()
            .find(|script| script.id == id)
            .unwrap_or_else(|| panic!("{id} should be installed"))
    }

    fn source(&self, id: &str) -> String {
        std::fs::read_to_string(self.scripts_dir().join(format!("{id}.user.js"))).expect("read the script")
    }

    /// Whether the metadata file records an empty `updateURL`. The one field
    /// this suite needs that `Userscript` does not carry; read as text, since
    /// the shape is AdGuard's own and fixed (contract §15).
    fn update_url_is_empty(&self, id: &str) -> bool {
        std::fs::read_to_string(self.scripts_dir().join(format!("{id}.meta.json")))
            .is_ok_and(|text| text.contains("\"updateURL\": \"\"") || text.contains("\"updateURL\":\"\""))
    }

    /// Append a line to the installed copy, the way an editor would save it.
    fn edit(&self, id: &str) {
        let path = self.scripts_dir().join(format!("{id}.user.js"));
        let mut text = std::fs::read_to_string(&path).expect("read the script");
        text.push_str(EDIT);
        std::fs::write(&path, text).expect("write the edit");
    }

    /// `check-update`, by its raw exit status as well as by what the wrapper
    /// made of it: the crash below is only visible in the first.
    fn check_update(&self) -> (Option<i32>, Result<adguard_core::UpdateReport, adguard_core::Error>) {
        let status = std::process::Command::new(self.cli.binary())
            .arg("check-update")
            .env("XDG_DATA_HOME", &self.root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run check-update");
        (status.code(), self.cli.check_update())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The line every edit appends, to be looked for afterwards.
const EDIT: &str = "// EDITED-BY-THE-USER\n";

/// A probe script. `extra` is any further metadata lines, already formatted.
fn script(name: &str, version: &str, extra: &str) -> String {
    format!(
        "// ==UserScript==\n\
         // @name         {name}\n\
         // @namespace    adguard-ui-tests\n\
         // @version      {version}\n\
         // @description  Written by userscripts_update_sandbox.rs\n\
         // @match        *://example.org/*\n\
         {extra}\
         // ==/UserScript==\n\
         (function () {{ /* {version} */ }})();\n"
    )
}

/// The metadata block alone, as a `.meta.js` serves it.
fn meta(source: &str) -> String {
    let end = source.find("// ==/UserScript==").expect("a block") + "// ==/UserScript==\n".len();
    source[..end].to_owned()
}

/// A loopback server whose routes can change between requests, and which
/// records every path asked of it — the question here is often *which* address
/// AdGuard fetched.
struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, String>>>,
    asked: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("non-blocking");
        let routes: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (routes, asked, shutdown) = (routes.clone(), asked.clone(), shutdown.clone());
            move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &routes, &asked),
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            }
        });
        Self { port, routes, asked, shutdown, thread: Some(thread) }
    }

    fn serve(&self, path: &str, body: String) {
        self.routes.lock().expect("routes").insert(path.to_owned(), body);
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every path requested since the last call.
    fn take_asked(&self) -> Vec<String> {
        std::mem::take(&mut *self.asked.lock().expect("asked"))
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

fn serve(mut stream: TcpStream, routes: &Mutex<HashMap<String, String>>, asked: &Mutex<Vec<String>>) {
    // Accepted sockets inherit nothing from a non-blocking listener on Linux,
    // but say so rather than rely on it: a non-blocking read here would see
    // nothing and answer 404 to a request still in flight.
    let _ = stream.set_nonblocking(false);
    let mut buffer = [0_u8; 4096];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    asked.lock().expect("asked").push(path.clone());
    let response = match routes.lock().expect("routes").get(&path) {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Publish `name` at `version` under `/<file>.user.js` and `/<file>.meta.js`,
/// with `extra` metadata lines, and return the script.
fn publish(server: &Server, file: &str, name: &str, version: &str, extra: &str) -> String {
    let source = script(name, version, extra);
    server.serve(&format!("/{file}.user.js"), source.clone());
    server.serve(&format!("/{file}.meta.js"), meta(&source));
    source
}

/// With an `updateURL`, AdGuard polls it and installs nothing while the version
/// there is the same — so an edit survives a run. **Without one, it downloads
/// the whole script on every run and installs it whether or not anything
/// changed**, so an edit is lost at the first `check-update`, and that run
/// reports `1 userscript(s) updated` about a script that did not change.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn an_equal_version_keeps_an_edit_only_with_an_update_url() {
    let Some(sandbox) = Sandbox::new("equal") else {
        return;
    };
    let server = Server::start();
    let polled = format!(
        "// @updateURL    {}\n// @downloadURL  {}\n",
        server.url("/polled.meta.js"),
        server.url("/polled.user.js")
    );
    let refetched = format!("// @downloadURL  {}\n", server.url("/refetched.user.js"));
    publish(&server, "polled", "Probe Polled", "1.0", &polled);
    publish(&server, "refetched", "Probe Refetched", "1.0", &refetched);
    sandbox.cli.userscripts_install(&server.url("/polled.user.js")).expect("install polled");
    sandbox.cli.userscripts_install(&server.url("/refetched.user.js")).expect("install refetched");
    assert!(sandbox.update_url_is_empty("refetched"), "AdGuard records the missing updateURL as empty");

    sandbox.edit("polled");
    sandbox.edit("refetched");
    server.take_asked();
    let (code, _) = sandbox.check_update();
    assert_eq!(code, Some(0));

    let asked = server.take_asked();
    eprintln!("asked during check-update: {asked:?}");
    assert!(sandbox.source("polled").contains(EDIT), "same version at the updateURL: the edit stands");
    assert!(
        !asked.contains(&"/polled.user.js".to_owned()),
        "nothing newer at the updateURL, so the script itself is not fetched"
    );
    assert!(
        !sandbox.source("refetched").contains(EDIT),
        "no updateURL: downloaded and installed again although nothing changed"
    );
    assert!(asked.contains(&"/refetched.user.js".to_owned()));
}

/// A newer version at the `updateURL` replaces the installed copy, edit and
/// all — downloaded from the `downloadURL` **recorded at install**, not from
/// the one the new metadata names. The new one is recorded for next time.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn a_newer_version_replaces_an_edit_from_the_recorded_address() {
    let Some(sandbox) = Sandbox::new("newer") else {
        return;
    };
    let server = Server::start();
    let lines = |download: &str| {
        format!(
            "// @updateURL    {}\n// @downloadURL  {}\n",
            server.url("/moved.meta.js"),
            server.url(download)
        )
    };
    publish(&server, "moved", "Probe Moved", "1.0", &lines("/moved.user.js"));
    sandbox.cli.userscripts_install(&server.url("/moved.user.js")).expect("install");
    sandbox.edit("moved");

    // 1.1, whose metadata now points at a different download address. Both
    // addresses serve 1.1, told apart by a marker line.
    let newer = script("Probe Moved", "1.1", &lines("/elsewhere.user.js"));
    server.serve("/moved.meta.js", meta(&newer));
    server.serve("/moved.user.js", format!("{newer}// FROM-THE-RECORDED-ADDRESS\n"));
    server.serve("/elsewhere.user.js", format!("{newer}// FROM-THE-NEW-ADDRESS\n"));
    server.take_asked();
    let (code, _) = sandbox.check_update();
    assert_eq!(code, Some(0));

    eprintln!("asked during check-update: {:?}", server.take_asked());
    let source = sandbox.source("moved");
    assert!(!source.contains(EDIT), "the edit is gone");
    assert!(source.contains("FROM-THE-RECORDED-ADDRESS"), "fetched where it was installed from");
    let moved = sandbox.find("moved");
    assert_eq!(moved.version.as_deref(), Some("1.1"));
    assert_eq!(
        moved.download_url.as_deref(),
        Some(server.url("/elsewhere.user.js").as_str()),
        "and the new address is recorded for the next update"
    );
}

/// A disabled script is not checked at all: not fetched, not replaced, and not
/// switched back on — unlike a reinstall, which always re-enables. So a script
/// the user switched off goes stale, edits intact, for as long as it stays off.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn a_disabled_script_is_skipped_and_stays_off() {
    let Some(sandbox) = Sandbox::new("disabled") else {
        return;
    };
    let server = Server::start();
    let lines = format!("// @updateURL    {}\n", server.url("/off.meta.js"));
    publish(&server, "off", "Probe Off", "1.0", &lines);
    sandbox.cli.userscripts_install(&server.url("/off.user.js")).expect("install");
    sandbox.edit("off");
    sandbox.cli.userscripts_disable("Probe Off").expect("disable");

    publish(&server, "off", "Probe Off", "2.0", &lines);
    server.take_asked();
    let (code, _) = sandbox.check_update();
    assert_eq!(code, Some(0));

    let asked = server.take_asked();
    assert!(asked.iter().all(|path| !path.starts_with("/off.")), "not even polled: {asked:?}");
    let off = sandbox.find("off");
    assert_eq!(off.version.as_deref(), Some("1.0"));
    assert!(!off.enabled, "still off");
    assert!(sandbox.source("off").contains(EDIT));
}

/// **An enabled script whose source names neither `@updateURL` nor
/// `@downloadURL` crashes `check-update`** — a segfault, exit 139, after
/// AdGuard's log reads `Cannot find version or updateURL for userscript` and
/// `ffurl_parse: : more data`. It is installed from a URL like any other, and
/// AdGuard records that URL as its `downloadURL`; the crash is in the update
/// path all the same. Everything after the userscripts step — Safe Browsing,
/// CRLite, the application — is not checked, and the output stops mid-report.
///
/// Switched off, the same script is skipped (above) and the run completes.
#[test]
#[ignore = "invokes the real adguard-cli and reaches the network"]
fn a_script_naming_no_update_address_crashes_check_update() {
    let Some(sandbox) = Sandbox::new("crash") else {
        return;
    };
    let server = Server::start();
    publish(&server, "bare", "Probe Bare", "1.0", "");
    sandbox.cli.userscripts_install(&server.url("/bare.user.js")).expect("install");
    let bare = sandbox.find("bare");
    assert_eq!(
        bare.download_url.as_deref(),
        Some(server.url("/bare.user.js").as_str()),
        "the install address is recorded, so the metadata file is not what is missing"
    );

    let (code, wrapped) = sandbox.check_update();
    eprintln!("exit {code:?}; the wrapper says {wrapped:?}");
    assert_eq!(code, None, "killed by a signal, not exited");

    sandbox.cli.userscripts_disable("Probe Bare").expect("disable");
    let (code, _) = sandbox.check_update();
    assert_eq!(code, Some(0), "switched off, it is skipped and the run completes");
}
