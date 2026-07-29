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
