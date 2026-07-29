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
    pub const LISTEN_ADDRESS: &str = "listen_address";
    pub const LISTEN_AUTH_ENABLED: &str = "listen_auth.enabled";
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

/// The ordered `config set` calls that move `listen_address` safely.
///
/// `architecture.md` §5 requires authentication to be forced on when the
/// listen address leaves loopback, since the config comment says so
/// (*"if not localhost, authentication is required"*). Measurement turned that
/// from a fix-up into a **precondition**, because the CLI tries to collect
/// credentials interactively and cannot be driven headlessly:
///
/// ```text
/// $ adguard-cli config set listen_address 0.0.0.0     # listen_auth off
/// Enter username for accessing proxy server:
/// Warning: No TTY for user input. Use `adguard-cli config set listen_auth.username` ...
/// listen_address = 127.0.0.1
/// Config has been updated
/// ```
///
/// Note the sting: the address it echoes back is the **old** one and the file
/// is untouched, yet it still claims the config was updated. Enabling
/// `listen_auth.enabled` first makes the same command succeed without a
/// prompt. So the order below is load-bearing, not cosmetic — reversed, the
/// second call silently does nothing while reporting success.
///
/// Returns the calls needed, in order; empty when nothing has to change.
pub fn listen_address_plan(address: &str, auth_enabled: bool) -> Vec<(&'static str, String)> {
    let mut plan = Vec::new();
    if !is_loopback(address) && !auth_enabled {
        plan.push((key::LISTEN_AUTH_ENABLED, "true".to_owned()));
    }
    plan.push((key::LISTEN_ADDRESS, address.to_owned()));
    plan
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

    /// The order is the whole point: without auth already on, the CLI prompts
    /// for a username, finds no TTY, and silently leaves the address alone
    /// while printing "Config has been updated".
    #[test]
    fn leaving_loopback_enables_auth_first() {
        let plan = listen_address_plan("0.0.0.0", false);
        assert_eq!(
            plan,
            vec![
                (key::LISTEN_AUTH_ENABLED, "true".to_owned()),
                (key::LISTEN_ADDRESS, "0.0.0.0".to_owned()),
            ]
        );
    }

    #[test]
    fn auth_already_on_needs_no_extra_call() {
        assert_eq!(
            listen_address_plan("0.0.0.0", true),
            vec![(key::LISTEN_ADDRESS, "0.0.0.0".to_owned())]
        );
    }

    /// Returning to loopback must not switch authentication on as a
    /// side effect — the requirement is about exposure, not a ratchet.
    #[test]
    fn returning_to_loopback_leaves_auth_alone() {
        assert_eq!(
            listen_address_plan("127.0.0.1", false),
            vec![(key::LISTEN_ADDRESS, "127.0.0.1".to_owned())]
        );
    }
}
