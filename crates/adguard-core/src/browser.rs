//! Whether AdGuard's browser integration has been installed.
//!
//! AdGuard's browser extension does not look for `adguard-cli` on `$PATH`. It
//! calls `chrome.runtime.connectNative` with a fixed host name, and the browser
//! resolves that name by reading a **native-messaging manifest** out of a
//! directory of its own. With no manifest the connection fails immediately and
//! the extension reports that it cannot detect AdGuard — which is a true
//! statement about the manifest and a misleading one about the machine, because
//! `adguard-cli` may be installed, running and filtering perfectly.
//!
//! The host name is the extension's, read from its `background.js`:
//!
//! ```text
//! const HOST_TYPES = { browserExtensionHost: 'com.adguard.browser_extension_host.nm' };
//! this.port = browser_polyfill_default().runtime.connectNative(HOST_TYPES.browserExtensionHost);
//! ```
//!
//! and the manifests are written by AdGuard's own `adguard-cli
//! install-browser-integration`, which is a **separate step that unpacking the
//! CLI does not perform**. A stock install therefore ships with the extension
//! unable to see it.
//!
//! **The six locations are measured, not guessed.** They are the only
//! native-messaging paths in the CLI binary's strings:
//!
//! ```text
//! .config/BraveSoftware/Brave-Browser/NativeMessagingHosts
//! .config/chromium/NativeMessagingHosts
//! .config/google-chrome/NativeMessagingHosts
//! .config/microsoft-edge/NativeMessagingHosts
//! .config/vivaldi/NativeMessagingHosts
//! .mozilla/native-messaging-hosts
//! ```
//!
//! **And the installer only writes where it already sees a browser.** Measured
//! against a sandbox `$HOME`: with no browser directories present at all, the
//! command prints `Native Messaging manifests installed successfully` and
//! creates **nothing**. Creating `.config/chromium` and re-running writes one
//! manifest, into that browser and no other; Firefox needs `.mozilla/firefox`
//! to exist before it is written to, its own `.mozilla` alone is not enough.
//!
//! That measurement is the whole reason this check has to be repeated rather
//! than performed once. Install a browser *after* running the command and it
//! gets no manifest, and nothing will ever tell the user — the command has
//! already reported success, and the extension's complaint names `adguard-cli`
//! rather than the missing file. The order the user happened to do things in is
//! not something the application can know, so it looks.
//!
//! Nothing here writes, spawns or escalates. It reads directory entries and, at
//! most, six small JSON files.

use std::fs;
use std::path::{Path, PathBuf};

/// The native-messaging host name the extension asks its browser for.
///
/// Public because it is the fact the whole module hangs on, and a reader
/// checking this code against a browser's own diagnostics needs the string.
pub const HOST: &str = "com.adguard.browser_extension_host.nm";

/// The manifest file name, in every one of the six directories.
const MANIFEST: &str = "com.adguard.browser_extension_host.nm.json";

/// The host binary the manifests point at, beside the resolved `adguard-cli`.
pub const HOST_BINARY: &str = "adguard_cli_nm";

/// One browser AdGuard's installer knows about, as found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    /// What to call it in a sentence.
    pub name: &'static str,
    /// The manifest that would make this browser's extension work.
    ///
    /// Kept whether or not anything is there, so a report can name the file it
    /// looked for rather than merely asserting an absence.
    pub manifest: PathBuf,
    pub state: State,
}

/// What the manifest for one browser was found to be.
///
/// Deliberately not a bool. "No manifest" and "a manifest naming a binary that
/// is no longer there" need different sentences and, in the second case, the
/// user is looking at a machine where the command *has* been run — telling them
/// to run it again without saying why would read as the application not having
/// looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// A manifest naming the host binary beside this machine's `adguard-cli`.
    Ready,
    /// No manifest at all: the ordinary never-run state, and the state of any
    /// browser installed after the command was last run.
    Missing,
    /// A manifest naming something else, or something that is no longer there —
    /// an AdGuard CLI that has moved or been reinstalled under another prefix.
    /// The path is the one the manifest names, so the row can show it.
    Stale(PathBuf),
    /// A manifest that exists and could not be read or understood. Different
    /// from every other answer, and never rendered as the reassuring one.
    Unreadable(String),
}

/// The check, across every browser this machine has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserIntegration {
    /// The host binary the manifests ought to name, or `None` when the CLI
    /// itself could not be located.
    pub host_binary: Option<PathBuf>,
    /// Whether that binary is actually there. A manifest pointing at a missing
    /// host is worse than no manifest, and re-running the installer would
    /// produce exactly that, so the command is not offered when this is false.
    pub host_present: bool,
    /// Only the browsers found on this machine, in the order [`KNOWN`] lists
    /// them. A browser that is not installed is not a gap — there is nothing to
    /// integrate with — so it is absent rather than `Missing`.
    pub browsers: Vec<Browser>,
}

/// The six AdGuard knows about: display name, the directory whose existence
/// means the browser is on this machine, and where its manifests live.
///
/// The presence marker is the browser's own profile or config directory, which
/// is what AdGuard's installer keys off too — hence Firefox's being
/// `.mozilla/firefox` rather than `.mozilla`, measured above. Paths are relative
/// to `$HOME` and hard-coded with `.config` rather than `$XDG_CONFIG_HOME`,
/// because the CLI hard-codes them: where the manifest actually lands is
/// AdGuard's decision, not ours, and a check that looked somewhere more correct
/// would be looking in the wrong place.
const KNOWN: [(&str, &str, &str); 6] = [
    (
        "Google Chrome",
        ".config/google-chrome",
        ".config/google-chrome/NativeMessagingHosts",
    ),
    (
        "Chromium",
        ".config/chromium",
        ".config/chromium/NativeMessagingHosts",
    ),
    (
        "Microsoft Edge",
        ".config/microsoft-edge",
        ".config/microsoft-edge/NativeMessagingHosts",
    ),
    (
        "Brave",
        ".config/BraveSoftware/Brave-Browser",
        ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
    ),
    (
        "Vivaldi",
        ".config/vivaldi",
        ".config/vivaldi/NativeMessagingHosts",
    ),
    ("Firefox", ".mozilla/firefox", ".mozilla/native-messaging-hosts"),
];

impl BrowserIntegration {
    /// Run the check for this machine's `$HOME` and this machine's CLI.
    ///
    /// `None` when neither variable below is set, which is the one input
    /// without which none of the six paths can even be formed.
    ///
    /// **`$ADGUARD_BROWSER_HOME` overrides `$HOME`**, and it is the counterpart
    /// of `$ADGUARD_ROOT_HELPER` and `$ADGUARD_CERT_INSTALLER`: the branch a
    /// view exists to render is the unmet one, and any machine that has run
    /// `install-browser-integration` no longer reaches it. Pointing the check
    /// at a directory of empty browser markers makes the whole rendering
    /// reachable again without writing into — or deleting out of — the user's
    /// real browser profiles, which is not something this application should do
    /// to inspect its own row. It overrides `$HOME` rather than the manifest
    /// paths individually because there are six of them and a per-path override
    /// would let them disagree.
    pub fn detect() -> Option<Self> {
        let home = std::env::var_os("ADGUARD_BROWSER_HOME")
            .filter(|home| !home.is_empty())
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)?;
        Some(Self::detect_under(&home, crate::paths::nm_host()))
    }

    /// The same check against an explicit `$HOME` and host binary.
    ///
    /// A parameter for the reason [`crate::RootHelper::inspect`]'s is: this
    /// machine has since run `install-browser-integration`, so the state the
    /// view exists to render is no longer the state this machine is in. Every
    /// branch stays reachable in a temporary directory, without touching the
    /// user's real browser profiles — which a test must not do, because the
    /// files here are ones a browser reads and this project does not write
    /// other programs' configuration even in a test.
    pub fn detect_under(home: &Path, host_binary: Option<PathBuf>) -> Self {
        let host_present = host_binary.as_ref().is_some_and(|path| path.is_file());

        let browsers = KNOWN
            .iter()
            .filter(|(_, marker, _)| home.join(marker).is_dir())
            .map(|(name, _, hosts)| {
                let manifest = home.join(hosts).join(MANIFEST);
                Browser {
                    name,
                    state: read_state(&manifest, host_binary.as_deref()),
                    manifest,
                }
            })
            .collect();

        Self {
            host_binary,
            host_present,
            browsers,
        }
    }

    /// Every browser whose extension cannot reach AdGuard as things stand.
    /// Empty when there is nothing to report — including when this machine has
    /// no browser AdGuard knows about.
    pub fn unmet(&self) -> Vec<&Browser> {
        self.browsers
            .iter()
            .filter(|browser| browser.state != State::Ready)
            .collect()
    }

    /// AdGuard's own command for this, or `None` when there is nothing honest
    /// to name.
    ///
    /// `None` on two different grounds, and the caller has to distinguish them:
    /// no CLI to run, or a CLI whose path cannot be written into a shell command
    /// without changing what the line would do. The host binary being absent is
    /// a third, checked by the caller through [`Self::host_present`] — running
    /// the installer then would write six manifests pointing at nothing.
    pub fn install_command(&self) -> Option<String> {
        let cli = crate::paths::cli_binary().filter(|path| crate::trust::quotable(path))?;
        Some(format!("\"{}\" install-browser-integration", cli.display()))
    }
}

/// Classify one manifest.
fn read_state(manifest: &Path, host_binary: Option<&Path>) -> State {
    let text = match fs::read_to_string(manifest) {
        Ok(text) => text,
        // The ordinary case, and the only `io::Error` that is not a fault:
        // there is simply no manifest here.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return State::Missing,
        Err(err) => return State::Unreadable(err.to_string()),
    };

    let Some(named) = manifest_path(&text) else {
        return State::Unreadable(String::from("no \"path\" in it"));
    };

    // Compared against the host binary beside *this* machine's CLI, not merely
    // tested for existence. A manifest left behind by an AdGuard that has been
    // reinstalled elsewhere can name a file that still exists — the old install
    // — and would then read as met while the extension talked to a stale binary.
    match host_binary {
        Some(expected) if named == expected => State::Ready,
        // Nothing to compare against: the CLI could not be located, which the
        // window says elsewhere. Existence is the most that can be claimed, and
        // claiming more would be a guess.
        None if named.is_file() => State::Ready,
        _ => State::Stale(named),
    }
}

/// The `"path"` value out of a native-messaging manifest.
///
/// A hand-rolled read rather than a JSON dependency, because this is the only
/// JSON in the project and the document is four fixed keys written by AdGuard.
/// It is still a real string reader — escapes and all — because the value is a
/// **file path**, `\\` is legal in one, and a scanner that stopped at the first
/// backslash would report a truncated path and call a working install stale.
///
/// Returns `None` for anything it cannot read confidently. The caller renders
/// that as [`State::Unreadable`], never as the reassuring answer.
fn manifest_path(text: &str) -> Option<PathBuf> {
    let mut rest = text;
    loop {
        let at = rest.find("\"path\"")?;
        rest = &rest[at + "\"path\"".len()..];

        // Only a `:` may sit between the key and its value. Anything else means
        // the match was inside some other string, so keep looking.
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let after = after.trim_start();
        let Some(body) = after.strip_prefix('"') else {
            continue;
        };

        return unescape(body);
    }
}

/// Read one JSON string body — everything up to the closing quote — resolving
/// the escapes a path can legally contain.
///
/// `\uXXXX` is deliberately not handled: AdGuard does not emit it, a path
/// containing one would be a surprise worth reporting rather than decoding, and
/// a half-right decoder here could turn an unreadable manifest into a confident
/// wrong answer. `None` says "ask a human", which is what the row does.
fn unescape(body: &str) -> Option<PathBuf> {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(PathBuf::from(out)),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                _ => return None,
            },
            _ => out.push(ch),
        }
    }
    // Ran out before the closing quote: a truncated file.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn manifest_for(host: &str) -> String {
        format!(
            "{{\n  \"allowed_origins\": [ \"chrome-extension://abc/\" ],\n  \
             \"description\": \"AdGuard CLI Native Messaging Connector\",\n  \
             \"name\": \"{HOST}\",\n  \"path\": \"{host}\",\n  \"type\": \"stdio\"\n}}\n"
        )
    }

    /// A sandbox `$HOME` with the given browser markers, and a host binary.
    fn sandbox(name: &str, markers: &[&str]) -> (Sandbox, PathBuf) {
        let dir = Sandbox::new(name);
        for marker in markers {
            fs::create_dir_all(dir.path().join(marker)).unwrap();
        }
        let host = dir.path().join("opt").join(HOST_BINARY);
        write(&host, "#!/bin/sh\n");
        (dir, host)
    }

    /// The state every stock install is in: AdGuard present, browsers present,
    /// and no manifest anywhere. This is the whole reason the view exists.
    #[test]
    fn browsers_with_no_manifests_are_all_unmet() {
        let (dir, host) = sandbox("none", &[".config/google-chrome", ".mozilla/firefox"]);
        let check = BrowserIntegration::detect_under(dir.path(), Some(host));

        assert!(check.host_present);
        assert_eq!(check.browsers.len(), 2);
        assert_eq!(check.unmet().len(), 2);
        assert!(check
            .browsers
            .iter()
            .all(|browser| browser.state == State::Missing));
        // Named for the sentence the row builds, and in KNOWN's order.
        assert_eq!(check.browsers[0].name, "Google Chrome");
        assert_eq!(check.browsers[1].name, "Firefox");
    }

    /// A browser that is not on this machine is not a gap. Nothing to integrate
    /// with, so nothing to say — and a row per uninstalled browser would be six
    /// rows of noise on every machine.
    #[test]
    fn browsers_that_are_not_installed_are_not_reported() {
        let (dir, host) = sandbox("absent", &[]);
        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        assert!(check.browsers.is_empty());
        assert!(check.unmet().is_empty());
    }

    /// Firefox is gated on its profile directory, not on `.mozilla` — measured
    /// against the installer, which writes nothing for a bare `.mozilla`.
    #[test]
    fn a_bare_mozilla_directory_is_not_firefox() {
        let (dir, host) = sandbox("mozilla", &[".mozilla"]);
        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        assert!(check.browsers.is_empty(), "{check:?}");
    }

    /// The met state, and that the manifest is read rather than merely counted.
    #[test]
    fn a_manifest_naming_this_machines_host_is_ready() {
        let (dir, host) = sandbox("ready", &[".config/chromium"]);
        write(
            &dir.path()
                .join(".config/chromium/NativeMessagingHosts")
                .join(MANIFEST),
            &manifest_for(&host.display().to_string()),
        );

        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        assert_eq!(check.browsers[0].state, State::Ready);
        assert!(check.unmet().is_empty());
    }

    /// The case a bare existence check would get wrong: a manifest left by an
    /// AdGuard installed somewhere else, naming a file that is still there.
    #[test]
    fn a_manifest_naming_another_install_is_stale_even_though_it_exists() {
        let (dir, host) = sandbox("stale", &[".config/vivaldi"]);
        let other = dir.path().join("elsewhere").join(HOST_BINARY);
        write(&other, "#!/bin/sh\n");
        write(
            &dir.path()
                .join(".config/vivaldi/NativeMessagingHosts")
                .join(MANIFEST),
            &manifest_for(&other.display().to_string()),
        );

        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        assert_eq!(check.browsers[0].state, State::Stale(other));
        assert_eq!(check.unmet().len(), 1);
    }

    /// One browser set up and another not — the state a machine lands in by
    /// installing a browser after running the command, which is the case that
    /// makes this a repeated check rather than a one-off.
    #[test]
    fn a_browser_installed_after_the_command_is_the_only_one_unmet() {
        let (dir, host) = sandbox("partial", &[".config/google-chrome", ".config/chromium"]);
        write(
            &dir.path()
                .join(".config/google-chrome/NativeMessagingHosts")
                .join(MANIFEST),
            &manifest_for(&host.display().to_string()),
        );

        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        let unmet = check.unmet();
        assert_eq!(unmet.len(), 1);
        assert_eq!(unmet[0].name, "Chromium");
        assert_eq!(unmet[0].state, State::Missing);
    }

    /// A manifest that is there and cannot be understood must not read as met.
    #[test]
    fn an_unparseable_manifest_is_unreadable_rather_than_ready() {
        let (dir, host) = sandbox("unreadable", &[".config/chromium"]);
        write(
            &dir.path()
                .join(".config/chromium/NativeMessagingHosts")
                .join(MANIFEST),
            "{ \"name\": \"com.adguard.browser_extension_host.nm\" }",
        );

        let check = BrowserIntegration::detect_under(dir.path(), Some(host));
        assert!(matches!(check.browsers[0].state, State::Unreadable(_)));
        assert_eq!(check.unmet().len(), 1);
    }

    /// A missing host binary is its own fact: the manifests would point at
    /// nothing, so the caller must be able to see it without inspecting paths.
    #[test]
    fn a_missing_host_binary_is_reported_separately() {
        let (dir, _) = sandbox("nohost", &[".config/chromium"]);
        let absent = dir.path().join("opt").join("not-here");
        let check = BrowserIntegration::detect_under(dir.path(), Some(absent));
        assert!(!check.host_present);
        assert_eq!(check.unmet().len(), 1);
    }

    #[test]
    fn reads_the_path_out_of_adguards_own_manifest() {
        let text = manifest_for("/home/you/.local/opt/adguard-cli/adguard_cli_nm");
        assert_eq!(
            manifest_path(&text),
            Some(PathBuf::from(
                "/home/you/.local/opt/adguard-cli/adguard_cli_nm"
            ))
        );
    }

    /// `\\` is legal in a path and the scanner must not stop at it — the bug
    /// that would call a working install stale.
    #[test]
    fn resolves_the_escapes_a_path_may_contain() {
        assert_eq!(
            manifest_path(r#"{"path": "/opt/a\\b/nm"}"#),
            Some(PathBuf::from(r"/opt/a\b/nm"))
        );
        assert_eq!(
            manifest_path(r#"{"path": "/opt/a\"b/nm"}"#),
            Some(PathBuf::from("/opt/a\"b/nm"))
        );
        assert_eq!(
            manifest_path(r#"{"path": "/opt/a\/b"}"#),
            Some(PathBuf::from("/opt/a/b"))
        );
    }

    /// A `"path"` inside some other string is not the key.
    #[test]
    fn ignores_the_word_path_where_it_is_not_a_key() {
        assert_eq!(
            manifest_path(r#"{"description": "the \"path\" thing", "path": "/opt/nm"}"#),
            Some(PathBuf::from("/opt/nm"))
        );
    }

    /// Nothing readable, and specifically not a confident wrong answer.
    #[test]
    fn refuses_what_it_cannot_read() {
        assert_eq!(manifest_path(r#"{"name": "x"}"#), None);
        // Truncated before the closing quote.
        assert_eq!(manifest_path(r#"{"path": "/opt/nm"#), None);
        // `\uXXXX` — an escape this reader deliberately does not decode. None
        // says "ask a human"; a half-right decoder would say "/opt/A".
        assert_eq!(manifest_path(r#"{"path": "/opt/\u0041"}"#), None);
    }

    /// A directory that goes away when the test does, without pulling in a
    /// dependency for it — named and seeded the way `config_sandbox.rs` and
    /// `filters_sandbox.rs` build theirs, so the whole workspace has one habit.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "adguard-ui-browser-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
