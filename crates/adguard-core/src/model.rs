//! State types shared between the logic layer and the UI.

use std::path::PathBuf;

use crate::paths;

/// Runtime state of the AdGuard proxy, as reported by `adguard-cli status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyStatus {
    pub running: bool,
    /// e.g. `127.0.0.1:3129`. `None` while stopped.
    pub http_proxy: Option<String>,
    /// e.g. `127.0.0.1:1081`. `None` while stopped.
    pub socks5_proxy: Option<String>,
    pub manual_dns_proxy: bool,
    pub system_wide_filtering: bool,
    pub system_dns_filtering: bool,
}

/// A switch on the Protection page.
///
/// Each variant names one boolean in `proxy.yaml`. [`Self::key`] is both the
/// dotted path used to read the file and the argument given to
/// `adguard-cli config set`, so the two directions cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    AdBlocking,
    HttpsFiltering,
    StealthMode,
    DnsFiltering,
    SafeBrowsing,
    Crlite,
}

impl Toggle {
    /// In the order the page renders them: what AdGuard does to traffic first,
    /// then the protections layered on top.
    pub const ALL: [Self; 6] = [
        Self::AdBlocking,
        Self::HttpsFiltering,
        Self::StealthMode,
        Self::DnsFiltering,
        Self::SafeBrowsing,
        Self::Crlite,
    ];

    pub fn key(self) -> &'static str {
        use crate::config::key;
        match self {
            Self::AdBlocking => key::AD_BLOCKING,
            Self::HttpsFiltering => key::HTTPS_FILTERING,
            Self::StealthMode => key::STEALTH_MODE,
            Self::DnsFiltering => key::DNS_FILTERING,
            Self::SafeBrowsing => key::SAFE_BROWSING,
            Self::Crlite => key::CRLITE,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::AdBlocking => "Ad blocking",
            Self::HttpsFiltering => "HTTPS filtering",
            Self::StealthMode => "Stealth mode",
            Self::DnsFiltering => "DNS filtering",
            Self::SafeBrowsing => "Safe Browsing",
            Self::Crlite => "Certificate revocation checks",
        }
    }

    /// Wording taken from the explanatory comments in `proxy.yaml` itself, so
    /// the GUI and a user reading the file are told the same thing.
    pub fn description(self) -> &'static str {
        match self {
            Self::AdBlocking => "Apply ad-blocking filters to requests",
            Self::HttpsFiltering => {
                "Decrypt HTTPS so filters can see inside it. Requires the AdGuard \
                 certificate to be installed"
            }
            Self::StealthMode => "Tracking protection: cookies, referrers, User-Agent and more",
            Self::DnsFiltering => "Filter DNS queries through the local DNS proxy",
            Self::SafeBrowsing => "Warn about malicious and phishing websites",
            Self::Crlite => "Check certificates against CRLite revocation lists",
        }
    }
}

/// What kind of control one Advanced setting needs, and the bounds the CLI
/// will not enforce for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Switch,

    /// An integer setting.
    ///
    /// `min`/`max` are **ours**, not the CLI's. Measured on v1.4.13,
    /// `config set` validates the *type* of an integer setting and nothing
    /// else: `listen_ports.http_proxy` accepts `0`, `65536`, `99999` and `-2`,
    /// and `worker_threads` accepts `0` and `-1`. `3.5` is accepted too and
    /// lands in the YAML as a **float**, where every subsequent integer read
    /// sees a value it cannot use. Only `abc` and the empty string are refused,
    /// with *"The value of the setting must be an integer"*.
    ///
    /// So range-checking is the GUI's job, exactly like the cross-setting
    /// dependencies in `docs/architecture.md` §5.
    Number {
        min: i64,
        max: i64,
        /// The value that means "switched off", if this setting has one —
        /// `-1` for both manual proxy ports, per the file's own comment
        /// (*"Use -1 to disable SOCKS5 manual proxy"*).
        disabled_value: Option<i64>,
    },

    Text {
        /// Render with a password entry and never echo the value — not in a
        /// toast, not in an error message.
        secret: bool,
    },

    /// One of a fixed set of strings.
    ///
    /// The CLI names the valid values in its refusal (*"Valid values are: info,
    /// debug, trace"*) and writes whatever spelling it was given straight into
    /// the file, so reads must be case-insensitive; see
    /// [`crate::config::Config::choice_at`].
    Choice { options: &'static [&'static str] },
}

/// One row of the Advanced page.
///
/// As with [`Toggle`], `key` is both the dotted path read from `proxy.yaml` and
/// the argument handed to `adguard-cli config set`, so the read and the write
/// cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub title: &'static str,
    /// Wording follows the explanatory comment in `proxy.yaml` where there is
    /// one, so the GUI and a user reading the file are told the same thing.
    pub description: &'static str,
    pub kind: Kind,
}

impl Setting {
    pub fn is_secret(self) -> bool {
        matches!(self.kind, Kind::Text { secret: true })
    }

    /// Is `value` inside the range this page is willing to write?
    ///
    /// A file value outside it is shown read-only, with the real number named,
    /// rather than clamped into range: clamping the *display* would misreport
    /// the file and invite the user to write the clamped value back by
    /// accident. Non-`Number` settings permit nothing.
    pub fn permits_number(self, value: i64) -> bool {
        match self.kind {
            Kind::Number { min, max, .. } => (min..=max).contains(&value),
            _ => false,
        }
    }

    /// The options of a [`Kind::Choice`], or `&[]`.
    pub fn options(self) -> &'static [&'static str] {
        match self.kind {
            Kind::Choice { options } => options,
            _ => &[],
        }
    }
}

/// A titled block of Advanced settings.
pub struct SettingGroup {
    pub title: &'static str,
    pub description: &'static str,
    pub settings: &'static [Setting],
}

/// Valid `log_level` values, from the file's comment and the CLI's own refusal
/// message (which lists them lowercase).
pub const LOG_LEVELS: &[&str] = &["info", "debug", "trace"];

/// Valid `outbound_proxy.mode` values. Spelled as the file's comment and its
/// default (`'HTTP'`) do; the CLI accepts either case and preserves what it is
/// given, so reads are case-insensitive.
pub const OUTBOUND_MODES: &[&str] = &["HTTP", "HTTPS", "SOCKS4", "SOCKS5"];

/// The Advanced page, in render order — `architecture.md` §5: ports, listen
/// address, auth, outbound proxy, worker threads, log level.
///
/// `listen_address` and `listen_auth.enabled` appear here for their wording and
/// their control type, but the page does **not** write them through the generic
/// path: both are gated by [`crate::config::listen_address_plan`], because
/// exposing the proxy beyond loopback has a precondition the CLI enforces by
/// silently doing nothing.
pub const ADVANCED: [SettingGroup; 4] = [
    SettingGroup {
        title: "Manual proxy ports",
        description: "Ports AdGuard listens on in manual proxy mode. \
                      Set a port to -1 to switch that protocol off.",
        settings: &[
            Setting {
                key: crate::config::key::LISTEN_PORT_HTTP,
                title: "HTTP proxy port",
                description: "Use -1 to disable HTTP manual proxy",
                kind: Kind::Number {
                    min: -1,
                    max: 65535,
                    disabled_value: Some(-1),
                },
            },
            Setting {
                key: crate::config::key::LISTEN_PORT_SOCKS5,
                title: "SOCKS5 proxy port",
                description: "Use -1 to disable SOCKS5 manual proxy",
                kind: Kind::Number {
                    min: -1,
                    max: 65535,
                    disabled_value: Some(-1),
                },
            },
        ],
    },
    SettingGroup {
        title: "Listen address",
        description: "Which addresses the proxy accepts connections on. \
                      Anything other than a loopback address exposes it to \
                      your network, which requires a username and password.",
        settings: &[
            Setting {
                key: crate::config::key::LISTEN_ADDRESS,
                title: "Listen address",
                description: "An IP address without a port. 127.0.0.1 keeps \
                              the proxy on this machine only",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::LISTEN_AUTH_ENABLED,
                title: "Require authentication",
                description: "Ask connecting clients for the username and password below",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::LISTEN_AUTH_USERNAME,
                title: "Username",
                description: "Username connecting clients must present",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::LISTEN_AUTH_PASSWORD,
                title: "Password",
                description: "Password connecting clients must present",
                kind: Kind::Text { secret: true },
            },
        ],
    },
    SettingGroup {
        title: "Outbound proxy",
        description: "Send AdGuard's own outgoing connections through another proxy.",
        settings: &[
            Setting {
                key: crate::config::key::OUTBOUND_ENABLED,
                title: "Use an outbound proxy",
                description: "Route filtered traffic onwards through the proxy below",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::OUTBOUND_MODE,
                title: "Protocol",
                description: "Supported modes are HTTP, HTTPS, SOCKS4, SOCKS5",
                kind: Kind::Choice {
                    options: OUTBOUND_MODES,
                },
            },
            Setting {
                key: crate::config::key::OUTBOUND_HOST,
                title: "Host",
                description: "Hostname or IP address of the outbound proxy",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::OUTBOUND_PORT,
                title: "Port",
                description: "Port of the outbound proxy",
                kind: Kind::Number {
                    min: 1,
                    max: 65535,
                    disabled_value: None,
                },
            },
            Setting {
                key: crate::config::key::OUTBOUND_USERNAME,
                title: "Username",
                description: "Leave empty if the outbound proxy needs no credentials",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::OUTBOUND_PASSWORD,
                title: "Password",
                description: "Password for the outbound proxy",
                kind: Kind::Text { secret: true },
            },
            Setting {
                key: crate::config::key::OUTBOUND_TRUST_ANY_CERT,
                title: "Trust any certificate",
                description: "Do not check certificate of HTTPS proxy",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::OUTBOUND_UDP_VIA_SOCKS5,
                title: "UDP through SOCKS5",
                description: "Use SOCKS5 proxy for UDP. If your SOCKS5 proxy does \
                              not support UDP, connection may break",
                kind: Kind::Switch,
            },
        ],
    },
    SettingGroup {
        title: "Diagnostics",
        description: "",
        settings: &[
            Setting {
                key: crate::config::key::WORKER_THREADS,
                title: "Worker threads",
                description: "Number of worker threads",
                kind: Kind::Number {
                    min: 1,
                    max: 64,
                    disabled_value: None,
                },
            },
            Setting {
                key: crate::config::key::LOG_LEVEL,
                title: "Log level",
                description: "Allowed log levels are: info, debug, trace",
                kind: Kind::Choice { options: LOG_LEVELS },
            },
        ],
    },
];

/// Which of the two filter catalogues an operation targets.
///
/// They are genuinely separate: different databases, and a `dns` prefix on
/// every CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterSet {
    /// HTTP/HTTPS content filters — `agflm_standard.db`, `adguard-cli filters`.
    Http,
    /// DNS filters — `agflm_dns.db`, `adguard-cli dns filters`.
    Dns,
}

impl FilterSet {
    /// Leading arguments for `adguard-cli`.
    pub fn cli_prefix(self) -> &'static [&'static str] {
        match self {
            Self::Http => &["filters"],
            Self::Dns => &["dns", "filters"],
        }
    }

    /// The SQLite catalogue backing this set. Open read-only.
    pub fn db_path(self) -> Option<PathBuf> {
        match self {
            Self::Http => paths::filters_db(),
            Self::Dns => paths::dns_filters_db(),
        }
    }

    /// The file behind this set's user-rules pseudo-filter.
    pub fn user_rules_file(self) -> Option<PathBuf> {
        match self {
            Self::Http => paths::user_rules_file(),
            Self::Dns => paths::dns_user_rules_file(),
        }
    }
}

/// What a switch flip has to ask the CLI to do.
///
/// Three actions rather than two because `enable` is not enough: see
/// [`Filter::action_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    /// `filters add` — subscribes to a filter *and* enables it in one step.
    Add,
    Enable,
    Disable,
}

impl FilterAction {
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }

    /// The word the CLI echoes back on success, as in
    /// `Filter [Title: AdGuard Base filter] enabled`.
    pub fn confirmation(self) -> &'static str {
        match self {
            Self::Add => "added",
            Self::Enable => "enabled",
            Self::Disable => "disabled",
        }
    }
}

/// A row of the `filter` table in `agflm_standard.db` / `agflm_dns.db`.
///
/// This is the authoritative filter state — richer and safer than parsing
/// `adguard-cli filters list`, whose title column overflows for long names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub id: i64,
    pub group_id: i64,
    /// Localised display name, already fallen back to [`Self::title`] when the
    /// active language has no `filter_localisation` row.
    pub name: String,
    /// The raw English `filter.title` column. Kept because it is what the CLI
    /// echoes in its own confirmation messages, which makes it the useful
    /// value in logs and errors.
    pub title: String,
    /// Localised description; empty when the catalogue has none.
    pub description: String,
    pub homepage: Option<String>,
    pub enabled: bool,
    pub installed: bool,
    pub trusted: bool,
}

impl Filter {
    /// The pseudo-filter standing in for the user's own rules (`user.txt`,
    /// `dns_user.txt`).
    ///
    /// It is **not** a subscribable list: it has an empty `download_url`, sits
    /// in a `group_id` of 0 that does not exist in `filter_group`, and may be
    /// `is_enabled` while not `is_installed` (observed in `agflm_dns.db`).
    /// [`crate::filters::Catalogue::filters`] excludes it; use
    /// [`crate::filters::Catalogue::user_rules`] to reach it.
    pub const USER_RULES_ID: i64 = i32::MIN as i64;

    pub fn is_user_rules(&self) -> bool {
        self.id == Self::USER_RULES_ID
    }

    /// The action needed to move this filter to `on`.
    ///
    /// Turning a switch on is **not** always `enable`. Measured on v1.4.13:
    ///
    /// - `filters enable <id>` on a filter that was never added is a semantic
    ///   failure — *"Before filters can be enabled, they must be added"*,
    ///   printed to stdout with exit code 0, changing nothing.
    /// - `filters add <id>` adds *and* enables, printing both confirmations.
    ///
    /// So an uninstalled filter needs `add`. Turning a switch off is always
    /// `disable`, which leaves `is_installed` set — removing a filter
    /// altogether is a separate, destructive action.
    ///
    /// The user-rules pseudo-filter is never `add`ed: it is not a list, and in
    /// `agflm_dns.db` it is enabled while `is_installed` is 0, which would
    /// otherwise send us down the `add` path.
    pub fn action_for(&self, on: bool) -> FilterAction {
        match (on, self.installed || self.is_user_rules()) {
            (false, _) => FilterAction::Disable,
            (true, true) => FilterAction::Enable,
            (true, false) => FilterAction::Add,
        }
    }
}

/// The two flags a mutation is verified against, re-read on their own so
/// confirming one toggle does not cost a whole catalogue read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterState {
    pub enabled: bool,
    pub installed: bool,
}

/// A category of filters (`filter_group`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterGroup {
    pub id: i64,
    /// Localised name, falling back to the English `filter_group.name`.
    pub name: String,
    pub display_number: i64,
}

impl FilterGroup {
    /// The "Custom filters" group, holding lists the user installed by URL.
    /// Unlike the user-rules pseudo-filter, this group is real and present in
    /// `filter_group`.
    pub const CUSTOM_ID: i64 = i32::MIN as i64;

    pub fn is_custom(&self) -> bool {
        self.id == Self::CUSTOM_ID
    }
}

/// One complete read of a filter catalogue — everything the Filters page
/// renders, from a single point in time.
#[derive(Debug, Clone, Default)]
pub struct FilterCatalogue {
    /// Categories in AdGuard's own display order.
    pub groups: Vec<FilterGroup>,
    /// The subscribable catalogue, excluding the user-rules pseudo-filter.
    pub filters: Vec<Filter>,
    /// The user's own rules, which belong in the UI as their own toggle.
    pub user_rules: Option<Filter>,
}

impl FilterCatalogue {
    /// Groups paired with their filters, in display order.
    ///
    /// Empty groups are dropped: "Custom filters" has no members until the
    /// user installs a list by URL, and an empty `AdwPreferencesGroup` renders
    /// as a stray heading with nothing under it.
    pub fn grouped(&self) -> Vec<(&FilterGroup, Vec<&Filter>)> {
        self.groups
            .iter()
            .filter_map(|group| {
                let filters: Vec<&Filter> = self
                    .filters
                    .iter()
                    .filter(|filter| filter.group_id == group.id)
                    .collect();
                (!filters.is_empty()).then_some((group, filters))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(id: i64, enabled: bool, installed: bool) -> Filter {
        Filter {
            id,
            group_id: 1,
            name: "Test".to_owned(),
            title: "Test".to_owned(),
            description: String::new(),
            homepage: None,
            enabled,
            installed,
            trusted: false,
        }
    }

    /// Enabling something never added fails at exit 0 and changes nothing, so
    /// the switch must reach for `add` instead.
    #[test]
    fn uninstalled_filter_is_added_not_enabled() {
        let f = filter(3, false, false);
        assert_eq!(f.action_for(true), FilterAction::Add);
    }

    #[test]
    fn installed_filter_is_enabled() {
        let f = filter(3, false, true);
        assert_eq!(f.action_for(true), FilterAction::Enable);
    }

    /// Off is always `disable` — never `remove`, which would unsubscribe.
    #[test]
    fn switching_off_disables_and_keeps_the_subscription() {
        assert_eq!(filter(3, true, true).action_for(false), FilterAction::Disable);
        assert_eq!(
            filter(Filter::USER_RULES_ID, true, true).action_for(false),
            FilterAction::Disable
        );
    }

    /// The DNS catalogue's user-rules row is enabled while `is_installed` is 0.
    /// Without the pseudo-filter exception that combination would ask the CLI
    /// to `add` the user's own rules as if they were a subscribable list.
    #[test]
    fn user_rules_are_never_added() {
        let user_rules = filter(Filter::USER_RULES_ID, true, false);
        assert_eq!(user_rules.action_for(true), FilterAction::Enable);
    }

    #[test]
    fn dns_commands_carry_the_dns_prefix() {
        assert_eq!(FilterSet::Http.cli_prefix(), &["filters"]);
        assert_eq!(FilterSet::Dns.cli_prefix(), &["dns", "filters"]);
    }

    fn advanced_settings() -> Vec<Setting> {
        ADVANCED
            .iter()
            .flat_map(|group| group.settings.iter().copied())
            .collect()
    }

    /// One row per key. A duplicate would give the page two controls writing
    /// the same setting, each reconciling over the other.
    #[test]
    fn advanced_keys_are_unique() {
        let mut keys: Vec<&str> = advanced_settings().iter().map(|s| s.key).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate key in ADVANCED");
    }

    /// The page's scope, from `architecture.md` §5.
    #[test]
    fn advanced_covers_the_documented_scope() {
        use crate::config::key;
        let keys: Vec<&str> = advanced_settings().iter().map(|s| s.key).collect();
        for expected in [
            key::LISTEN_PORT_HTTP,
            key::LISTEN_PORT_SOCKS5,
            key::LISTEN_ADDRESS,
            key::LISTEN_AUTH_ENABLED,
            key::LISTEN_AUTH_USERNAME,
            key::LISTEN_AUTH_PASSWORD,
            key::OUTBOUND_ENABLED,
            key::OUTBOUND_MODE,
            key::OUTBOUND_HOST,
            key::OUTBOUND_PORT,
            key::WORKER_THREADS,
            key::LOG_LEVEL,
        ] {
            assert!(keys.contains(&expected), "{expected} is missing from ADVANCED");
        }
    }

    /// Both passwords must be marked secret, and nothing else should be. The
    /// flag is what keeps the value out of entry rendering and error messages.
    #[test]
    fn exactly_the_passwords_are_secret() {
        use crate::config::key;
        let secret: Vec<&str> = advanced_settings()
            .iter()
            .filter(|s| s.is_secret())
            .map(|s| s.key)
            .collect();
        assert_eq!(
            secret,
            vec![key::LISTEN_AUTH_PASSWORD, key::OUTBOUND_PASSWORD]
        );
    }

    /// The CLI range-checks nothing, so these bounds are the only thing between
    /// a spin row and `http_proxy: 99999`.
    #[test]
    fn number_ranges_are_inclusive_and_reject_the_cli_accepted_junk() {
        use crate::config::key;
        let find = |key: &str| {
            advanced_settings()
                .into_iter()
                .find(|s| s.key == key)
                .expect("setting should exist")
        };

        let http = find(key::LISTEN_PORT_HTTP);
        assert!(http.permits_number(-1), "-1 disables the port");
        assert!(http.permits_number(3129));
        assert!(http.permits_number(65535));
        // All four of these are accepted by `config set`.
        assert!(!http.permits_number(65536));
        assert!(!http.permits_number(99999));
        assert!(!http.permits_number(-2));

        let threads = find(key::WORKER_THREADS);
        assert!(threads.permits_number(1));
        assert!(threads.permits_number(4));
        assert!(!threads.permits_number(0), "a proxy with no workers is not a setting we offer");
        assert!(!threads.permits_number(-1));

        // An outbound port has no "off" value — the switch does that.
        let outbound = find(key::OUTBOUND_PORT);
        assert!(!outbound.permits_number(-1));
        assert!(!outbound.permits_number(0));
        assert!(outbound.permits_number(1));
    }

    /// A `Switch` or `Text` setting has no numeric range, so it must not
    /// accidentally report one.
    #[test]
    fn non_numeric_settings_permit_no_numbers() {
        use crate::config::key;
        let listen = advanced_settings()
            .into_iter()
            .find(|s| s.key == key::LISTEN_ADDRESS)
            .unwrap();
        assert!(!listen.permits_number(0));
        assert!(listen.options().is_empty());
    }

    /// The values the file actually ships with have to be in the option lists,
    /// or the combo row would open on nothing and the row read as unavailable.
    #[test]
    fn shipped_defaults_are_among_the_options() {
        assert!(LOG_LEVELS.contains(&"info"));
        assert!(OUTBOUND_MODES.contains(&"HTTP"));
        for setting in advanced_settings() {
            if let Kind::Choice { options } = setting.kind {
                assert!(!options.is_empty(), "{} has no options", setting.key);
            }
        }
    }

    #[test]
    fn grouped_skips_groups_with_no_filters() {
        let catalogue = FilterCatalogue {
            groups: vec![
                FilterGroup {
                    id: FilterGroup::CUSTOM_ID,
                    name: "Custom filters".to_owned(),
                    display_number: 0,
                },
                FilterGroup {
                    id: 1,
                    name: "Ad blocking".to_owned(),
                    display_number: 1,
                },
            ],
            filters: vec![filter(2, true, true)],
            user_rules: None,
        };

        let grouped = catalogue.grouped();
        assert_eq!(grouped.len(), 1, "empty group should be dropped");
        assert_eq!(grouped[0].0.id, 1);
        assert_eq!(grouped[0].1.len(), 1);
    }
}
