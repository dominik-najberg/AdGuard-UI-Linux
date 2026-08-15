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

/// A successful reading of `adguard-cli license`.
///
/// Measured on v1.4.13 against the licensed install on this machine: three
/// lines on stdout, exit 0, nothing on stderr, and — unusually for this CLI —
/// no ANSI escapes at all.
///
/// ```text
/// License owner: someone@example.com
/// License key: XXXXXXXXXXXXXXXX
/// License status: APP_ACTIVE
/// ```
///
/// # Every field here is sensitive
///
/// The key is a secret and the owner is personal data, which makes this the one
/// state type in this crate that must not be printed whole. [`Self::masked_key`]
/// is the only sanctioned way to show the key, and the `Debug` implementation
/// below is hand-written so that a `{:?}` in a log line, a test failure or an
/// error path cannot leak either of them (`architecture.md` §8).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct License {
    /// The account the licence belongs to — an e-mail address.
    pub owner: String,
    /// The licence key, in full. Show it through [`Self::masked_key`].
    pub key: String,
    /// The CLI's own status word, e.g. `APP_ACTIVE`. Kept verbatim rather than
    /// mapped to a boolean: a status we do not recognise should render as
    /// itself, not as "inactive".
    pub status: String,
}

impl License {
    /// The status word of a licence that is working.
    pub const ACTIVE: &'static str = "APP_ACTIVE";

    /// How much of the key [`Self::masked_key`] leaves visible —
    /// `architecture.md` §5: "mask the key to its last four characters".
    const VISIBLE: usize = 4;

    /// Is this licence actually working?
    ///
    /// Compared case-insensitively for the same reason config values are: the
    /// spelling is the CLI's, and matching it exactly would turn a cosmetic
    /// upstream change into a user being told their licence is dead.
    pub fn is_active(&self) -> bool {
        self.status.eq_ignore_ascii_case(Self::ACTIVE)
    }

    /// The key with everything but its last four characters replaced.
    ///
    /// Enough to tell two licences apart or to read down the phone to support,
    /// and not enough to use. A key of four characters or fewer is masked
    /// **entirely** — the rule is "the last four", not "at least the last
    /// four", and a short value is far more likely to be junk than a key worth
    /// revealing.
    ///
    /// Length is preserved, which says how long the key is. That is not a
    /// secret: every key AdGuard issues is sixteen characters, measured.
    pub fn masked_key(&self) -> String {
        let count = self.key.chars().count();
        let visible = if count <= Self::VISIBLE { 0 } else { Self::VISIBLE };
        let mut masked: String = "•".repeat(count - visible);
        masked.extend(self.key.chars().skip(count - visible));
        masked
    }
}

/// Hand-written so the key and the owner cannot ride out on a `{:?}`.
///
/// Both of the derived alternative's escape routes are real: `Error` values are
/// printed with `{err:?}` all over the test suites, and a `dbg!` left in a page
/// would put a licence key on stderr. Masking here means the leak has to be
/// written deliberately, by naming the field.
impl std::fmt::Debug for License {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("License")
            .field("owner", &"<hidden>")
            .field("key", &self.masked_key())
            .field("status", &self.status)
            .finish()
    }
}

/// What `adguard-cli check-update` said about one component.
///
/// Measured on v1.4.13 over fourteen runs (contract §14): the command answers in
/// pairs, a `Checking <name> updates...` line and a verdict on the next.
///
/// **`said` is not decoration.** `Failed to update filters` is the sentence for
/// a failure of the HTTP filters *and* for a failure of the DNS filters — the
/// same string, naming neither — so the verdict line alone cannot say which
/// component it belongs to. Only the header can, which is why the two are
/// carried together in one value and never separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentUpdate {
    /// Which component, read from the header.
    pub part: UpdatePart,
    /// What the verdict amounts to, for the two decisions that turn on it: what
    /// to re-read, and what to draw attention to.
    pub verdict: Verdict,
    /// AdGuard's own sentence, verbatim. What the UI shows — better wording than
    /// ours, and it stays right when the CLI is reworded.
    pub said: String,
}

/// One of the six things `check-update` covers.
///
/// [`Self::Other`] is not defensive clutter: the six are a fixed list on 1.4.13
/// and a seventh would otherwise be silently dropped from a report the user is
/// reading as complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePart {
    /// HTTP/HTTPS filter lists — the Filters page's catalogue.
    Filters,
    /// DNS filter lists — the DNS page's catalogue.
    DnsFilters,
    Userscripts,
    SafeBrowsing,
    CrLite,
    /// The application itself. The only one `check-update` checks rather than
    /// updates.
    App,
    /// A component this build does not know, carrying the header's own name.
    Other(String),
}

impl UpdatePart {
    /// The name as the CLI prints it between `Checking ` and ` updates...`.
    ///
    /// Matched case-insensitively for the reason [`License::is_active`] is: the
    /// spelling belongs to AdGuard, and a cosmetic change upstream should not
    /// turn a component into an unknown one.
    pub fn from_header(name: &str) -> Self {
        let name = name.trim();
        for known in [
            Self::Filters,
            Self::DnsFilters,
            Self::Userscripts,
            Self::SafeBrowsing,
            Self::CrLite,
            Self::App,
        ] {
            if name.eq_ignore_ascii_case(known.header()) {
                return known;
            }
        }
        Self::Other(name.to_owned())
    }

    /// The header spelling, exactly as measured.
    pub fn header(&self) -> &str {
        match self {
            Self::Filters => "filters",
            Self::DnsFilters => "DNS filters",
            Self::Userscripts => "userscripts",
            Self::SafeBrowsing => "SafebrowsingV2",
            Self::CrLite => "CRLite",
            Self::App => "app",
            Self::Other(name) => name,
        }
    }

    /// What to call it on screen.
    ///
    /// Not the header: `SafebrowsingV2` is an internal spelling and `app` is
    /// ambiguous in an application that has a version of its own to show beside
    /// it. An unknown component is shown under the CLI's own name, because
    /// inventing one would be worse than repeating theirs.
    pub fn title(&self) -> &str {
        match self {
            Self::Filters => "Filter lists",
            Self::DnsFilters => "DNS filter lists",
            Self::Userscripts => "Userscripts",
            Self::SafeBrowsing => "Safe Browsing",
            Self::CrLite => "Certificate revocation (CRLite)",
            Self::App => "AdGuard CLI",
            Self::Other(name) => name,
        }
    }
}

/// What one verdict line amounts to.
///
/// Deliberately coarse. The sentence itself is kept in
/// [`ComponentUpdate::said`] and is what the user reads; this exists only for
/// the two decisions code has to make — whether to re-read a catalogue, and
/// whether to draw attention to a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// `Up to date`.
    UpToDate,
    /// `Updated`, or `N filter(s) updated`.
    Changed,
    /// `Failed to update filters`, whichever component it was about.
    Failed,
    /// A shape nobody has measured, including a header the CLI never answered.
    ///
    /// Not an error and not silently discarded: shown as itself. **The
    /// application line reaches the user through here by design** — what
    /// `check-update` says when a newer AdGuard CLI exists has never been
    /// observed (contract §14), so anything that is not `Up to date` is
    /// repeated verbatim rather than interpreted.
    Unrecognised,
}

impl Verdict {
    /// `Up to date`, the one verdict that means nothing happened.
    const UP_TO_DATE: &'static str = "Up to date";
    /// Every failure seen opens with this.
    const FAILED: &'static str = "Failed";
    /// `Updated` and `N filter(s) updated` both end here.
    const UPDATED: &'static str = "updated";

    /// Read one verdict line.
    ///
    /// **Failure is tested first, and the order is load-bearing.** Both
    /// remaining rules are suffix and equality matches over sentences AdGuard
    /// may reword, and of the two ways to be wrong about a reworded one, only
    /// reading a failure as a success loses information the user needed. A
    /// success read as a failure is a visible, checkable complaint.
    ///
    /// Nothing here consults the exit status, which is 0 for a failed component
    /// as reliably as for a successful one (contract §14).
    pub fn classify(said: &str) -> Self {
        let said = said.trim();
        if said.starts_with(Self::FAILED) {
            Self::Failed
        } else if said.eq_ignore_ascii_case(Self::UP_TO_DATE) {
            Self::UpToDate
        } else if said.to_ascii_lowercase().ends_with(Self::UPDATED) {
            // Covers the bare `Updated` and both counted forms. A count of zero
            // has never been seen — the CLI says `Up to date` instead — so it is
            // not special-cased here; it would report a change and cost one
            // redundant catalogue re-read, which is the harmless direction.
            Self::Changed
        } else {
            Self::Unrecognised
        }
    }
}

/// A whole reading of `adguard-cli check-update`.
///
/// Holds the components in the order the CLI listed them, which is the order
/// they are shown in: it is AdGuard's account of its own run, and reordering it
/// would be this application editing a report it did not write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub components: Vec<ComponentUpdate>,
}

impl UpdateReport {
    /// What the CLI said about one component, if it mentioned it.
    pub fn part(&self, part: &UpdatePart) -> Option<&ComponentUpdate> {
        self.components.iter().find(|component| &component.part == part)
    }

    /// Did this component actually change? Drives the catalogue re-reads.
    ///
    /// A component the CLI did not mention answers `false`: a page is re-read
    /// because something was said to have moved, never because nothing was said.
    pub fn changed(&self, part: &UpdatePart) -> bool {
        self.part(part).is_some_and(|component| component.verdict == Verdict::Changed)
    }

    /// Every component that failed, in report order.
    ///
    /// Failures are an ordinary outcome here rather than an exception — five of
    /// the fourteen measured runs carried one, and in every case the next run of
    /// that component succeeded (contract §14). So they are listed, not raised.
    pub fn failures(&self) -> impl Iterator<Item = &ComponentUpdate> {
        self.components.iter().filter(|component| component.verdict == Verdict::Failed)
    }

    /// AdGuard's sentence about the application, when it amounts to news.
    ///
    /// `None` is the ordinary case and means there is nothing to say. `Some` is
    /// the case this project has never seen and will not fabricate: the sentence
    /// is shown as it arrived, with the `adguard-cli update` command named
    /// beside it, and nothing here runs that command — see contract §14 and
    /// `architecture.md` §6.
    ///
    /// **Two shapes are held back, and neither is the unmeasured one.**
    ///
    /// A [`Verdict::Failed`] app line is a failed *check*, not a release: it is
    /// already reported by [`Self::failures`], and passing it through here as
    /// well would show one event twice — the second time as an update notice
    /// recommending a command, which is advice derived from a check that did
    /// not finish.
    ///
    /// An empty sentence is a header the CLI announced and never answered. That
    /// arrives as [`Verdict::Unrecognised`] with nothing in it, and a notice
    /// carrying no words is a row that says only that something is wrong.
    /// [`Self::failures`] does not catch this one either, so without the guard
    /// it would reach the page as `Some("")`.
    pub fn app_notice(&self) -> Option<&str> {
        self.part(&UpdatePart::App)
            .filter(|component| !matches!(component.verdict, Verdict::UpToDate | Verdict::Failed))
            .map(|component| component.said.trim())
            .filter(|said| !said.is_empty())
    }
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

    /// May this setting legitimately hold nothing at all?
    ///
    /// `Kind::Text` has no room for the difference between *empty*, *absent*
    /// and *unreadable*, and for nine of the ten text rows it does not need
    /// one: a credential or a hostname holds the empty string, which `str_at`
    /// returns as `Some("")`, and a `None` there means a key the page genuinely
    /// cannot read. `outbound_interface` is the exception measured on 2 August
    /// 2026 — the only null-valued scalar in `proxy.yaml`, where null is the
    /// **shipped** state and means "the system chooses the interface".
    ///
    /// Without this, that row renders as *unavailable* on an install nobody has
    /// touched, and `every_advanced_setting_resolves_with_the_right_type` fails
    /// against the real config. That assertion is the guard working, so the fix
    /// is to teach it the distinction rather than to loosen it: `str_at` is
    /// unchanged and [`crate::Config::resolves`] consults this.
    ///
    /// A method over the key, for the same reason [`Self::requires`] is one —
    /// one setting in forty declares this, and one place listing it reads
    /// better than thirty-nine `may_be_absent: false`.
    pub fn may_be_absent(self) -> bool {
        self.key == crate::config::key::OUTBOUND_INTERFACE
    }

    /// The setting that must be **on** for this one to do anything.
    ///
    /// `proxy.yaml` states these dependencies in its comments and nothing
    /// enforces them. Measured: `config set
    /// https_filtering.encrypted_client_hello true` succeeds and prints
    /// `Config has been updated` with `dns_filtering.enabled = false`, leaving
    /// a setting that reads "on" and does nothing. `architecture.md` §5 makes
    /// that the GUI's problem — the same reason the Protection page marks DNS
    /// filtering that is switched on but inert.
    ///
    /// A method over the key rather than a field on every literal: two of the
    /// forty-odd settings have a dependency, and one place listing them reads
    /// better than forty-two `requires: None`.
    pub fn requires(self) -> Option<&'static str> {
        match self.key {
            crate::config::key::HTTPS_ECH | crate::config::key::FILTER_SECURE_DNS_MODE => {
                Some(crate::config::key::DNS_FILTERING)
            }
            _ => None,
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

/// Valid `https_filtering.filter_secure_dns_mode` values, from the CLI's own
/// refusal: *"Valid values are: off, transparent, redirect"*.
pub const SECURE_DNS_MODES: &[&str] = &["off", "transparent", "redirect"];

/// Valid `proxy_mode` values, from the CLI's own refusal: *"Valid values are:
/// manual, auto"* (contract §8).
///
/// `auto` needs AdGuard's root helper set up, and **the CLI does not check that
/// when the value is written** — measured, `config set proxy_mode auto`
/// succeeds with all three properties unmet and the file really holds `auto`
/// afterwards. So the check belongs to the GUI, and the unmet state has to be
/// renderable as well as preventable: a terminal can put the file into it.
pub const PROXY_MODES: &[&str] = &["manual", "auto"];

/// The Stealth page — the ~26 settings behind the single `stealthmode.enabled`
/// switch the Protection page shows.
///
/// Its own page rather than a group on Protection (handoff §3 gap 4): six
/// groups of them would bury the five other protections beside which that
/// switch belongs.
///
/// Every key here, including the nested `anti_dpi` ones, was measured readable
/// with `config get` and writable with `config set` on v1.4.13 — nesting costs
/// the dotted path nothing. **The master switch is deliberately absent**: it
/// lives on Protection, and two pages writing one key is the arrangement
/// merging the tray into the GUI process existed to end.
pub const STEALTH: [SettingGroup; 5] = [
    SettingGroup {
        title: "Cookies",
        description: "Stealth mode must be on for any of this to apply. Times are in minutes; 0 blocks the cookie outright rather than expiring it.",
        settings: &[
            Setting {
                key: crate::config::key::SM_THIRD_PARTY_COOKIES,
                title: "Block third-party cookies",
                description: "Deletes third-party cookies after a set time",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_THIRD_PARTY_COOKIES_MIN,
                title: "Third-party cookie lifetime",
                description: "Minutes before deletion; 0 blocks them completely",
                kind: Kind::Number { min: 0, max: 525600, disabled_value: None },
            },
            Setting {
                key: crate::config::key::SM_FIRST_PARTY_COOKIES,
                title: "Block first-party cookies",
                description: "Deletes all cookies after a set time",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_FIRST_PARTY_COOKIES_MIN,
                title: "First-party cookie lifetime",
                description: "Minutes before deletion; 0 blocks them completely",
                kind: Kind::Number { min: 0, max: 525600, disabled_value: None },
            },
        ],
    },
    SettingGroup {
        title: "Tracking",
        description: "",
        settings: &[
            Setting {
                key: crate::config::key::SM_DISABLE_THIRD_PARTY_CACHE,
                title: "Disable third-party cache",
                description: "Prevents tracking by blocking ETag caching for third-party content",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_REMOVE_X_CLIENT_DATA,
                title: "Remove X-Client-Data header",
                description: "Strips the Chrome header that identifies your browser build to Google services",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_THIRD_PARTY_AUTH,
                title: "Block third-party Authorization",
                description: "Blocks the Authorization header in third-party requests to prevent tracking",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_DO_NOT_TRACK,
                title: "Send Do Not Track signals",
                description: "Send \"Do not track\" signals",
                kind: Kind::Switch,
            },
        ],
    },
    SettingGroup {
        title: "Identity",
        description: "An empty custom value means the header is reduced rather than replaced — for the referrer, changed to the origin.",
        settings: &[
            Setting {
                key: crate::config::key::SM_HIDE_IP,
                title: "Hide IP address",
                description: "Adds an X-Forwarded-For header. Deprecated: sites usually no longer honour it",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_CUSTOM_IP,
                title: "Custom IP",
                description: "The address sent in X-Forwarded-For",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::SM_HIDE_USER_AGENT,
                title: "Hide User-Agent",
                description: "Replaces or reduces the User-Agent header",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_CUSTOM_USER_AGENT,
                title: "Custom User-Agent",
                description: "Empty means the User-Agent is reduced: extra information is removed",
                kind: Kind::Text { secret: false },
            },
            Setting {
                key: crate::config::key::SM_HIDE_SEARCH_QUERIES,
                title: "Hide search queries",
                description: "Hides the referrer URL when navigating from a search engine",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_REMOVE_REFERRER,
                title: "Remove third-party referrer",
                description: "Hides the referrer URL in third-party requests",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_CUSTOM_REFERRER,
                title: "Custom referrer",
                description: "Used by both referrer settings. Empty means the referrer becomes the origin",
                kind: Kind::Text { secret: false },
            },
        ],
    },
    SettingGroup {
        title: "Browser APIs",
        description: "",
        settings: &[
            Setting {
                key: crate::config::key::SM_BLOCK_WEB_RTC,
                title: "Block WebRTC",
                description: "Prevents IP leaks via WebRTC; may disrupt certain browser applications",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_BLOCK_PUSH_API,
                title: "Block push notifications",
                description: "Blocks browser push notifications from websites even when the browser is inactive",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_BLOCK_LOCATION_API,
                title: "Block location access",
                description: "Prevents the browser from sharing GPS data, protecting location privacy",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_BLOCK_FLASH,
                title: "Block Flash",
                description: "Blocks the Flash Player plugin to reduce security vulnerabilities and load times",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_BLOCK_JAVA,
                title: "Block Java",
                description: "Disables Java plugins to prevent security risks; JavaScript remains enabled",
                kind: Kind::Switch,
            },
        ],
    },
    SettingGroup {
        title: "Anti-DPI",
        description: "Alters outgoing packet data to bypass DPI-based content filters and restrictions. A fragment size of 0 disables that split.",
        settings: &[
            Setting {
                key: crate::config::key::SM_DPI_ENABLED,
                title: "Protect from DPI",
                description: "Enables the packet alterations below",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_DPI_CLIENT_HELLO_FRAGMENT,
                title: "ClientHello fragment size",
                description: "Size of the first fragment when splitting ClientHello; 0 to disable",
                kind: Kind::Number { min: 0, max: 1500, disabled_value: Some(0) },
            },
            Setting {
                key: crate::config::key::SM_DPI_HTTP_FRAGMENT,
                title: "HTTP fragment size",
                description: "Size of the first fragment when splitting a plain HTTP request; 0 to disable",
                kind: Kind::Number { min: 0, max: 1500, disabled_value: Some(0) },
            },
            Setting {
                key: crate::config::key::SM_DPI_SPLIT_DELAY,
                title: "Split delay",
                description: "Milliseconds between the two fragments of a split request",
                kind: Kind::Number { min: 0, max: 60000, disabled_value: None },
            },
            Setting {
                key: crate::config::key::SM_DPI_SPACE_JUGGLING,
                title: "HTTP space juggling",
                description: "Swaps some spaces in plain HTTP requests to trick DPI",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::SM_DPI_FIRST_PACKET,
                title: "Increase first packet size",
                description: "Pads ClientHello or the first plain HTTP request across several packets",
                kind: Kind::Switch,
            },
        ],
    },
];

/// The Advanced page, in render order — `architecture.md` §5: proxy mode, HTTPS
/// filtering, secure DNS filtering, ports, listen address, auth, outbound proxy,
/// worker threads, log level.
///
/// The *HTTPS filtering* group is the parity enumeration's first slice
/// (`architecture.md` §5, *What the pages do not render*): five booleans that
/// were in `proxy.yaml` and on no page. It sits above *Secure DNS filtering*
/// because that group is a specialisation of it — both are `https_filtering.*`
/// keys, and the narrower one reads better after the general one.
///
/// `listen_address` and `listen_auth.enabled` appear here for their wording and
/// their control type, but the page does **not** write them through the generic
/// path: both are gated by [`crate::config::listen_address_plan`], because
/// exposing the proxy beyond loopback has a precondition the CLI enforces by
/// silently doing nothing.
///
/// **This block used to sit above `STEALTH`**, with no blank line between the
/// two runs of `///`, so all of it documented the Stealth table and `ADVANCED`
/// had no documentation at all — `cargo doc` is what proves it, not reading.
/// A blank line does *not* separate two `///` runs; only an intervening item
/// does. Moved here 2 August 2026.
pub const ADVANCED: [SettingGroup; 10] = [
    SettingGroup {
        title: "Proxy mode",
        description: "Manual mode listens on the ports below and leaves it to you \
                      to point applications at them. Automatic mode redirects \
                      traffic system-wide, which needs AdGuard's root helper set \
                      up first.",
        settings: &[Setting {
            key: crate::config::key::PROXY_MODE,
            title: "Proxy mode",
            description: "How traffic reaches the proxy",
            kind: Kind::Choice {
                options: PROXY_MODES,
            },
        }],
    },
    SettingGroup {
        title: "HTTPS filtering",
        description: "HTTPS filtering must be on for any of this to apply — the \
                      switch is on the Protection page. These change how AdGuard \
                      treats certificates and protocols once it is.",
        settings: &[
            Setting {
                key: crate::config::key::HTTPS_FILTER_EV,
                title: "Filter EV certificate sites",
                description: "By default AdGuard does not filter sites with EV \
                              certificates; this enables it",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::HTTPS_TLS13,
                title: "TLS 1.3 support",
                description: "Enable TLS1.3 support",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::HTTPS_OCSP,
                title: "OCSP checks",
                description: "Enable OCSP checks for domains",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::HTTPS_CERT_TRANSPARENCY,
                title: "Certificate Transparency",
                description: "Enforce Certificate Transparency Timestamps checks, \
                              like Chrome does",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::HTTPS_HTTP3,
                title: "Filter HTTP/3",
                description: "Filter HTTP/3 (experimental)",
                kind: Kind::Switch,
            },
        ],
    },
    SettingGroup {
        title: "Secure DNS filtering",
        description: "Both need DNS filtering switched on to have any effect, and \
                      the CLI will not stop you setting them without it.",
        settings: &[
            Setting {
                key: crate::config::key::FILTER_SECURE_DNS_MODE,
                title: "Secure DNS filtering mode",
                description: "Filters DoH/DoT requests through the local DNS proxy. \
                              'transparent' filters inline; 'redirect' forces the \
                              configured upstream",
                kind: Kind::Choice {
                    options: SECURE_DNS_MODES,
                },
            },
            Setting {
                key: crate::config::key::HTTPS_ECH,
                title: "Encrypted Client Hello",
                description: "Enables ECH for better privacy",
                kind: Kind::Switch,
            },
        ],
    },
    SettingGroup {
        title: "Filtered ports",
        description: "Ports AdGuard redirects into itself in automatic proxy \
                      mode. Traffic to any other port reaches the network \
                      without being filtered.",
        settings: &[Setting {
            key: crate::config::key::FILTERED_PORTS,
            title: "Filtered ports",
            description: "Single ports and low:high ranges, separated by commas \
                          — 80,443,8080 or 80:5221,5300:49151",
            kind: Kind::Text { secret: false },
        }],
    },
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
        title: "Outgoing connections",
        description: "Which network interface AdGuard's own outgoing connections \
                      leave from. Leave it empty to let the system choose, which \
                      is how it ships.",
        settings: &[Setting {
            key: crate::config::key::OUTBOUND_INTERFACE,
            title: "Bind to interface",
            description: "A name as `ip link` reports it, such as eth0 or wlan0. \
                          AdGuard does not check that it exists",
            kind: Kind::Text { secret: false },
        }],
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
            Setting {
                key: crate::config::key::ADGUARD_HEADERS,
                title: "Tag filtered responses",
                description: "Adds X-Adguard-Filtered and X-Adguard-Rule to responses on \
                              their way to your browser, naming the rule that matched. The \
                              sites you visit never see them",
                kind: Kind::Switch,
            },
        ],
    },
    // Its own group rather than two more rows in Diagnostics, because the
    // description below is the whole point of the feature and would be false
    // if it sat over "Worker threads". `location` declares no `requires()`:
    // its dependency is the switch above it, in its own section, which is the
    // case `the_https_filtering_group_declares_no_dependency` settled.
    SettingGroup {
        title: "Traffic capture",
        description: "Records the pages you load, in full, to a file — the addresses and \
                      what came back, not a summary. Six minutes of ordinary browsing \
                      wrote 114 MB, every account on this machine can read the file, and \
                      each time the proxy starts it writes another one and keeps the old \
                      ones. Turn it on to collect something for a bug report, then turn \
                      it off.",
        settings: &[
            Setting {
                key: crate::config::key::HAR_ENABLED,
                title: "Capture traffic to a file",
                description: "Off is how it ships",
                kind: Kind::Switch,
            },
            Setting {
                key: crate::config::key::HAR_LOCATION,
                title: "Capture folder",
                description: "The folder the files go in, named adguard.har. A single dot \
                              means AdGuard's own data folder — not the folder you started \
                              it from",
                kind: Kind::Text { secret: false },
            },
        ],
    },
];

/// The Filters page's settings half — one key, rendered above the catalogue.
///
/// A separate table rather than a group in [`ADVANCED`], because this key is
/// about the list the user is looking at: it is a **writer of that catalogue**
/// that runs whether or not any page renders it, and the row is the only brake
/// on it. A user who finds a filter switched on that they never switched on
/// looks at the Filters page, not at Advanced.
///
/// Rendered through the same [`crate::model::Setting`] machinery as the other
/// two tables, so the write, the verify-by-re-read and the external-edit
/// reconcile are the ones that already exist rather than a second copy of them.
pub const FILTER_SETTINGS: [SettingGroup; 1] = [SettingGroup {
    title: "Automatic filters",
    description: "What AdGuard changes in this list on its own.",
    settings: &[Setting {
        key: crate::config::key::AUTO_ENABLE_LANGUAGE_FILTERS,
        title: "Add filters for languages you browse in",
        description: "Adds catalogue filters for the languages of the pages you visit and \
                      for your system language, and switches them on, without asking. A \
                      list you switched off stays off — but a list you removed can come \
                      back, switched on",
        kind: Kind::Switch,
    }],
}];

/// The first-run assistant, in render order.
///
/// These are the questions `adguard-cli configure` asks that are worth asking
/// again in a GUI. It asks eight; this table carries four, and the four it
/// leaves out are left out for stated reasons rather than by oversight:
///
/// | Wizard prompt | Key | Why not here |
/// | --- | --- | --- |
/// | proxy server mode | `proxy_mode` | Still out, and now for a **measured** reason rather than an assumed one — see below. |
/// | proxy listen address | `listen_address` | Always blocked at first run: the seeded config has `listen_auth` off with empty credentials, and moving beyond loopback in that state is a **measured silent no-op** (contract §5). The Advanced page owns it, after setup, where the credentials can be set first. |
/// | certificate name | `https_filtering.root_certificate_name` | **Not cosmetic, though this table said so until the trust check was built.** The value names the CA *file*, so changing it points the check at a path nothing will create — only `configure` generates a certificate, and it will not run again here. Left out all the more firmly, and read-only everywhere (`config::key::ROOT_CERTIFICATE_NAME`). |
/// | filter list groups | the `filters` list | The Filters page is the whole of this, with a localised catalogue the wizard's numbered list cannot match. |
///
/// What remains is: the one protection switch whose answer changes what the
/// proxy does on day one, the two ports someone with a port conflict has to
/// change before anything works, and a consent question no other page asks.
///
/// **`proxy_mode` was revisited when auto mode landed, and stays out.** The
/// decision that put it here was "auto needs the root helper", which was an
/// assumption about *when* AdGuard enforces that; the measurement is worse than
/// the assumption. `config set proxy_mode auto` succeeds with the helper unmet
/// — exit 0, `Config has been updated`, and the file really holds `auto`
/// (contract §8). So the assistant could not offer the question honestly even
/// if it wanted to: it would have to run the helper check, explain the suid
/// bit, and show a `sudo` command, at the one moment the user is being walked
/// through first-time setup and has not yet seen a single page of the app. The
/// Advanced page has the room for that and the focus re-check that makes it
/// live; a wizard step has neither. The row there also has to render the unmet
/// state rather than merely prevent it, which is a second thing this table has
/// no way to express.
///
/// Each `key` is written with an ordinary `config set` **after** the directory
/// has been seeded — before that, every one of them is refused (contract §5).
pub const SETUP: [SettingGroup; 3] = [
    SettingGroup {
        title: "Protection",
        description: "AdGuard filters plain HTTP without this; most of the web is not \
                      plain HTTP. Leaving it on is the reason to install AdGuard at all.",
        settings: &[Setting {
            key: crate::config::key::HTTPS_FILTERING,
            title: "HTTPS filtering",
            description: "Filter encrypted traffic. Needs AdGuard's certificate \
                          trusted by each browser you want filtered",
            kind: Kind::Switch,
        }],
    },
    SettingGroup {
        title: "Ports",
        description: "The ports AdGuard listens on in manual proxy mode. Change one \
                      only if something else on this machine already holds it. \
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
        title: "Crash reports",
        description: "The CLI's wizard asks this and no other page of this app does, \
                      so it is asked here rather than answered on your behalf.",
        settings: &[Setting {
            key: crate::config::key::SEND_CRASH_REPORTS,
            title: "Send crash reports to AdGuard",
            description: "Helps AdGuard fix crashes. Off unless you turn it on",
            kind: Kind::Switch,
        }],
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

    /// The catalogue group this set will not switch on without an agreement
    /// typed at a prompt — see [`ANNOYANCE_TERMS`].
    ///
    /// Per-set rather than a bare constant, because **the same number means
    /// something else in the other database**. Measured on v1.4.13:
    /// `agflm_standard.db` group 4 is *Annoyances*; `agflm_dns.db` group 4 is
    /// *Security*. A plain `group_id == 4` test would put a dialog about
    /// violating websites' terms in front of the DNS malware lists.
    ///
    /// `None` for DNS because the DNS catalogue has no Annoyances group at all
    /// — its five groups are Custom filters, General, Other, Regional and
    /// Security — so `dns filters` never raises the prompt.
    pub fn annoyances_group(self) -> Option<i64> {
        match self {
            Self::Http => Some(FilterGroup::ANNOYANCES_ID),
            Self::Dns => None,
        }
    }
}

/// Whether the user has already agreed to a prompt the CLI raises on stdin.
///
/// Every other command in this crate runs with stdin closed so that each prompt
/// takes its default — the reasoning is on [`crate::Cli::run`], and it holds.
/// One prompt has no usable default: the annoyance-filter agreement *refuses
/// the work* rather than proceeding, so with stdin closed those lists can never
/// be switched on at all. This is how an answer reaches it.
///
/// A type rather than a `bool` so that no call site can pass `true` meaning
/// something else, and so that granting consent is a word that has to be
/// written down at the place it is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// stdin stays closed. What everything else in this crate does.
    Withheld,
    /// The user was shown [`ANNOYANCE_TERMS`] and accepted them.
    Granted,
}

/// AdGuard's own wording for the agreement it demands before enabling a list
/// from the Annoyances group.
///
/// Copied verbatim from the v1.4.13 prompt, reflowed from its 80-column
/// hard wrap into one paragraph. Verbatim on purpose: this is a statement about
/// what the user is liable for, and a paraphrase would be us putting our own
/// words into AdGuard's disclaimer. What is agreed to in the GUI has to be the
/// same thing the CLI asked about.
pub const ANNOYANCE_TERMS: &str = "You are about to enable one or more annoyance filters. \
     They block elements that are either unrelated to website content or related but annoying \
     to your user experience. Website owners may consider these elements mandatory: if you \
     block them, you may be violating their terms; some functionality of websites may not be \
     available or may not work properly. You understand and agree that you are solely \
     responsible to comply with the terms of use of websites you visit and that AdGuard is not \
     responsible for your compliance with the terms of use of websites you visit using our \
     products.";

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
    /// `filters remove` — **and it does two different things.**
    ///
    /// Against a catalogue filter it only clears `is_installed` and the row
    /// stays, which is why turning a switch off is [`Self::Disable`] everywhere
    /// in this app and never this. Against a *custom* filter the row is deleted
    /// from `filter` outright and there is no undo but re-fetching the URL
    /// (contract §6). That asymmetry is the whole reason removal is a
    /// confirmed action of its own rather than a quiet suffix button.
    Remove,
}

impl FilterAction {
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Remove => "remove",
        }
    }

    /// The word the CLI echoes back on success, as in
    /// `Filter [Title: AdGuard Base filter] enabled`.
    pub fn confirmation(self) -> &'static str {
        match self {
            Self::Add => "added",
            Self::Enable => "enabled",
            Self::Disable => "disabled",
            Self::Remove => "removed",
        }
    }

    /// Whether this action can destroy something the user cannot get back.
    ///
    /// True only for [`Self::Remove`], and only *because* of what it does to a
    /// custom row — the caller still has to know which kind of filter it holds.
    /// Kept here so the answer is beside the asymmetry it comes from.
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Remove)
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
    /// Where the list is fetched from. Empty for the user-rules pseudo-filter,
    /// which has no source; for a custom filter it is the URL the user gave
    /// [`crate::Cli::filters_install`], normalised (a local path arrives back
    /// as `file://…`), and the only thing identifying an untitled one.
    pub download_url: String,
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

    /// A list the user installed by URL, rather than one from AdGuard's
    /// catalogue.
    ///
    /// Decided by the group, not by the sign of the id. Custom filters are
    /// numbered from `-10001` downwards, so a range test would work today and
    /// would silently take in the user-rules sentinel (`i32::MIN`) if the
    /// numbering ever moved. [`FilterGroup::CUSTOM_ID`] is a real group that
    /// both databases carry.
    pub fn is_custom(&self) -> bool {
        self.group_id == FilterGroup::CUSTOM_ID
    }

    /// What to put on the row.
    ///
    /// [`Self::name`] is normally enough — it has already fallen back from the
    /// localised name to the English title. A **custom** filter can defeat both:
    /// installing a list with no `! Title:` header leaves `title` set to the
    /// empty string, and custom filters have no `filter_localisation` rows at
    /// all, so the whole `COALESCE` chain resolves to `''` and the row would
    /// render nameless (contract §6).
    ///
    /// The CLI papers over this in its own confirmation — it echoes
    /// `Filter [Title: file:///…]` for a title it then stores as empty — so the
    /// URL is both the honest fallback and the one the user will recognise.
    pub fn display_name(&self) -> &str {
        for candidate in [self.name.as_str(), self.title.as_str(), self.download_url.as_str()] {
            if !candidate.trim().is_empty() {
                return candidate;
            }
        }
        "Untitled filter"
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

    /// Whether switching this filter on will raise the CLI's annoyance-filter
    /// agreement (contract §7).
    ///
    /// **Decided by the group, not by the name or the id.** The report that
    /// found this named the five `AdGuard …` lists — 18 to 22 — but measuring
    /// the whole group turned up eleven: Fanboy's Annoyances, Web Annoyances
    /// Ultralist, Adblock Warning Removal List, EasyList Cookie List and the
    /// rest are gated identically. Meanwhile *CJX's Annoyances List* has the
    /// word in its title, sits in "Language-specific", and is not gated at all.
    /// So neither the name nor a range of ids describes the population; the
    /// group does.
    ///
    /// Only for the actions that switch a list **on**. `disable` and `remove`
    /// were measured ungated, and asking someone to accept liability for
    /// turning something *off* would be nonsense in any case.
    pub fn needs_annoyance_consent(&self, set: FilterSet, action: FilterAction) -> bool {
        matches!(action, FilterAction::Add | FilterAction::Enable)
            && set.annoyances_group() == Some(self.group_id)
    }

    /// Whether this filter's trust can be changed, and therefore whether its
    /// row gets the control that changes it.
    ///
    /// **Three separate reasons to say no, and the CLI only enforces two of
    /// them.** All measured on v1.4.13; contract §6, *Marking a custom filter
    /// trusted*.
    ///
    /// - **A DNS list cannot.** `adguard-cli dns filters` carries no
    ///   `set-trusted` at all — it is absent from that subcommand's help, and
    ///   asking for it answers `A subcommand is required` at exit 1. The
    ///   operation is *unrepresentable* for that set rather than merely
    ///   inadvisable, which is why [`crate::Cli::filters_set_trusted`] takes no
    ///   [`FilterSet`] to be wrong about.
    /// - **A catalogue filter cannot**, and AdGuard says so itself:
    ///   `set-trusted 2 true` answers `Failed to update trust filter with ID:
    ///   2: Filter not custom` at exit 0 and writes nothing. Trust is a
    ///   property of a list the user chose to fetch.
    /// - **The user-rules pseudo-filter must not — and this is the one the CLI
    ///   lets straight through.** It ships `is_trusted = 1`, and
    ///   `set-trusted -2147483648 false` was measured to *really write*, which
    ///   would quietly stop the scriptlet and HTML rules in the user's own
    ///   `user.txt` from being applied. Nothing in the output distinguishes
    ///   that from any other success. [`Self::is_custom`] already excludes it,
    ///   by the group rather than by the id — but that exclusion is load-bearing
    ///   here in a way it is nowhere else, which is why this is a predicate with
    ///   a name rather than an `is_custom()` at the call site.
    pub fn supports_trust(&self, set: FilterSet) -> bool {
        matches!(set, FilterSet::Http) && self.is_custom()
    }
}

/// The flags a mutation is verified against, re-read on their own so
/// confirming one toggle does not cost a whole catalogue read.
///
/// `trusted` joined the other two when the trust control landed, and it is the
/// one that moves *independently* of them: measured, `filters set-trusted`
/// works on a switched-off row and the flag survives a `disable`/`enable`
/// round trip (contract §6). So a re-read that patched only `enabled` and
/// `installed` would leave the row's record of its own trust behind after
/// every switch flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterState {
    pub enabled: bool,
    pub installed: bool,
    pub trusted: bool,
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

    /// The "Annoyances" group of `agflm_standard.db`, whose eleven lists the
    /// CLI will not enable without an agreement to [`ANNOYANCE_TERMS`].
    ///
    /// Reach it through [`FilterSet::annoyances_group`] and never directly:
    /// group 4 of the DNS catalogue is *Security*, and this number on its own
    /// does not say which database it belongs to.
    pub const ANNOYANCES_ID: i64 = 4;

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

/// One installed userscript, as the Extensions page renders it.
///
/// Assembled by [`crate::userscripts`] from two sources that answer different
/// questions — the `userscripts/` directory for what is installed, `proxy.yaml`
/// for what is switched on. See contract §15.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Userscript {
    /// The filename stem of the pair, e.g. `adguard-extra`.
    ///
    /// This is what `userscripts enable|disable|remove` is given — though the
    /// CLI matches it as a *substring* against ids and titles alike, which is
    /// what [`Self::ambiguous`] is about.
    pub id: String,
    /// Localised `name`, already fallen back through the bare language to the
    /// plain `name` key. Empty when the metadata carries none.
    pub name: String,
    /// Localised `description`; empty when there is none.
    pub description: String,
    /// `version` from the metadata. `None` when the script's source carried no
    /// `@version` — the CLI stores `""` for it, and issue #9 asks for the
    /// version *"when it is available"*, so absence is a state to render
    /// rather than a blank to print.
    pub version: Option<String>,
    /// `homepageURL`, falling back to `supportURL`. `None` when neither is set,
    /// which is the ordinary case for a script installed from a bare URL.
    pub homepage: Option<String>,
    /// `downloadURL` — where the script came from, and the only thing that
    /// makes a reinstall possible. `None` for a script whose metadata omits it.
    pub download_url: Option<String>,
    /// Whether `proxy.yaml`'s `userscripts:` list carries this script.
    pub enabled: bool,
    /// Whether the CLI can be made to act on this script at all.
    ///
    /// `true` when this id is a case-insensitive substring of **another**
    /// installed script's id or title, which makes every `enable`, `disable`
    /// and `remove` naming it refuse with `Multiple userscripts match …` — even
    /// when the exact id was passed, because there is no exact-match flag to
    /// reach for (contract §15).
    ///
    /// Computed here rather than in the GUI so that the value a row is drawn
    /// from is the value that knows the row cannot be acted on. A page that
    /// worked this out for itself would be re-deriving a CLI behaviour from a
    /// widget.
    pub ambiguous: bool,
}

impl Userscript {
    /// What to call this script on screen.
    ///
    /// The id is the fallback rather than a placeholder: it is a real name the
    /// user can act on, it is what the CLI's own messages will echo, and a
    /// script with no `@name` is far likelier than a filter with no title —
    /// nothing validates a userscript's metadata block.
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// Whether the two controls that change this script may be offered.
    ///
    /// The switch, the trash and the reinstall all go through a name the CLI
    /// resolves by substring, so an ambiguous script can be shown and read but
    /// not touched.
    pub fn actionable(&self) -> bool {
        !self.ambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape, with the key replaced by sixteen characters of the same
    /// length as the one this machine holds.
    fn license() -> License {
        License {
            owner: "someone@example.com".to_owned(),
            key: "ABCDEFGH12345678".to_owned(),
            status: License::ACTIVE.to_owned(),
        }
    }

    #[test]
    fn masking_keeps_only_the_last_four_characters() {
        let license = license();
        assert_eq!(license.masked_key(), "••••••••••••5678");
        assert!(
            !license.masked_key().contains("ABCDEFGH"),
            "the key survived masking"
        );
    }

    /// The rule is "the last four", not "at least the last four": a value too
    /// short to mask is not a reason to print it in full.
    #[test]
    fn a_short_key_is_masked_entirely() {
        for key in ["", "1", "abcd"] {
            let license = License {
                key: key.to_owned(),
                ..license()
            };
            let masked = license.masked_key();
            assert_eq!(masked.chars().count(), key.chars().count());
            assert!(
                masked.chars().all(|c| c == '•'),
                "{key:?} rendered as {masked:?}"
            );
        }
    }

    /// A key is bytes, not ASCII, and slicing one by byte index would panic on
    /// the first multi-byte character.
    #[test]
    fn masking_counts_characters_not_bytes() {
        let license = License {
            key: "ключ-ЖЖЖЖ".to_owned(),
            ..license()
        };
        assert_eq!(license.masked_key(), "•••••ЖЖЖЖ");
    }

    /// The one thing that must never work: a `{:?}` that carries the secret.
    /// `Error` values are printed this way throughout the test suites.
    #[test]
    fn debug_leaks_neither_the_key_nor_the_owner() {
        let printed = format!("{:?}", license());
        assert!(!printed.contains("ABCDEFGH12345678"), "{printed}");
        assert!(!printed.contains("someone@example.com"), "{printed}");
        // The status is not sensitive and is the part worth having in a log.
        assert!(printed.contains(License::ACTIVE), "{printed}");
    }

    /// The spelling is AdGuard's, so a case change upstream must not read as a
    /// dead licence.
    #[test]
    fn active_is_recognised_whatever_its_case() {
        for status in ["APP_ACTIVE", "app_active", "App_Active"] {
            assert!(License {
                status: status.to_owned(),
                ..license()
            }
            .is_active());
        }
        for status in ["APP_EXPIRED", "", "ACTIVE_APP"] {
            assert!(!License {
                status: status.to_owned(),
                ..license()
            }
            .is_active());
        }
    }

    fn filter(id: i64, enabled: bool, installed: bool) -> Filter {
        Filter {
            id,
            group_id: 1,
            name: "Test".to_owned(),
            title: "Test".to_owned(),
            description: String::new(),
            homepage: None,
            download_url: "https://example.org/list.txt".to_owned(),
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

    fn annoyance(installed: bool) -> Filter {
        Filter { group_id: FilterGroup::ANNOYANCES_ID, ..filter(18, false, installed) }
    }

    /// Both ways of switching a list on raise the agreement — the report that
    /// found this only ever saw `enable`, because by then the list had already
    /// been added by the click before.
    #[test]
    fn both_ways_of_switching_an_annoyance_list_on_need_consent() {
        for installed in [true, false] {
            let f = annoyance(installed);
            let action = f.action_for(true);
            assert!(
                f.needs_annoyance_consent(FilterSet::Http, action),
                "{action:?} on an annoyance list should ask first"
            );
        }
    }

    /// Nothing is gated on the way off, and asking someone to accept liability
    /// for *stopping* filtering would be nonsense.
    #[test]
    fn switching_an_annoyance_list_off_asks_nothing() {
        let f = annoyance(true);
        assert!(!f.needs_annoyance_consent(FilterSet::Http, FilterAction::Disable));
        assert!(!f.needs_annoyance_consent(FilterSet::Http, FilterAction::Remove));
    }

    /// The sharp edge: group 4 of `agflm_dns.db` is **Security**, not
    /// Annoyances. A bare `group_id == 4` test would put a dialog about
    /// violating websites' terms in front of the DNS malware lists — and the
    /// DNS catalogue has no annoyance gate to answer for in the first place.
    #[test]
    fn group_four_of_the_dns_catalogue_is_not_annoyances() {
        let security = annoyance(false);
        assert!(!security.needs_annoyance_consent(FilterSet::Dns, FilterAction::Add));
        assert_eq!(FilterSet::Dns.annoyances_group(), None);
    }

    /// *CJX's Annoyances List* is in "Language-specific" and is measured
    /// ungated, so the population cannot be described by the word in the title.
    #[test]
    fn a_list_named_annoyances_outside_the_group_is_not_gated() {
        let cjx = Filter {
            group_id: 7,
            name: "CJX's Annoyances List".to_owned(),
            ..filter(220, false, false)
        };
        assert!(!cjx.needs_annoyance_consent(FilterSet::Http, FilterAction::Add));
    }

    fn custom(id: i64) -> Filter {
        Filter { group_id: FilterGroup::CUSTOM_ID, ..filter(id, true, true) }
    }

    #[test]
    fn a_custom_http_list_is_the_one_thing_that_can_be_trusted() {
        assert!(custom(-10001).supports_trust(FilterSet::Http));
    }

    /// `dns filters` has no `set-trusted` subcommand at all, so a control on a
    /// DNS row would have nothing to call.
    #[test]
    fn a_custom_dns_list_cannot_be_trusted() {
        assert!(!custom(-10001).supports_trust(FilterSet::Dns));
    }

    /// AdGuard refuses this one itself — `Filter not custom` — so the predicate
    /// is agreeing with the CLI rather than inventing a rule.
    #[test]
    fn a_catalogue_filter_cannot_be_trusted() {
        assert!(!filter(2, true, true).supports_trust(FilterSet::Http));
    }

    /// The trap. The CLI accepts the sentinel and **writes**: the row ships
    /// `is_trusted = 1`, and untrusting it silently stops the user's own
    /// scriptlet and HTML rules from being applied. Nothing downstream would
    /// catch it, so nothing may offer it.
    #[test]
    fn the_user_rules_pseudo_filter_is_never_offered_a_trust_control() {
        // Group 0, as both databases really carry it — not a group that exists
        // in `filter_group`, and emphatically not the custom one.
        let user_rules = Filter { group_id: 0, ..filter(Filter::USER_RULES_ID, true, true) };
        assert!(!user_rules.supports_trust(FilterSet::Http));
        assert!(!user_rules.supports_trust(FilterSet::Dns));
    }

    /// A custom list installed with no `! Title:` header has an empty title and
    /// no localisation rows, so everything the catalogue would normally fall
    /// back to is also empty. Without the URL the row renders nameless.
    #[test]
    fn an_untitled_custom_filter_falls_back_to_its_url() {
        let mut f = filter(-10001, true, true);
        f.group_id = FilterGroup::CUSTOM_ID;
        f.name = String::new();
        f.title = String::new();
        f.download_url = "https://example.org/list.txt".to_owned();
        assert_eq!(f.display_name(), "https://example.org/list.txt");
    }

    /// The last resort. A row with nothing at all is still a row the user can
    /// switch off, so it needs *some* name.
    #[test]
    fn a_filter_with_nothing_to_show_is_still_named() {
        let mut f = filter(-10001, true, true);
        f.name = String::new();
        f.title = String::new();
        f.download_url = "   ".to_owned();
        assert_eq!(f.display_name(), "Untitled filter");
    }

    /// The localised name wins whenever there is one — the fallback must not
    /// take over a catalogue filter that has a perfectly good name.
    #[test]
    fn a_named_filter_keeps_its_name() {
        let f = filter(2, true, true);
        assert_eq!(f.display_name(), "Test");
    }

    /// Custom filters are numbered from -10001 downwards *today*, which makes a
    /// sign test look sufficient — it would also swallow the user-rules
    /// pseudo-filter, whose id is `i32::MIN` and which is not a custom list.
    #[test]
    fn custom_filters_are_told_apart_by_group_not_by_id() {
        let mut custom = filter(-10001, true, true);
        custom.group_id = FilterGroup::CUSTOM_ID;
        assert!(custom.is_custom());

        let user_rules = filter(Filter::USER_RULES_ID, true, false);
        assert!(!user_rules.is_custom(), "user rules read as a custom filter");

        assert!(!filter(2, true, true).is_custom());
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

    /// The HAR pair is one group with one switch, and the switch comes first.
    /// Order is load-bearing here in a way it is not elsewhere on the page: the
    /// folder row is inert until the switch is on, and a folder row rendered
    /// above the switch that governs it reads as a setting that already
    /// applies.
    #[test]
    fn traffic_capture_is_a_switch_then_a_folder() {
        use crate::config::key;
        let group = ADVANCED
            .iter()
            .find(|group| group.title == "Traffic capture")
            .expect("the Traffic capture group vanished");

        let keys: Vec<&str> = group.settings.iter().map(|s| s.key).collect();
        assert_eq!(keys, vec![key::HAR_ENABLED, key::HAR_LOCATION]);
        assert!(matches!(group.settings[0].kind, Kind::Switch));
        assert!(matches!(group.settings[1].kind, Kind::Text { secret: false }));
    }

    /// The cost is measured (`cli-contract.md` §9) and the group description is
    /// the only place a user meets it, so it may not quietly lose the parts
    /// that make capture different from every other switch on the page. Each
    /// clause here is a separate measurement and each was a surprise.
    #[test]
    fn the_capture_group_states_what_capture_costs() {
        let group = ADVANCED
            .iter()
            .find(|group| group.title == "Traffic capture")
            .expect("the Traffic capture group vanished");

        for claim in ["114 MB", "every account", "keeps the old"] {
            assert!(
                group.description.contains(claim),
                "the capture group stopped saying {claim:?}"
            );
        }
    }

    /// `.` is the data directory, not the working directory, and that is the
    /// one thing about this row a user cannot guess — measured 2 August 2026
    /// by starting a proxy from a third directory that stayed empty.
    #[test]
    fn the_folder_row_explains_the_shipped_dot() {
        use crate::config::key;
        let row = advanced_settings()
            .into_iter()
            .find(|s| s.key == key::HAR_LOCATION)
            .expect("the capture folder row vanished");

        assert!(row.description.contains("dot"));
        assert!(row.description.contains("data folder"));
        assert!(
            row.description.contains("not the folder you started"),
            "the row stopped ruling out the working directory, which is the whole finding"
        );
    }

    /// One key, one switch. The table exists to be the Filters page's settings
    /// half and nothing else; a second row here would be a settings *page*
    /// growing on top of a catalogue, which is the shape `architecture.md` §5
    /// rejected for userscripts.
    #[test]
    fn the_filters_table_is_one_switch() {
        use crate::config::key;
        assert_eq!(FILTER_SETTINGS.len(), 1);
        assert_eq!(FILTER_SETTINGS[0].settings.len(), 1);
        let row = FILTER_SETTINGS[0].settings[0];
        assert_eq!(row.key, key::AUTO_ENABLE_LANGUAGE_FILTERS);
        assert!(matches!(row.kind, Kind::Switch));
        assert_eq!(row.requires(), None, "invented a dependency nothing measured");
    }

    /// The key is on **one** page. Rendering it on Advanced as well would give
    /// the user two switches for one line of `proxy.yaml`, which is the
    /// "second, contradictory way" the `filters` list is kept unrendered for.
    #[test]
    fn the_language_key_is_not_also_on_advanced() {
        use crate::config::key;
        assert!(
            !advanced_settings()
                .iter()
                .any(|s| s.key == key::AUTO_ENABLE_LANGUAGE_FILTERS),
            "the language switch appeared on Advanced as well as Filters"
        );
    }

    /// **The measurement is the row.** `cli-contract.md` §6: the automatic add
    /// keys on `is_installed`, so a `disable` survives it and a `remove` does
    /// not. Both halves have to be in the subtitle, because they point opposite
    /// ways and a user who reads only one gets the wrong model — and because
    /// the asymmetry is the thing the row waited on a proxy run to learn.
    #[test]
    fn the_language_row_states_both_halves_of_the_asymmetry() {
        let row = FILTER_SETTINGS[0].settings[0];
        assert!(
            row.description.contains("switched off stays off"),
            "the row stopped saying a disabled list is respected"
        );
        assert!(
            row.description.contains("removed can come back"),
            "the row stopped saying a removed list is not respected"
        );
    }

    /// Exactly the two settings `proxy.yaml` documents as needing
    /// `dns_filtering`, and nothing else. A dependency the GUI invents would be
    /// as wrong as one it misses.
    #[test]
    fn only_the_documented_settings_declare_a_dependency() {
        use crate::config::key;

        let dependent: Vec<(&str, &str)> = ADVANCED
            .iter()
            .chain(STEALTH.iter())
            .flat_map(|group| group.settings.iter())
            .filter_map(|s| s.requires().map(|r| (s.key, r)))
            .collect();

        assert_eq!(
            dependent,
            vec![
                (key::FILTER_SECURE_DNS_MODE, key::DNS_FILTERING),
                (key::HTTPS_ECH, key::DNS_FILTERING),
            ]
        );
    }

    /// Both are reachable from a page, or the dependency marking has nowhere to
    /// appear.
    #[test]
    fn the_dependent_settings_are_on_a_page() {
        use crate::config::key;
        let keys: Vec<&str> = ADVANCED
            .iter()
            .flat_map(|group| group.settings.iter())
            .map(|s| s.key)
            .collect();
        assert!(keys.contains(&key::HTTPS_ECH));
        assert!(keys.contains(&key::FILTER_SECURE_DNS_MODE));
    }

    /// Same rule for the Stealth table, and one more: the master switch must
    /// NOT be here. It lives on Protection, and a key with a control on two
    /// pages is two writers for one setting.
    #[test]
    fn stealth_keys_are_unique_and_exclude_the_master_switch() {
        let keys: Vec<&str> = STEALTH
            .iter()
            .flat_map(|group| group.settings.iter())
            .map(|s| s.key)
            .collect();

        let mut sorted = keys.clone();
        let count = sorted.len();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "duplicate key in STEALTH");

        assert!(
            !keys.contains(&crate::config::key::STEALTH_MODE),
            "the master switch belongs to Protection, not here"
        );
        // Every key addresses the stealth section and nothing else.
        for key in &keys {
            assert!(key.starts_with("stealthmode."), "{key} is not a stealth setting");
        }
    }

    /// The nested section is the part gap 4 called out, so pin that it is
    /// actually represented rather than quietly dropped.
    #[test]
    fn stealth_includes_the_nested_anti_dpi_section() {
        let nested = STEALTH
            .iter()
            .flat_map(|group| group.settings.iter())
            .filter(|s| s.key.starts_with("stealthmode.anti_dpi."))
            .count();
        assert_eq!(nested, 6, "anti_dpi has six settings in proxy.yaml");
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
            key::ADGUARD_HEADERS,
            // The parity enumeration's first slice — `architecture.md` §5.
            key::HTTPS_FILTER_EV,
            key::HTTPS_TLS13,
            key::HTTPS_OCSP,
            key::HTTPS_CERT_TRANSPARENCY,
            key::HTTPS_HTTP3,
            key::FILTERED_PORTS,
            key::OUTBOUND_INTERFACE,
            key::HAR_ENABLED,
            key::HAR_LOCATION,
        ] {
            assert!(keys.contains(&expected), "{expected} is missing from ADVANCED");
        }
    }

    /// None of the five parity rows claims a `requires()`. Their dependency is
    /// `https_filtering.enabled` — the section they live in, not another one —
    /// which the group description states, the way Stealth's groups state
    /// theirs. A `requires()` here would be the invented dependency
    /// `only_the_documented_settings_declare_a_dependency` exists to catch.
    ///
    /// The count is asserted because the loop alone would pass just as happily
    /// against a table the group had been deleted from.
    #[test]
    fn the_https_filtering_group_declares_no_dependency() {
        use crate::config::key;
        let group = [
            key::HTTPS_FILTER_EV,
            key::HTTPS_TLS13,
            key::HTTPS_OCSP,
            key::HTTPS_CERT_TRANSPARENCY,
            key::HTTPS_HTTP3,
        ];
        let mut seen = 0;
        for setting in advanced_settings() {
            if group.contains(&setting.key) {
                seen += 1;
                assert!(setting.key.starts_with("https_filtering."));
                assert_eq!(setting.requires(), None, "{} invented a dependency", setting.key);
                assert!(
                    matches!(setting.kind, Kind::Switch),
                    "{} is a boolean in proxy.yaml and must render as a switch",
                    setting.key
                );
            }
        }
        assert_eq!(seen, group.len(), "the HTTPS filtering group lost a row");
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

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    /// The statistics key is deliberately **not** a seventh [`Toggle`].
    ///
    /// `Toggle` is the six switches that change what AdGuard does to traffic,
    /// and `Toggle::description` is documented as taking its wording from
    /// `proxy.yaml`'s own comments. This key has no comment to take, so a
    /// seventh variant would either carry an invented description or break that
    /// rule silently. It lives in its own group on the Protection page instead.
    #[test]
    fn the_statistics_key_is_not_a_protection_toggle() {
        for toggle in Toggle::ALL {
            assert_ne!(
                toggle.key(),
                crate::config::key::SAFEBROWSING_STATS,
                "{:?} took over the consent key",
                toggle
            );
        }
        assert_eq!(Toggle::ALL.len(), 6, "the protection switches changed count");
    }

    /// Every `Toggle` description really does come from the file, which is the
    /// rule the consent key could not satisfy. Cheap guard: the six are all
    /// non-empty and none of them announces a missing description.
    #[test]
    fn every_toggle_still_has_wording_from_the_file() {
        for toggle in Toggle::ALL {
            let description = toggle.description();
            assert!(!description.is_empty(), "{toggle:?} lost its description");
            assert!(
                !description.contains("not documented"),
                "{toggle:?} has no wording in proxy.yaml and should not be a Toggle"
            );
        }
    }
}

#[cfg(test)]
mod header_row_tests {
    use super::*;

    /// The row went into the group that already existed rather than creating
    /// one. `architecture.md` §5 justified the placement by *"the natural
    /// neighbour of HAR capture"*, which appeals to something unbuilt and
    /// blocked; the neighbour that actually ships is `log_level`, and the
    /// reason that survives is `proxy.yaml`'s own word — *debugging*.
    #[test]
    fn the_header_switch_joined_diagnostics_rather_than_inventing_a_group() {
        let diagnostics = ADVANCED
            .iter()
            .find(|group| group.title == "Diagnostics")
            .expect("the Diagnostics group vanished");
        let row = diagnostics
            .settings
            .iter()
            .find(|setting| setting.key == crate::config::key::ADGUARD_HEADERS)
            .expect("the header switch left Diagnostics");
        assert!(matches!(row.kind, Kind::Switch));
        assert_eq!(row.requires(), None, "invented a dependency nothing measured");
    }

    /// Directionality is the whole row, and it was very nearly written the
    /// wrong way round. These headers are added to **responses**, so the sites
    /// the user visits never receive them — a subtitle implying otherwise would
    /// be a privacy claim measured false. Both header names are named, because
    /// they are what a user greps for in devtools.
    #[test]
    fn the_header_row_does_not_imply_the_site_sees_them() {
        let row = ADVANCED
            .iter()
            .flat_map(|group| group.settings.iter())
            .find(|setting| setting.key == crate::config::key::ADGUARD_HEADERS)
            .expect("the header row is not on the Advanced page");
        let description = row.description;
        assert!(description.contains("X-Adguard-Filtered"), "{description}");
        assert!(description.contains("X-Adguard-Rule"), "{description}");
        assert!(description.contains("responses"), "{description}");
        assert!(
            description.contains("never see them"),
            "the row stopped saying the site cannot see these: {description}"
        );
    }
}

/// The `filtered_ports` row, whose entire design is that it does **not** repeat
/// what the CLI says — `architecture.md` §5 and `cli-contract.md` §5.
#[cfg(test)]
mod filtered_ports_tests {
    use super::{Kind, ADVANCED};

    fn row() -> super::Setting {
        *ADVANCED
            .iter()
            .flat_map(|group| group.settings.iter())
            .find(|setting| setting.key == crate::config::key::FILTERED_PORTS)
            .expect("the filtered ports row is not on the Advanced page")
    }

    /// The reason this row exists at all. `adguard-cli` refuses a bad value
    /// with *"Valid values are: space-separated list of valid ports or range of
    /// port"* — and `80 443` is refused. Measured, `cli-contract.md` §5. A
    /// future edit "helpfully" aligning this wording with the CLI's would hand
    /// the user the one form that cannot work, so the word is banned outright.
    #[test]
    fn the_row_never_repeats_the_cli_wrong_separator() {
        let description = row().description;
        assert!(
            !description.contains("space-separated") && !description.contains("space separated"),
            "took the CLI's wording, which recommends the form it rejects: {description}"
        );
        assert!(
            description.contains("commas"),
            "the row stopped naming the separator that works: {description}"
        );
    }

    /// Both forms `proxy.yaml`'s comment gives, verbatim, because the file was
    /// right where the binary was wrong and that is the only reason we know it.
    #[test]
    fn the_row_shows_the_two_documented_forms() {
        let description = row().description;
        assert!(description.contains("80,443,8080"), "{description}");
        assert!(description.contains("80:5221,5300:49151"), "{description}");
    }

    /// Its dependency is on `proxy_mode`, which is a **choice**, and
    /// `requires()` models a dependency on a boolean. It could not be declared
    /// here even if it should be — so it lives in the group description, the
    /// way *Manual proxy ports* states the opposite mode.
    #[test]
    fn the_mode_dependency_is_in_the_group_not_a_requires() {
        assert_eq!(row().requires(), None, "invented a dependency requires() cannot express");
        assert!(matches!(row().kind, Kind::Text { secret: false }));

        let group = ADVANCED
            .iter()
            .find(|group| {
                group
                    .settings
                    .iter()
                    .any(|setting| setting.key == crate::config::key::FILTERED_PORTS)
            })
            .expect("the group vanished");
        assert!(
            group.description.contains("automatic proxy"),
            "the group stopped naming the mode this applies in: {}",
            group.description
        );
        assert_eq!(
            group.settings.len(),
            1,
            "a second row here would inherit a mode caveat nobody wrote for it"
        );
    }
}

/// `outbound_interface`, whose placement the parity enumeration got wrong and
/// whose null the page had no way to render — `architecture.md` §5.
#[cfg(test)]
mod outbound_interface_tests {
    use super::{Kind, ADVANCED};

    /// **Not part of the outbound proxy, despite sharing a prefix.** It is a
    /// top-level key 144 lines above `outbound_proxy:` in `proxy.yaml` and it
    /// binds *every* outgoing connection, so filing it in that group — which
    /// the enumeration proposed — would tell the user it only applies to
    /// traffic going through a proxy they may not even have enabled.
    #[test]
    fn it_did_not_join_the_outbound_proxy_group() {
        let group = ADVANCED
            .iter()
            .find(|group| {
                group
                    .settings
                    .iter()
                    .any(|setting| setting.key == crate::config::key::OUTBOUND_INTERFACE)
            })
            .expect("the outbound interface row is not on the Advanced page");
        assert_ne!(group.title, "Outbound proxy");
        assert_eq!(group.settings.len(), 1);
        assert!(
            group.description.contains("empty"),
            "the group stopped saying what leaving it blank does: {}",
            group.description
        );
    }

    /// The one setting allowed to hold nothing, and the row has to say what
    /// nothing *means* — the system choosing — rather than leaving a blank box
    /// the user has to guess about.
    #[test]
    fn the_row_admits_the_value_is_unchecked() {
        let row = ADVANCED
            .iter()
            .flat_map(|group| group.settings.iter())
            .find(|setting| setting.key == crate::config::key::OUTBOUND_INTERFACE)
            .expect("the row vanished");
        assert!(row.may_be_absent());
        assert!(matches!(row.kind, Kind::Text { secret: false }));
        assert!(
            row.description.contains("does not check"),
            "the row stopped saying AdGuard accepts any name: {}",
            row.description
        );
    }
}
