//! Reading `proxy.yaml`.
//!
//! This module is **read-only by construction** — it holds a parsed document
//! and typed accessors, and offers no way to write. Every mutation goes
//! through `adguard-cli config set` in [`crate::cli`], because roughly half of
//! `proxy.yaml`'s 221 lines are upstream explanatory comments and no YAML
//! serialiser round-trips those. `config set` was measured to be surgical: it
//! replaces the one line and leaves every comment intact.
//!
//! Values come from the file rather than `adguard-cli config show`, which is a
//! rendered view — it folds large sections to `<folded> enabled` and masks
//! secrets (`password: <set>` where the file holds `password: 'admin'`). See
//! `docs/cli-contract.md` §5.
//!
//! ## Why a value tree and not a struct
//!
//! The obvious design — `#[derive(Deserialize)]` onto a `ProxyConfig` struct —
//! is wrong for this file, for a measured reason. The CLI accepts `1` and `0`
//! for a boolean setting:
//!
//! ```text
//! $ adguard-cli config set stealthmode.enabled 1
//! stealthmode.enabled = 1
//! Config has been updated
//! ```
//!
//! and the file then literally holds `enabled: 1` — an integer where a bool
//! belongs. A strict deserialize fails the *whole document* on that one key,
//! so a single type-punned value would blank the entire Protection page rather
//! than one switch. The same applies to any key a future CLI version retypes.
//!
//! So: parse once into a generic tree, then read individual scalars by dotted
//! path with per-key tolerance. An unreadable key costs exactly its own row.
//! The dotted paths are the same strings `config set` takes, which lets one
//! [`Toggle::key`] drive both the read and the write.

use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

use crate::model::Toggle;
use crate::paths;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not locate proxy.yaml — has AdGuard CLI been run yet?")]
    NotFound,

    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid YAML: {message}")]
    Parse { path: PathBuf, message: String },
}

/// A parsed `proxy.yaml`, read at one point in time.
#[derive(Debug, Clone)]
pub struct Config {
    root: Yaml,
    path: PathBuf,
}

impl Config {
    /// Read AdGuard's `proxy.yaml` from its data directory.
    pub fn load() -> Result<Self, Error> {
        let path = paths::config_file().ok_or(Error::NotFound)?;
        Self::read(&path)
    }

    pub fn read(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::NotFound);
        }
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let documents = YamlLoader::load_from_str(text).map_err(|err| Error::Parse {
            path: path.to_owned(),
            message: err.to_string(),
        })?;

        // An empty file parses to zero documents. Treat it as an empty mapping
        // rather than an error: every accessor then returns `None` and the UI
        // reports each setting as unavailable, which is truer than refusing to
        // open the page.
        Ok(Self {
            root: documents.into_iter().next().unwrap_or(Yaml::Null),
            path: path.to_owned(),
        })
    }

    /// The file these values came from — for "edit this yourself" hints.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Walk a dotted path such as `stealthmode.anti_dpi.enabled`.
    ///
    /// Indexing a `Yaml` with a missing key yields `BadValue` rather than
    /// panicking, so a path that runs off the end of the document simply falls
    /// through to a value no typed accessor will accept.
    fn at(&self, key: &str) -> &Yaml {
        key.split('.')
            .fold(&self.root, |node, segment| &node[segment])
    }

    /// Read a boolean setting.
    ///
    /// Deliberately tolerant of what `adguard-cli config set` will actually
    /// leave in the file. Measured on v1.4.13, a boolean key accepts:
    ///
    /// | written | lands in proxy.yaml as | accepted here |
    /// | --- | --- | --- |
    /// | `true` / `false` | `true` / `false` | yes — what we always write |
    /// | `1` / `0` | `1` / `0` (an **integer**) | yes — the type-pun |
    /// | `True`, `TRUE`, `yes`, `on` | rejected, file unchanged | n/a |
    ///
    /// A hand-editing user may also have quoted it, so a `'true'` string is
    /// honoured too. Anything else is `None` — unknown beats guessing, since
    /// the caller renders `None` as "unavailable" rather than as "off".
    pub fn bool_at(&self, key: &str) -> Option<bool> {
        match self.at(key) {
            Yaml::Boolean(value) => Some(*value),
            // The type-pun: `config set <bool key> 1` writes an integer.
            Yaml::Integer(1) => Some(true),
            Yaml::Integer(0) => Some(false),
            Yaml::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Some(true),
                "false" | "no" | "off" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn int_at(&self, key: &str) -> Option<i64> {
        match self.at(key) {
            Yaml::Integer(value) => Some(*value),
            // A quoted port survives a hand edit as a string.
            Yaml::String(text) => text.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn str_at(&self, key: &str) -> Option<&str> {
        match self.at(key) {
            Yaml::String(text) => Some(text.as_str()),
            _ => None,
        }
    }

    /// Read an enumerated setting, returning the matching entry of `options`.
    ///
    /// Case-insensitive, because the CLI is: `config set log_level INFO` and
    /// `config set outbound_proxy.mode socks5` are both accepted and the value
    /// is written back **verbatim**, so the file can legitimately hold `'INFO'`
    /// where the comment says `info`, or `'socks5'` where the default is
    /// `'HTTP'`. Matching exactly would render those rows as unavailable.
    ///
    /// Returns the canonical spelling from `options` rather than the file's, so
    /// a caller can compare it by pointer or feed it straight to a combo row.
    /// A value outside the list is `None` — the CLI refuses to write one, so it
    /// means a hand edit, and "unavailable" is the honest rendering.
    pub fn choice_at(&self, key: &str, options: &[&'static str]) -> Option<&'static str> {
        let value = self.str_at(key)?.trim();
        options
            .iter()
            .find(|option| option.eq_ignore_ascii_case(value))
            .copied()
    }

    /// Read a list-valued setting, e.g. `filters` or `dns_filtering.filters`.
    ///
    /// These are the keys `config get` refuses with *"This field is not a
    /// separate setting"*; they are written with `config list-add` /
    /// `list-remove`. Non-scalar entries are skipped rather than failing the
    /// whole read — `apps` mixes bare strings with mappings.
    pub fn list_at(&self, key: &str) -> Option<Vec<&str>> {
        let items = self.at(key).as_vec()?;
        Some(items.iter().filter_map(Yaml::as_str).collect())
    }

    /// The state of one Protection switch.
    ///
    /// `None` means the key is absent or holds something that is not a
    /// boolean — render the row as unavailable rather than as off, because the
    /// two are very different claims to make about ad blocking.
    pub fn toggle(&self, toggle: Toggle) -> Option<bool> {
        self.bool_at(toggle.key())
    }

    /// `proxy_mode` — `manual` or `auto`.
    pub fn proxy_mode(&self) -> Option<&str> {
        self.str_at(key::PROXY_MODE)
    }

    /// Whether DNS filtering, if switched on, would actually do anything.
    ///
    /// In `manual` proxy mode the local DNS proxy only listens when
    /// `dns_filtering.listen_port` is a real port. The file documents this
    /// itself:
    ///
    /// ```text
    /// # -1 = disabled (no DNS proxy in manual mode; no extra listener in auto mode)
    /// #  0 = random port in manual mode (original behaviour); ...
    /// #  N = listen on port N (e.g. 5353) — required for DNS filtering in manual proxy mode
    /// ```
    ///
    /// So `dns_filtering.enabled: true` with `listen_port: -1` in manual mode
    /// is a switch that reads on and filters nothing. The CLI does not warn
    /// about it — nothing enforces this dependency, as `config set` will
    /// happily accept either key in any order — so the UI has to.
    /// Tested against `auto` rather than for `manual` so that an unreadable,
    /// misspelled or future mode is treated as manual and still shows the
    /// caveat. Only `auto` is known not to need the listener, and warning
    /// about a limitation that does not apply is a great deal better than
    /// staying quiet about one that does.
    pub fn dns_filtering_is_inert(&self) -> bool {
        let needs_listener = self
            .proxy_mode()
            .is_none_or(|mode| !mode.trim().eq_ignore_ascii_case("auto"));
        needs_listener && self.int_at(key::DNS_LISTEN_PORT) == Some(-1)
    }

    /// Whether the proxy is reachable from outside this machine.
    pub fn listens_beyond_loopback(&self) -> bool {
        self.str_at(key::LISTEN_ADDRESS)
            .is_some_and(|address| !is_loopback(address))
    }

    pub fn listen_auth_enabled(&self) -> Option<bool> {
        self.bool_at(key::LISTEN_AUTH_ENABLED)
    }

    /// Everything [`listen_address_plan`] needs to know about `listen_auth`.
    ///
    /// Each field defaults to the value that makes the plan *more* cautious
    /// when the key cannot be read: authentication off, credentials absent. The
    /// two mistakes are not symmetric — over-estimating what is configured
    /// produces a `config set` that silently does nothing while reporting
    /// success, which is the exact failure this whole path exists to avoid.
    pub fn listen_auth(&self) -> AuthState {
        AuthState {
            enabled: self.listen_auth_enabled().unwrap_or(false),
            username_set: self.credential_set(key::LISTEN_AUTH_USERNAME),
            password_set: self.credential_set(key::LISTEN_AUTH_PASSWORD),
        }
    }

    /// Is this credential non-empty as far as the *CLI* is concerned?
    ///
    /// Measured: the check is a literal emptiness test, not a trim. A username
    /// of `' '` — one space — is enough to satisfy it and the address write
    /// goes through. So this deliberately does not trim: its only job is to
    /// predict the CLI's behaviour, and trimming would block a write that would
    /// in fact have succeeded.
    fn credential_set(&self, key: &str) -> bool {
        self.str_at(key).is_some_and(|value| !value.is_empty())
    }
}

/// The state of `listen_auth`, as the CLI will see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthState {
    pub enabled: bool,
    /// `listen_auth.username` is present and not the empty string.
    pub username_set: bool,
    pub password_set: bool,
}

impl AuthState {
    /// Can the proxy be exposed beyond loopback without the CLI stopping to ask
    /// for credentials it cannot read?
    pub fn is_complete(self) -> bool {
        self.enabled && self.username_set && self.password_set
    }

    /// Why exposing the proxy beyond loopback is currently impossible, or
    /// `None` when it is not.
    ///
    /// Only the credentials are consulted, not [`Self::enabled`]:
    /// [`listen_address_plan`] switches authentication on by itself, but it
    /// cannot invent a username or a password. Lets the UI explain the
    /// constraint before the user runs into it.
    pub fn exposure_blocker(self) -> Option<String> {
        (!self.username_set || !self.password_set)
            .then(|| missing_credentials_message(!self.username_set, !self.password_set))
    }
}

/// Name the credentials that have to be set, and why it matters.
///
/// Worth spelling out rather than passing the CLI's own wording through,
/// because the CLI's advice is *wrong* in one of the three cases: it always
/// prompts for a username and always names `config set listen_auth.username`,
/// even when the username is fine and it is the password that is empty.
/// Following that advice would not fix it.
fn missing_credentials_message(username: bool, password: bool) -> String {
    let missing = match (username, password) {
        (true, true) => "a username and a password",
        (true, false) => "a username",
        (false, true) => "a password",
        // Not constructed by either caller, both of which check first. Kept
        // total rather than panicking.
        (false, false) => "credentials",
    };
    format!(
        "Set {missing} for the proxy before letting it listen beyond this \
         machine — without them AdGuard silently keeps the old address"
    )
}

/// The dotted paths this crate reads and writes.
///
/// One constant per setting, used for both the `proxy.yaml` lookup and the
/// `adguard-cli config set` argument, so the two can never drift apart.
pub mod key {
    pub const PROXY_MODE: &str = "proxy_mode";
    pub const AD_BLOCKING: &str = "ad_blocking_enabled";
    pub const HTTPS_FILTERING: &str = "https_filtering.enabled";
    pub const STEALTH_MODE: &str = "stealthmode.enabled";
    pub const DNS_FILTERING: &str = "dns_filtering.enabled";
    pub const SAFE_BROWSING: &str = "safebrowsing.enabled";
    pub const CRLITE: &str = "crlite.enabled";

    pub const DNS_LISTEN_PORT: &str = "dns_filtering.listen_port";

    // --- the Advanced page ---
    pub const LISTEN_ADDRESS: &str = "listen_address";
    pub const LISTEN_AUTH_ENABLED: &str = "listen_auth.enabled";
    pub const LISTEN_AUTH_USERNAME: &str = "listen_auth.username";
    pub const LISTEN_AUTH_PASSWORD: &str = "listen_auth.password";
    pub const LISTEN_PORT_HTTP: &str = "listen_ports.http_proxy";
    pub const LISTEN_PORT_SOCKS5: &str = "listen_ports.socks5_proxy";
    pub const WORKER_THREADS: &str = "worker_threads";
    pub const LOG_LEVEL: &str = "log_level";
    pub const OUTBOUND_ENABLED: &str = "outbound_proxy.enabled";
    pub const OUTBOUND_MODE: &str = "outbound_proxy.mode";
    pub const OUTBOUND_HOST: &str = "outbound_proxy.host";
    pub const OUTBOUND_PORT: &str = "outbound_proxy.port";
    pub const OUTBOUND_USERNAME: &str = "outbound_proxy.username";
    pub const OUTBOUND_PASSWORD: &str = "outbound_proxy.password";
    pub const OUTBOUND_TRUST_ANY_CERT: &str = "outbound_proxy.trust_any_certificate";
    pub const OUTBOUND_UDP_VIA_SOCKS5: &str = "outbound_proxy.udp_through_socks5_enabled";
}

/// Is this listen address confined to the local machine?
///
/// Anything else exposes the proxy to the network, which is what makes
/// [`listen_address_plan`] necessary.
///
/// Unrecognised input answers `false` — "not known to be loopback". The two
/// possible mistakes are not symmetric: calling a loopback address exposed
/// costs a needless authentication prompt, while calling an exposed address
/// loopback leaves an open proxy on the network.
pub fn is_loopback(address: &str) -> bool {
    let address = address.trim().trim_matches(['\'', '"']).trim();
    if address.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // The bracketed form a URL would use.
    let address = address
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(address);

    address.parse::<std::net::IpAddr>().is_ok_and(|ip| {
        // `to_canonical` unwraps IPv4-mapped IPv6 — `::ffff:127.0.0.1` is
        // loopback in practice, but `Ipv6Addr::is_loopback` reserves `true`
        // for `::1` alone and would call it exposed.
        ip.to_canonical().is_loopback()
    })
}

/// What has to happen for `listen_address` to actually become `address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressPlan {
    /// The ordered `config set` calls to issue. The order is load-bearing;
    /// stop at the first one that fails.
    Calls(Vec<(&'static str, String)>),

    /// The move cannot be made yet, and **no call should be issued**: the CLI
    /// would try to collect the missing credentials interactively, find no TTY,
    /// keep the old address, and still print `Config has been updated`.
    ///
    /// At least one of the two flags is true.
    NeedsCredentials { username: bool, password: bool },
}

impl AddressPlan {
    /// The calls to issue, or `&[]` when the plan is blocked.
    pub fn calls(&self) -> &[(&'static str, String)] {
        match self {
            Self::Calls(calls) => calls,
            Self::NeedsCredentials { .. } => &[],
        }
    }

    /// Why the move is blocked, phrased for the user. See
    /// [`missing_credentials_message`] for why this is not the CLI's own wording.
    pub fn blocked_reason(&self) -> Option<String> {
        match self {
            Self::Calls(_) => None,
            Self::NeedsCredentials { username, password } => {
                Some(missing_credentials_message(*username, *password))
            }
        }
    }
}

/// The ordered `config set` calls that move `listen_address` safely.
///
/// `architecture.md` §5 requires authentication to be forced on when the listen
/// address leaves loopback, since the config comment says so (*"if not
/// localhost, authentication is required"*). Measurement turned that from a
/// fix-up into a **precondition**, because the CLI tries to collect credentials
/// interactively and cannot be driven headlessly:
///
/// ```text
/// $ adguard-cli config set listen_address 0.0.0.0     # listen_auth off
/// Enter username for accessing proxy server:
/// Warning: No TTY for user input. Use `adguard-cli config set listen_auth.username` ...
/// listen_address = 127.0.0.1
/// Config has been updated
/// ```
///
/// Note the sting: the address it echoes back is the **old** one and the file is
/// untouched, yet it still claims the config was updated.
///
/// # Enabling authentication is necessary but not sufficient
///
/// Measured on v1.4.13, against a sandboxed copy of `proxy.yaml`. The prompt
/// appears unless authentication is on **and both credentials are non-empty**:
///
/// | `enabled` | `username` | `password` | `config set listen_address 0.0.0.0` |
/// | --- | --- | --- | --- |
/// | `false` | `admin` | `admin` | prompts, no-op |
/// | `true` | `''` | `admin` | prompts, no-op |
/// | `true` | `admin` | `''` | **prompts, no-op** |
/// | `true` | `admin` | `admin` | succeeds |
///
/// The third row is why this returns a plan rather than a list of calls: an
/// earlier version enabled authentication and then wrote the address, which on
/// a machine with a blank password would have reported success and changed
/// nothing. There is no way to fix that by reordering, and inventing a password
/// on the user's behalf would be a security decision made behind their back —
/// one they could never log in past. So the move is refused and named.
///
/// Note also that the CLI prompts for a *username* whichever credential is
/// missing, and its suggested remedy names only `listen_auth.username`; see
/// [`AddressPlan::blocked_reason`].
///
/// # Retreating to loopback is always allowed
///
/// Measured from every broken starting state — exposed with authentication off,
/// with an empty username, with an empty password: writing a loopback address
/// always succeeds and never prompts. The trigger is the **new** value, not the
/// old one. That asymmetry matters, because it means a user who is exposed with
/// unusable credentials can always be brought back to safety; the UI must never
/// gate the retreat behind the same checks as the exposure.
pub fn listen_address_plan(address: &str, auth: AuthState) -> AddressPlan {
    if is_loopback(address) {
        return AddressPlan::Calls(vec![(key::LISTEN_ADDRESS, address.to_owned())]);
    }

    if !auth.username_set || !auth.password_set {
        return AddressPlan::NeedsCredentials {
            username: !auth.username_set,
            password: !auth.password_set,
        };
    }

    let mut calls = Vec::new();
    if !auth.enabled {
        calls.push((key::LISTEN_AUTH_ENABLED, "true".to_owned()));
    }
    calls.push((key::LISTEN_ADDRESS, address.to_owned()));
    AddressPlan::Calls(calls)
}

/// Notices when `proxy.yaml` has actually *changed*.
///
/// # Why a file monitor cannot trust its own events
///
/// Every `adguard-cli` invocation rewrites `proxy.yaml` and touches its mtime —
/// `--version` included, and even when not one byte differs (contract §5). The
/// app polls `status` every 2 s with a window open, so a `gio::FileMonitor`
/// wired straight to a repaint would fire against the app's own traffic, for
/// the whole life of the session, redrawing pages under the user's pointer.
/// Debouncing does not help: the churn never stops, so there is no quiet period
/// to debounce *to*.
///
/// So the event is only a prompt to look. The content decides.
///
/// # Bytes, not a digest
///
/// The file is ~9 KB. Comparing it whole costs less than the read that produced
/// it, needs no hashing crate, and cannot collide — an edit that a digest
/// happened to hash identically would be an edit the UI silently ignored.
pub struct Watch {
    path: PathBuf,
    /// The text behind the last [`Config`] this handed out.
    ///
    /// Held as the raw text rather than the parsed value because that is what
    /// the comparison needs, and because `Config` is a lossy view of it: two
    /// different files can parse to the same tree, and one of them may be the
    /// user's comments.
    seen: Option<String>,
}

impl Watch {
    /// Start watching, having seen nothing. The first [`Self::changed`] call
    /// therefore reports a change — which is what a caller priming itself at
    /// startup wants.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            seen: None,
        }
    }

    /// Watch AdGuard's own `proxy.yaml`.
    pub fn on_config() -> Option<Self> {
        paths::config_file().map(Self::new)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record the file as it is now, without reporting it.
    ///
    /// For a caller that has just read the file itself and only wants to hear
    /// about what happens *next*. Priming with [`Self::changed`] instead would
    /// hand back a change that is really the state already on screen — costing
    /// a redundant repaint, and making a startup indistinguishable from an edit
    /// in whatever the caller does with the answer.
    pub fn prime(&mut self) {
        let _ = self.changed();
    }

    /// Re-read the file, and return a [`Config`] only if the bytes moved.
    ///
    /// `None` covers four cases that all mean "nothing to repaint from":
    /// unchanged, unreadable, absent, and unparseable.
    ///
    /// The snapshot advances **only on a successful parse**. The CLI rewrites
    /// this file in place, so a read can catch it half-written; storing that
    /// text would mean the completed write — differing from the torn read —
    /// looked like just another change, but storing *nothing* means the next
    /// look tries again. It also makes a genuinely malformed file retry
    /// harmlessly rather than latch.
    pub fn changed(&mut self) -> Option<Config> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        if self.seen.as_deref() == Some(text.as_str()) {
            return None;
        }

        // Parse before storing: see above.
        let config = Config::parse(&text, &self.path).ok()?;
        self.seen = Some(text);
        Some(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference machine's `proxy.yaml`, trimmed to the keys under test
    /// but keeping the comment density that motivates never writing this file.
    const SAMPLE: &str = r#"
# AdGuard CLI configuration file

# Supported proxy modes are: manual, auto
proxy_mode: 'manual'
# if not localhost, authentication is required
listen_address: '127.0.0.1'
listen_auth:
  enabled: false
  username: 'admin'
  password: 'admin'

# Apply ad-blocking filters to requests
ad_blocking_enabled: true

filters:
  - 'flm://'
  - 'user.txt'

https_filtering:
  enabled: true
  filter_secure_dns_mode: 'transparent'

dns_filtering:
  enabled: false
  filters:
    - 'dns_user.txt'
  listen_port: -1

safebrowsing:
  enabled: true

crlite:
  enabled: true

stealthmode:
  enabled: false
  anti_dpi:
    enabled: false
"#;

    fn sample() -> Config {
        Config::parse(SAMPLE, Path::new("proxy.yaml")).expect("sample should parse")
    }

    #[test]
    fn reads_top_level_and_nested_booleans() {
        let config = sample();
        assert_eq!(config.bool_at(key::AD_BLOCKING), Some(true));
        assert_eq!(config.bool_at(key::HTTPS_FILTERING), Some(true));
        assert_eq!(config.bool_at(key::STEALTH_MODE), Some(false));
        assert_eq!(config.bool_at(key::DNS_FILTERING), Some(false));
    }

    /// `config show anti_dpi` fails because only top-level sections expand,
    /// but reading the file has no such limit — depth is just another segment.
    #[test]
    fn reads_a_three_deep_path() {
        assert_eq!(
            sample().bool_at("stealthmode.anti_dpi.enabled"),
            Some(false)
        );
    }

    /// The measured type-pun: `config set stealthmode.enabled 1` is accepted
    /// and writes an integer. Rejecting it would blank the switch for a value
    /// the CLI itself produced.
    #[test]
    fn integer_written_by_the_cli_reads_as_a_boolean() {
        let config = Config::parse("a: 1\nb: 0\n", Path::new("t.yaml")).unwrap();
        assert_eq!(config.bool_at("a"), Some(true));
        assert_eq!(config.bool_at("b"), Some(false));
    }

    /// One bad key must cost one row, not the page — the reason this module
    /// walks a tree instead of deriving Deserialize.
    #[test]
    fn a_junk_value_does_not_poison_its_neighbours() {
        let config = Config::parse(
            "ad_blocking_enabled: sometimes\nsafebrowsing:\n  enabled: true\n",
            Path::new("t.yaml"),
        )
        .unwrap();
        assert_eq!(config.bool_at(key::AD_BLOCKING), None);
        assert_eq!(config.bool_at(key::SAFE_BROWSING), Some(true));
    }

    /// Absent is `None`, never `false`: "we don't know" and "ad blocking is
    /// off" are different claims and the UI shows them differently.
    #[test]
    fn missing_keys_are_unknown_not_false() {
        let config = sample();
        assert_eq!(config.bool_at("no_such_key"), None);
        assert_eq!(config.bool_at("stealthmode.no_such_key"), None);
        assert_eq!(config.bool_at("ad_blocking_enabled.deeper"), None);
        assert_eq!(config.bool_at("no.such.path.at.all"), None);
    }

    /// A scalar key with a path walked *through* it must not resolve.
    #[test]
    fn walking_through_a_scalar_yields_nothing() {
        assert_eq!(sample().str_at("proxy_mode.enabled"), None);
    }

    #[test]
    fn reads_scalars_and_lists() {
        let config = sample();
        assert_eq!(config.proxy_mode(), Some("manual"));
        assert_eq!(config.int_at(key::DNS_LISTEN_PORT), Some(-1));
        assert_eq!(config.list_at("filters"), Some(vec!["flm://", "user.txt"]));
        assert_eq!(
            config.list_at("dns_filtering.filters"),
            Some(vec!["dns_user.txt"])
        );
        assert_eq!(config.list_at("proxy_mode"), None);
    }

    /// An empty or comment-only file opens with everything unknown rather than
    /// failing — the page then says so per row.
    #[test]
    fn an_empty_document_is_not_an_error() {
        let config = Config::parse("# just a comment\n", Path::new("t.yaml")).unwrap();
        assert_eq!(config.bool_at(key::AD_BLOCKING), None);
        assert_eq!(config.proxy_mode(), None);
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(Config::parse("a:\n\t- tab indent\n", Path::new("t.yaml")).is_err());
    }

    /// `dns_filtering.enabled: true` with `listen_port: -1` in manual mode is
    /// a switch that filters nothing. Nothing in the CLI says so.
    #[test]
    fn dns_filtering_is_inert_without_a_listen_port() {
        assert!(sample().dns_filtering_is_inert());

        let with_port = Config::parse(
            "proxy_mode: 'manual'\ndns_filtering:\n  listen_port: 5353\n",
            Path::new("t.yaml"),
        )
        .unwrap();
        assert!(!with_port.dns_filtering_is_inert());
    }

    /// In `auto` mode the listener is not needed, so -1 is not a problem.
    #[test]
    fn auto_mode_does_not_need_a_dns_listen_port() {
        let auto = Config::parse(
            "proxy_mode: 'auto'\ndns_filtering:\n  listen_port: -1\n",
            Path::new("t.yaml"),
        )
        .unwrap();
        assert!(!auto.dns_filtering_is_inert());
    }

    #[test]
    fn recognises_loopback_addresses() {
        for address in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
            "127.1.2.3",
            "'127.0.0.1'",
            "127.0.0.1 ",
            // IPv4-mapped IPv6: loopback in practice, though
            // `Ipv6Addr::is_loopback` alone would say otherwise.
            "::ffff:127.0.0.1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(is_loopback(address), "{address:?} should be loopback");
        }

        for address in ["0.0.0.0", "192.168.1.10", "::", "example.com", ""] {
            assert!(!is_loopback(address), "{address:?} should not be loopback");
        }
    }

    /// The asymmetry is deliberate: something we cannot parse must not be
    /// waved through as local. A needless authentication prompt is a nuisance;
    /// an open proxy on the network is a hole.
    #[test]
    fn unparseable_addresses_are_not_assumed_local() {
        for address in ["", "  ", "0.0.0.0:3128", "127.0.0.1/8", "not an address"] {
            assert!(!is_loopback(address), "{address:?} was assumed to be local");
        }
    }

    /// An unreadable or unrecognised `proxy_mode` must still raise the caveat:
    /// only `auto` is known not to need the DNS listener.
    #[test]
    fn only_auto_mode_escapes_the_dns_caveat() {
        let inert = |yaml: &str| {
            Config::parse(yaml, Path::new("t.yaml"))
                .unwrap()
                .dns_filtering_is_inert()
        };

        for mode in ["'manual'", "'MANUAL'", "'manual '", "bogus", "1", "null"] {
            assert!(
                inert(&format!(
                    "proxy_mode: {mode}\ndns_filtering:\n  listen_port: -1\n"
                )),
                "proxy_mode {mode} should still warn",
            );
        }
        assert!(inert("dns_filtering:\n  listen_port: -1\n"), "absent mode should warn");

        for mode in ["'auto'", "auto", "'AUTO'", "'auto '"] {
            assert!(
                !inert(&format!(
                    "proxy_mode: {mode}\ndns_filtering:\n  listen_port: -1\n"
                )),
                "proxy_mode {mode} needs no listener",
            );
        }
    }

    /// A duplicate key fails the whole document rather than silently picking
    /// one of the two values. That is the right trade for a file the user is
    /// invited to hand-edit: the parse error names the line, where a silent
    /// choice would leave them wondering why their edit did nothing.
    #[test]
    fn a_duplicate_key_is_a_loud_failure() {
        let err = Config::parse(
            "ad_blocking_enabled: true\nad_blocking_enabled: false\n",
            Path::new("t.yaml"),
        )
        .expect_err("duplicate keys should not parse");
        assert!(
            err.to_string().contains("line 2"),
            "the error should locate the duplicate: {err}"
        );
    }

    #[test]
    fn sample_listens_on_loopback_only() {
        let config = sample();
        assert!(!config.listens_beyond_loopback());
        assert_eq!(config.listen_auth_enabled(), Some(false));
    }

    /// The CLI writes an enum value back verbatim, so `'INFO'` and `'socks5'`
    /// are both things the real file can hold. Matching case-sensitively would
    /// blank those rows.
    #[test]
    fn enum_values_are_matched_case_insensitively() {
        const LEVELS: [&str; 3] = ["info", "debug", "trace"];
        let read = |yaml: &str| {
            Config::parse(yaml, Path::new("t.yaml"))
                .unwrap()
                .choice_at(key::LOG_LEVEL, &LEVELS)
        };

        assert_eq!(read("log_level: 'info'"), Some("info"));
        assert_eq!(read("log_level: 'INFO'"), Some("info"), "the CLI accepts INFO");
        assert_eq!(read("log_level: Debug"), Some("debug"));
        assert_eq!(read("log_level: ' trace '"), Some("trace"));
        // Outside the list, or not a string at all: unavailable, not a guess.
        assert_eq!(read("log_level: 'bogus'"), None);
        assert_eq!(read("log_level: 3"), None);
        assert_eq!(read("other: 1"), None);
    }

    /// The sample has credentials but authentication switched off — the state
    /// the reference machine is actually in.
    #[test]
    fn reads_the_auth_state() {
        assert_eq!(
            sample().listen_auth(),
            AuthState {
                enabled: false,
                username_set: true,
                password_set: true,
            }
        );
    }

    /// An unreadable `listen_auth` must not be optimistically reported as
    /// usable: over-estimating it is what produces a silently-failing write.
    #[test]
    fn an_unreadable_auth_state_is_assumed_absent() {
        let config = Config::parse("listen_address: '127.0.0.1'", Path::new("t.yaml")).unwrap();
        assert_eq!(
            config.listen_auth(),
            AuthState {
                enabled: false,
                username_set: false,
                password_set: false,
            }
        );
        assert!(!config.listen_auth().is_complete());
    }

    /// Measured: the CLI's emptiness check is literal, not a trim — a username
    /// of one space is enough to satisfy it. Mirroring that exactly is the
    /// point of this function; trimming would block a write that would in fact
    /// have gone through.
    #[test]
    fn a_whitespace_credential_counts_as_set_because_the_cli_says_so() {
        let config = Config::parse(
            "listen_auth:\n  enabled: true\n  username: ' '\n  password: ' '\n",
            Path::new("t.yaml"),
        )
        .unwrap();
        assert!(config.listen_auth().is_complete());
    }

    /// The order is the whole point: without auth already on, the CLI prompts
    /// for a username, finds no TTY, and silently leaves the address alone
    /// while printing "Config has been updated".
    #[test]
    fn leaving_loopback_enables_auth_first() {
        let auth = AuthState {
            enabled: false,
            username_set: true,
            password_set: true,
        };
        assert_eq!(
            listen_address_plan("0.0.0.0", auth),
            AddressPlan::Calls(vec![
                (key::LISTEN_AUTH_ENABLED, "true".to_owned()),
                (key::LISTEN_ADDRESS, "0.0.0.0".to_owned()),
            ])
        );
    }

    #[test]
    fn auth_already_on_needs_no_extra_call() {
        let auth = AuthState {
            enabled: true,
            username_set: true,
            password_set: true,
        };
        assert_eq!(
            listen_address_plan("0.0.0.0", auth),
            AddressPlan::Calls(vec![(key::LISTEN_ADDRESS, "0.0.0.0".to_owned())])
        );
    }

    /// Enabling authentication is not sufficient. Measured: with `enabled:
    /// true` and either credential empty, the address write still prompts and
    /// still silently no-ops. So the plan must refuse rather than issue calls
    /// that would report success and change nothing.
    #[test]
    fn leaving_loopback_is_refused_without_credentials() {
        let cases = [
            (false, false, true, true),
            (true, false, false, true),
            (false, true, true, false),
        ];

        for (username_set, password_set, want_username, want_password) in cases {
            let auth = AuthState {
                enabled: true,
                username_set,
                password_set,
            };
            assert_eq!(
                listen_address_plan("0.0.0.0", auth),
                AddressPlan::NeedsCredentials {
                    username: want_username,
                    password: want_password,
                },
                "username_set={username_set} password_set={password_set}",
            );
        }
    }

    /// A blocked plan must hand back no calls at all. Issuing the first one
    /// alone would switch authentication on for an address that then never
    /// moves — a change the user did not ask for, in service of nothing.
    #[test]
    fn a_blocked_plan_issues_nothing() {
        let auth = AuthState {
            enabled: false,
            username_set: false,
            password_set: false,
        };
        let plan = listen_address_plan("0.0.0.0", auth);
        assert!(plan.calls().is_empty());
        assert!(plan.blocked_reason().is_some());
    }

    /// The CLI always prompts for a *username* and always suggests setting
    /// `listen_auth.username`, even when the password is the empty one. Our
    /// message has to name the credential that is actually missing, or it sends
    /// the user to fix the wrong field.
    #[test]
    fn the_block_names_the_credential_that_is_missing() {
        let reason = |username: bool, password: bool| {
            AddressPlan::NeedsCredentials { username, password }
                .blocked_reason()
                .expect("a blocked plan has a reason")
        };

        assert!(reason(false, true).contains("a password"));
        assert!(!reason(false, true).contains("username"));
        assert!(reason(true, false).contains("a username"));
        assert!(!reason(true, false).contains("password"));
        assert!(reason(true, true).contains("a username and a password"));
    }

    /// Returning to loopback must not switch authentication on as a side
    /// effect — the requirement is about exposure, not a ratchet.
    #[test]
    fn returning_to_loopback_leaves_auth_alone() {
        let auth = AuthState {
            enabled: false,
            username_set: true,
            password_set: true,
        };
        assert_eq!(
            listen_address_plan("127.0.0.1", auth),
            AddressPlan::Calls(vec![(key::LISTEN_ADDRESS, "127.0.0.1".to_owned())])
        );
    }

    /// The safety-critical direction, measured from every broken state: a
    /// retreat to loopback always succeeds, so it must never be gated behind
    /// the credential check that guards exposure. Getting this wrong would
    /// strand a user on an open address precisely because their credentials
    /// were unusable.
    #[test]
    fn the_retreat_to_loopback_is_never_blocked() {
        let broken = AuthState {
            enabled: false,
            username_set: false,
            password_set: false,
        };
        for address in ["127.0.0.1", "127.0.0.2", "::1"] {
            assert_eq!(
                listen_address_plan(address, broken),
                AddressPlan::Calls(vec![(key::LISTEN_ADDRESS, address.to_owned())]),
                "{address} should always be reachable",
            );
        }
    }

    // ---- Watch ----

    /// A `proxy.yaml` in a directory of its own, so these can run in parallel.
    fn scratch(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("adguard-watch-{name}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("proxy.yaml");
        std::fs::write(&path, text).expect("write scratch config");
        path
    }

    /// The whole reason this type exists. Every `adguard-cli` invocation
    /// rewrites the file without changing it, and the app causes one every
    /// 2 seconds — if a byte-identical rewrite read as a change, the pages
    /// would repaint continuously for the life of the session.
    #[test]
    fn a_byte_identical_rewrite_is_not_a_change() {
        let path = scratch("identical", SAMPLE);
        let mut watch = Watch::new(&path);
        assert!(watch.changed().is_some(), "the first look primes");

        for _ in 0..5 {
            std::fs::write(&path, SAMPLE).expect("rewrite");
            assert!(
                watch.changed().is_none(),
                "an unchanged rewrite must not read as a change"
            );
        }
    }

    #[test]
    fn a_real_edit_is_a_change() {
        let path = scratch("edited", SAMPLE);
        let mut watch = Watch::new(&path);
        watch.changed().expect("prime");

        std::fs::write(&path, SAMPLE.replace("proxy_mode: 'manual'", "proxy_mode: 'auto'"))
            .expect("edit");
        let config = watch.changed().expect("an edit should be seen");
        assert_eq!(config.proxy_mode(), Some("auto"));
        assert!(watch.changed().is_none(), "and only once");
    }

    /// A comment-only edit changes no value, but the file did move — reporting
    /// it costs one repaint that renders identically, while suppressing it
    /// would mean diffing parsed trees and guessing what counts.
    #[test]
    fn a_comment_only_edit_is_still_a_change() {
        let path = scratch("comment", SAMPLE);
        let mut watch = Watch::new(&path);
        watch.changed().expect("prime");

        std::fs::write(&path, format!("# a note to self\n{SAMPLE}")).expect("edit");
        assert!(watch.changed().is_some());
    }

    /// A torn read must not become the baseline: the completed write would then
    /// look like an ordinary change, and a file that is briefly invalid must
    /// not latch the watch into ignoring it.
    #[test]
    fn a_broken_file_does_not_advance_the_snapshot() {
        let path = scratch("broken", SAMPLE);
        let mut watch = Watch::new(&path);
        watch.changed().expect("prime");

        std::fs::write(&path, "listen_ports:\n  - [unclosed\n").expect("write junk");
        assert!(watch.changed().is_none(), "unparseable yields nothing");
        assert!(watch.changed().is_none(), "and stays that way while it is");

        std::fs::write(&path, SAMPLE.replace("proxy_mode: 'manual'", "proxy_mode: 'auto'"))
            .expect("repair");
        let config = watch
            .changed()
            .expect("the repaired file must still be seen as a change");
        assert_eq!(config.proxy_mode(), Some("auto"));
    }

    #[test]
    fn a_missing_file_is_not_a_change() {
        let path = std::env::temp_dir().join("adguard-watch-absent/proxy.yaml");
        let _ = std::fs::remove_file(&path);
        let mut watch = Watch::new(&path);
        assert!(watch.changed().is_none());
    }
}
