//! State types shared between the logic layer and the UI.

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

/// A row of the `filter` table in `agflm_standard.db` / `agflm_dns.db`.
///
/// This is the authoritative filter state — richer and safer than parsing
/// `adguard-cli filters list`, whose title column overflows for long names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub id: i64,
    pub group_id: i64,
    pub title: String,
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
}

/// A category of filters (`filter_group`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterGroup {
    pub id: i64,
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
