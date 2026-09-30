//! Every check this application already makes, in one place and one report.
//!
//! Built for [issue #21], whose second item asked for a diagnostics page. Most
//! of what such a page needs was already here, spread across the pages that act
//! on it: the root helper on Advanced, the certificate and the browser manifests
//! on Protection, the helper's liveness and the access-log verdict folded into
//! the Status page's one answer. This module adds **no new way of knowing
//! anything**. It asks the same modules the same questions and reports each
//! answer on its own line.
//!
//! # Evidence, not a verdict
//!
//! That is the difference between this and the Status page, and it is the reason
//! the page exists. Status reduces everything to *am I protected?*, and the
//! reduction is careful about which facts may outrank which — a corpse in `/proc`
//! beats the access log, and neither is read unless `status` claims a single
//! running daemon. Here nothing is reduced. The helper process and the log
//! verdict are both shown, so someone asking *why* can see each input the
//! Status page weighed.
//!
//! What is **not** relaxed is the rule those checks are built on: only positive
//! evidence counts. [`HelperProcess::Unseen`] and [`Filtering::Unseen`] are
//! [`Level::Unknown`] here, never [`Level::Problem`], and the level vocabulary
//! is shaped so that a caller cannot render "we could not tell" as bad news.
//!
//! # The report is written to be pasted
//!
//! [`Report::text`] is what the page's copy button puts on the clipboard, and its
//! destination is a bug report — public, on this repository's tracker. So the
//! report carries nothing personal by construction rather than by scrubbing:
//!
//! - **The licence contributes its status word and nothing else.** [`Inputs`]
//!   holds a `String`, not a [`crate::License`], so the owner's e-mail and the key
//!   never reach this module at all and no later edit can print them by accident.
//! - **No setting that names the user's own network is read** — not
//!   `listen_address`, not the upstream DNS servers, not an outbound proxy —
//!   and the one place such an address arrives anyway, the endpoints `status`
//!   prints, keeps only the port unless the host is loopback.
//! - **Nothing from the access log but a verdict.** The log is a record of what
//!   the user browsed, and the verdict is one word.
//! - **The home directory is shortened to `~`** by [`redact_home`], over the whole
//!   text rather than path by path, so an error message from the CLI that quotes
//!   its own binary is caught too.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::borrow::Cow;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::browser::{self, BrowserIntegration};
use crate::config::key;
use crate::nss::{BrowserStores, Store, StoreState};
use crate::{
    access, helper, orphan, CaTrust, Cli, Config, Error, Filtering, HelperProcess, ProxyStatus,
    RootHelper,
};

/// How a finding should be read.
///
/// Four values rather than a bool, and the two in the middle are the point.
/// [`Self::Fact`] is a reading with no good or bad — a version, a path, a
/// setting the user chose. [`Self::Unknown`] is a check that could not answer,
/// which is not the same as one that answered badly (see the module header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The check ran and found what a working install has.
    Healthy,
    /// The check ran and found something that stops AdGuard working as
    /// configured. **Only ever set on positive evidence.**
    Problem,
    /// The check could not answer. Never a reason to reassure, never an alarm.
    Unknown,
    /// A reading, not a verdict.
    Fact,
}

impl Level {
    /// The marker [`Report::text`] puts in front of a line — fixed width, plain
    /// ASCII, so it survives a paste into anything and the columns still line
    /// up in a monospaced issue body.
    fn marker(self) -> &'static str {
        match self {
            Self::Healthy => "[ok]  ",
            Self::Problem => "[FAIL]",
            Self::Unknown => "[?]   ",
            Self::Fact => "      ",
        }
    }
}

/// The page that holds the fix for a problem.
///
/// Named here rather than in the GUI because *which* page owns a fix is a fact
/// about the check, and the report's text has to say it too. The GUI maps each
/// one to its own navigation; nothing here knows how a page is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedOn {
    /// Start, stop, restart, and licence activation.
    Status,
    /// The certificate and browser-integration commands.
    Protection,
    /// The root helper's setup command, under the proxy-mode setting.
    AdvancedProxyMode,
    /// The local DNS proxy's listen port.
    DnsProxy,
}

/// What to do about a problem, and where.
///
/// **A pointer, never the command itself.** Each command already has one
/// renderer, on the page that owns it, which checks it is safe to show before
/// showing it (`trust::quotable`) and withholds it when it is not. A second
/// copy here would be a second place to get that wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remedy {
    pub hint: &'static str,
    /// `None` when no page in this application can help — a missing file only
    /// reinstalling AdGuard restores.
    pub page: Option<FixedOn>,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Fixed for every line but the browser stores', whose number depends on
    /// the machine — one per Firefox profile.
    pub label: Cow<'static, str>,
    pub value: String,
    pub level: Level,
    /// Set on every [`Level::Problem`] this application knows a fix for.
    pub remedy: Option<Remedy>,
}

impl Finding {
    fn new(label: impl Into<Cow<'static, str>>, value: impl Into<String>, level: Level) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            level,
            remedy: None,
        }
    }

    /// Attach a fix. Only meaningful on a problem, and only ever called on one.
    fn fixed(mut self, hint: &'static str, page: Option<FixedOn>) -> Self {
        self.remedy = Some(Remedy { hint, page });
        self
    }
}

/// A titled group of findings — one per subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    /// A sentence saying what this section can and cannot see, where that is
    /// not obvious from the title. The certificate section needs one: it says
    /// which trust stores were looked in, and which were not.
    pub note: Option<&'static str>,
    pub findings: Vec<Finding>,
}

/// Every section, in the order a problem is usually traced: the CLI, the
/// process it runs, what reaches it, then the surfaces that depend on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub sections: Vec<Section>,
}

/// The proxy daemon, as `/proc` saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Daemons {
    /// No process running the resolved binary's `start`.
    None,
    /// Exactly one — the only case the per-run checks can be asked about.
    One {
        pid: i32,
        /// Seconds the process has been running, when its start could be dated.
        uptime: Option<u64>,
        helper: HelperProcess,
        /// `None` when the run could not be dated, so the log was not read.
        filtering: Option<Filtering>,
    },
    /// Several. `/proc` cannot say which one `status` meant.
    Several(Vec<i32>),
}

/// Everything the report is built from, gathered in one pass.
///
/// Separate from [`Report`] so the rendering can be tested against states this
/// machine is not in — a dead helper, a bypass, a missing manifest — without
/// producing any of them for real.
#[derive(Debug)]
pub struct Inputs {
    pub binary: PathBuf,
    pub version: Result<String, String>,
    pub status: Result<ProxyStatus, String>,
    /// The licence's status word **only**. See the module header for why this
    /// is not a [`crate::License`].
    pub licence: Result<String, LicenceError>,
    pub config: Result<Config, String>,
    pub daemons: Daemons,
    pub helper: Option<Result<RootHelper, String>>,
    pub ca: Option<CaTrust>,
    /// The browsers' own certificate stores. `None` when there was nothing to
    /// look for — no certificate, or no home directory.
    pub stores: Option<BrowserStores>,
    pub browsers: Option<BrowserIntegration>,
}

/// Why the licence could not be read — the one distinction the Status page
/// keeps too, because "not activated" is an answer and a failed read is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenceError {
    Unlicensed,
    Unreadable(String),
}

impl Inputs {
    /// Read this machine.
    ///
    /// **Blocking**, and meant for a worker thread: three `adguard-cli`
    /// invocations, a walk of `/proc`, and — when exactly one daemon is running
    /// — a read of the access log's tail, which is a few mebibytes.
    ///
    /// The three invocations run one after the other, never together. Two
    /// commands against the same data directory is the shape measured to make
    /// one fail on a fresh install and block on an initialised one (contract
    /// §3), and this is not worth either.
    pub fn collect(cli: &Cli) -> Self {
        let version = cli.version().map_err(|err| err.to_string());
        let status = cli.status().map_err(|err| err.to_string());
        let licence = match cli.license() {
            Ok(licence) => Ok(licence.status),
            Err(Error::Unlicensed { .. }) => Err(LicenceError::Unlicensed),
            Err(err) => Err(LicenceError::Unreadable(err.to_string())),
        };
        let config = Config::load().map_err(|err| err.to_string());

        let certificate_name = config
            .as_ref()
            .map_or(crate::trust::DEFAULT_CERTIFICATE_NAME, Config::certificate_name);

        Self {
            binary: cli.binary().clone(),
            version,
            status,
            licence,
            daemons: daemons(cli),
            helper: RootHelper::detect().map(|found| found.map_err(|err: io::Error| err.to_string())),
            stores: crate::paths::certificate(certificate_name)
                .as_deref()
                .and_then(BrowserStores::detect),
            ca: CaTrust::detect(certificate_name),
            browsers: BrowserIntegration::detect(),
            config,
        }
    }
}

/// Find the daemon, and ask the per-run questions of it when there is one.
fn daemons(cli: &Cli) -> Daemons {
    let found = orphan::daemons(cli.binary());
    match found.as_slice() {
        [] => Daemons::None,
        [daemon] => {
            let started = daemon.started_at();
            let uptime = started
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .map(|age| age.as_secs());
            Daemons::One {
                pid: daemon.pid(),
                uptime,
                helper: helper::process(daemon.pid()),
                // Scoped to the run, exactly as the Status page scopes it: an
                // undatable run is not read at all, because reading the log
                // unscoped would count a previous run's failures against it.
                filtering: started.map(access::filtering),
            }
        }
        several => Daemons::Several(several.iter().map(orphan::Daemon::pid).collect()),
    }
}

impl Report {
    /// Build the report from what was read.
    pub fn build(inputs: &Inputs) -> Self {
        let config = inputs.config.as_ref().ok();
        Self {
            sections: vec![
                cli_section(inputs),
                proxy_section(inputs, config),
                helper_section(inputs),
                https_section(inputs, config),
                browser_section(inputs),
                dns_section(inputs, config),
            ],
        }
    }

    /// Every finding at [`Level::Problem`], in report order.
    pub fn problems(&self) -> impl Iterator<Item = &Finding> {
        self.sections
            .iter()
            .flat_map(|section| &section.findings)
            .filter(|finding| finding.level == Level::Problem)
    }

    /// The report as plain text, for pasting into a bug report.
    ///
    /// `app_version` is this application's, which the core crate does not know.
    /// The home directory is **not** yet shortened here — the caller passes the
    /// result through [`redact_home`], which is kept separate so it can be
    /// tested against a home directory that is not the test runner's.
    pub fn text(&self, app_version: &str) -> String {
        let mut out = format!("AdGuard UI {app_version} — diagnostics\n");
        for section in &self.sections {
            out.push_str(&format!("\n{}\n", section.title));
            if let Some(note) = section.note {
                out.push_str(&format!("  ({note})\n"));
            }
            for finding in &section.findings {
                out.push_str(&format!(
                    "  {} {}: {}\n",
                    finding.level.marker(),
                    finding.label,
                    finding.value
                ));
                if let Some(remedy) = finding.remedy {
                    out.push_str(&format!("         Fix: {}\n", remedy.hint));
                }
            }
        }
        out
    }
}

/// Replace every occurrence of the home directory in `text` with `~`.
///
/// Over the whole text rather than per path, because paths also arrive inside
/// sentences this module did not write — an error from the CLI quotes its own
/// binary. A home that is empty or `/` is left alone: replacing it would
/// mangle every path in the report rather than shorten one.
pub fn redact_home(text: &str, home: Option<&str>) -> String {
    match home.map(|home| home.trim_end_matches('/')) {
        Some(home) if !home.is_empty() => text.replace(home, "~"),
        _ => text.to_owned(),
    }
}

fn cli_section(inputs: &Inputs) -> Section {
    let mut findings = vec![
        match &inputs.version {
            Ok(banner) => Finding::new("Version", short_version(banner), Level::Fact),
            Err(err) => Finding::new("Version", err.clone(), Level::Unknown),
        },
        Finding::new("Binary", inputs.binary.display().to_string(), Level::Fact),
    ];

    findings.push(match &inputs.config {
        Ok(config) => Finding::new("Configuration", config.path().display().to_string(), Level::Fact),
        // An unreadable `proxy.yaml` is positive evidence: every page reads
        // its settings from there, and none of them can show anything.
        Err(err) => Finding::new("Configuration", err.clone(), Level::Problem).fixed(
            "Every page reads its settings from this file. Correct it by hand, or restore it \
             from a backup.",
            None,
        ),
    });

    findings.push(match &inputs.licence {
        Ok(status) if status.eq_ignore_ascii_case(crate::License::ACTIVE) => {
            Finding::new("Licence", "Active", Level::Healthy)
        }
        // Rendered as itself: a status word this does not recognise is shown
        // rather than mapped, the same as the Status page does.
        Ok(status) => Finding::new("Licence", status.clone(), Level::Problem)
            .fixed(ACTIVATE, Some(FixedOn::Status)),
        Err(LicenceError::Unlicensed) => Finding::new("Licence", "Not activated", Level::Problem)
            .fixed(ACTIVATE, Some(FixedOn::Status)),
        Err(LicenceError::Unreadable(err)) => Finding::new("Licence", err.clone(), Level::Unknown),
    });

    Section {
        title: "AdGuard CLI",
        note: None,
        findings,
    }
}

fn proxy_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let mut findings = Vec::new();

    let running = match &inputs.status {
        Ok(status) => {
            findings.push(if status.running {
                Finding::new("State", "Running", Level::Healthy)
            } else {
                // Stopped is something the user may have chosen, and still the
                // first thing anyone diagnosing "nothing is filtered" needs.
                Finding::new("State", "Stopped", Level::Problem)
                    .fixed("Start protection on the Status page.", Some(FixedOn::Status))
            });
            Some(status)
        }
        Err(err) => {
            findings.push(Finding::new("State", err.clone(), Level::Unknown));
            None
        }
    };

    if let Some(mode) = config.and_then(Config::proxy_mode) {
        findings.push(Finding::new("Mode", mode.trim(), Level::Fact));
    }
    if let Some(status) = running.filter(|status| status.running) {
        for (label, endpoint) in [
            ("HTTP proxy", &status.http_proxy),
            ("SOCKS5 proxy", &status.socks5_proxy),
        ] {
            if let Some(endpoint) = endpoint {
                findings.push(Finding::new(label, loopback_or_port(endpoint), Level::Fact));
            }
        }
    }

    let claims_running = running.map(|status| status.running);
    findings.push(daemon_finding(&inputs.daemons, claims_running));

    // The two per-run checks, each on its own line. Only with exactly one
    // daemon: with none there is no run, and with several there is no saying
    // which run `status` meant.
    if let Daemons::One {
        helper, filtering, ..
    } = &inputs.daemons
    {
        findings.push(match helper {
            HelperProcess::Running => Finding::new("Root helper process", "Running", Level::Healthy),
            HelperProcess::Defunct => Finding::new(
                "Root helper process",
                "Exited — traffic is no longer reaching the proxy",
                Level::Problem,
            )
            .fixed(
                "Restart protection on the Status page. A restart is what clears this.",
                Some(FixedOn::Status),
            ),
            HelperProcess::Unseen => {
                Finding::new("Root helper process", "Not seen", Level::Unknown)
            }
        });
        findings.push(match filtering {
            Some(Filtering::Reaching) => Finding::new(
                "Traffic",
                "AdGuard's own requests are getting through",
                Level::Healthy,
            ),
            Some(Filtering::Bypassed) => Finding::new(
                "Traffic",
                "AdGuard's own requests have been failing for hours, and nothing else is \
                 reaching the proxy",
                Level::Problem,
            )
            .fixed(
                "Restart protection on the Status page, which is what cleared this every time it \
                 was measured. If this computer has been offline for hours, that alone would \
                 explain it.",
                Some(FixedOn::Status),
            ),
            Some(Filtering::Unseen) => Finding::new(
                "Traffic",
                "Not enough in the access log to tell yet",
                Level::Unknown,
            ),
            None => Finding::new("Traffic", "This run could not be dated", Level::Unknown),
        });
    }

    Section {
        title: "Proxy",
        note: None,
        findings,
    }
}

/// The process count, read against what `status` claims.
///
/// **The contradiction is the finding, not either fact alone** — the pairing
/// [`crate::orphan`] is built on. No daemon while stopped is ordinary; one
/// while stopped is a proxy the CLI has lost track of.
fn daemon_finding(daemons: &Daemons, claims_running: Option<bool>) -> Finding {
    const LABEL: &str = "Proxy process";
    match (daemons, claims_running) {
        (Daemons::None, Some(true)) => Finding::new(
            LABEL,
            "None found, though the CLI reports the proxy running",
            Level::Unknown,
        ),
        (Daemons::None, _) => Finding::new(LABEL, "None", Level::Fact),
        (Daemons::One { pid, .. }, Some(false)) => Finding::new(
            LABEL,
            format!("PID {pid}, though the CLI reports the proxy stopped — it has lost track of it"),
            Level::Problem,
        )
        .fixed(
            "Start protection on the Status page. If this process is in the way, the page ends \
             it and tries again.",
            Some(FixedOn::Status),
        ),
        (Daemons::One { pid, uptime, .. }, _) => {
            let value = match uptime {
                Some(seconds) => format!("PID {pid}, up {}", duration(*seconds)),
                None => format!("PID {pid}"),
            };
            Finding::new(LABEL, value, Level::Fact)
        }
        (Daemons::Several(pids), _) => Finding::new(
            LABEL,
            format!(
                "{} running ({}) — only one should be",
                pids.len(),
                pids.iter().map(i32::to_string).collect::<Vec<_>>().join(", ")
            ),
            Level::Problem,
        )
        .fixed(
            "Stop protection on the Status page and start it again. A start that finds a \
             leftover process in its way ends it and tries again.",
            Some(FixedOn::Status),
        ),
    }
}

fn helper_section(inputs: &Inputs) -> Section {
    let findings = match &inputs.helper {
        None => vec![Finding::new(
            "Setup",
            "Not looked for — the CLI could not be located",
            Level::Unknown,
        )],
        Some(Err(err)) => vec![Finding::new("Setup", err.clone(), Level::Problem)],
        Some(Ok(helper)) => {
            let setup = if helper.is_set_up() {
                Finding::new("Setup", "Owned by root, setuid, executable", Level::Healthy)
            } else {
                Finding::new(
                    "Setup",
                    format!("Missing {}", helper.unmet().join(", ")),
                    Level::Problem,
                )
                .fixed(
                    "Advanced shows AdGuard's own command that sets it up, under Proxy mode.",
                    Some(FixedOn::AdvancedProxyMode),
                )
            };
            vec![
                setup,
                Finding::new("Path", helper.path.display().to_string(), Level::Fact),
            ]
        }
    };
    Section {
        title: "Root helper",
        note: None,
        findings,
    }
}

fn https_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let mut findings = Vec::new();
    let enabled = config.and_then(|config| config.bool_at(key::HTTPS_FILTERING));
    findings.push(on_off("HTTPS filtering", enabled));
    findings.push(on_off(
        "HTTP/3 filtering",
        config.and_then(|config| config.bool_at(key::HTTPS_HTTP3)),
    ));

    match &inputs.ca {
        None => findings.push(Finding::new(
            "Certificate",
            "Not looked for — AdGuard's data directory could not be located",
            Level::Unknown,
        )),
        Some(ca) => {
            // An untrusted CA only breaks anything while HTTPS filtering is on.
            // Off, the same reading is a fact about the machine rather than a
            // fault in it, and flagging it would send the user to fix
            // something that is not in their way.
            let failing = if enabled == Some(false) {
                Level::Fact
            } else {
                Level::Problem
            };
            findings.push(match ca.unmet().first() {
                None => Finding::new("Certificate", "Trusted by the system", Level::Healthy),
                Some(unmet) if failing == Level::Problem => {
                    Finding::new("Certificate", capitalise(unmet), failing)
                        .fixed(certificate_fix(ca), Some(FixedOn::Protection))
                }
                Some(unmet) => Finding::new("Certificate", capitalise(unmet), failing),
            });
            findings.push(Finding::new(
                "Certificate file",
                ca.certificate.display().to_string(),
                Level::Fact,
            ));
        }
    }

    if let Some(stores) = &inputs.stores {
        let failing = if enabled == Some(false) {
            Level::Fact
        } else {
            Level::Problem
        };
        if stores.stores.is_empty() {
            findings.push(Finding::new(
                "Browser stores",
                "None found",
                Level::Fact,
            ));
        }
        for store in &stores.stores {
            findings.push(store_finding(store, failing));
        }
    }

    Section {
        title: "HTTPS filtering",
        note: Some(
            "Checks the system trust store, and the stores Firefox profiles and Chromium-based \
             browsers keep of their own — the places AdGuard's installer writes to, and the \
             Flatpak and newer Firefox locations it does not know.",
        ),
        findings,
    }
}

/// One browser store as a line, under a name that carries no profile name —
/// see [`Store::anonymous_name`].
///
/// `failing` is the level a store that lacks the CA gets: a problem while
/// HTTPS filtering is on, and a plain reading while it is off, as for the
/// system store above.
fn store_finding(store: &Store, failing: Level) -> Finding {
    let label = store.anonymous_name();
    let finding = match &store.state {
        StoreState::Trusted => return Finding::new(label, "Trusted", Level::Healthy),
        StoreState::Unreadable(why) => {
            return Finding::new(label, format!("Could not be read: {why}"), Level::Unknown)
        }
        StoreState::Missing => Finding::new(label, "Does not have the certificate", failing),
        StoreState::Untrusted => Finding::new(
            label,
            "Has the certificate, but does not trust it to identify websites",
            failing,
        ),
    };
    if failing != Level::Problem {
        return finding;
    }
    let hint = if !store.installer_reaches() {
        OUT_OF_INSTALLER_REACH
    } else if store.profile_flag().is_some() {
        NAMED_PROFILE
    } else {
        INSTALL_IN_BROWSERS
    };
    finding.fixed(hint, Some(FixedOn::Protection))
}

fn browser_section(inputs: &Inputs) -> Section {
    let findings = match &inputs.browsers {
        None => vec![Finding::new(
            "Browsers",
            "Not looked for — no home directory",
            Level::Unknown,
        )],
        Some(integration) => {
            let mut findings = Vec::new();
            if !integration.host_present {
                findings.push(
                    Finding::new(
                        "Native host",
                        format!("{} is missing beside adguard-cli", browser::HOST_BINARY),
                        Level::Problem,
                    )
                    .fixed("Reinstalling AdGuard CLI restores it.", None),
                );
            }
            if integration.browsers.is_empty() {
                findings.push(Finding::new(
                    "Browsers",
                    "None of the six AdGuard supports were found",
                    Level::Fact,
                ));
            }
            for found in &integration.browsers {
                findings.push(match &found.state {
                    browser::State::Ready => Finding::new(found.name, "Integrated", Level::Healthy),
                    browser::State::Missing => {
                        Finding::new(found.name, "Not integrated", Level::Problem)
                            .fixed(INTEGRATE, Some(FixedOn::Protection))
                    }
                    browser::State::Stale(named) => Finding::new(
                        found.name,
                        format!("Points at {}, not this AdGuard", named.display()),
                        Level::Problem,
                    )
                    .fixed(INTEGRATE, Some(FixedOn::Protection)),
                    browser::State::Unreadable(why) => Finding::new(
                        found.name,
                        format!("Manifest could not be read: {why}"),
                        Level::Unknown,
                    ),
                });
            }
            findings
        }
    };
    Section {
        title: "Browser integration",
        note: None,
        findings,
    }
}

fn dns_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let enabled = config.and_then(|config| config.bool_at(key::DNS_FILTERING));
    let mut findings = vec![on_off("DNS filtering", enabled)];

    // The one dependency the CLI does not enforce and the pages already warn
    // about: on in manual mode with no listener filters nothing (contract §5).
    if enabled == Some(true) && config.is_some_and(Config::dns_filtering_is_inert) {
        findings.push(
            Finding::new(
                "Listener",
                "None — in manual mode DNS filtering needs a listen port, so it filters nothing",
                Level::Problem,
            )
            .fixed(
                "Give the local DNS proxy a port on the DNS page.",
                Some(FixedOn::DnsProxy),
            ),
        );
    }
    if let Ok(status) = &inputs.status {
        if status.running {
            findings.push(Finding::new(
                "System DNS filtering",
                if status.system_dns_filtering { "On" } else { "Off" },
                Level::Fact,
            ));
        }
    }

    Section {
        title: "DNS",
        note: None,
        findings,
    }
}

/// An endpoint as the report may carry it: whole when it is loopback, and as
/// its port alone otherwise.
///
/// `status` prints the address the proxy listens on, which is
/// `listen_address` — and a user who has opened the proxy to their network has
/// put a LAN address there. The module header promises nothing that names the
/// user's network, so the host goes and the port, which is what a diagnosis
/// needs, stays.
fn loopback_or_port(endpoint: &str) -> String {
    let (host, port) = endpoint.rsplit_once(':').unwrap_or((endpoint, ""));
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        endpoint.to_owned()
    } else if port.is_empty() {
        "a non-loopback address".to_owned()
    } else {
        format!("port {port}, on a non-loopback address")
    }
}

/// The licence's fix, from either of its two failing readings.
const ACTIVATE: &str = "Activate it on the Status page.";

/// A browser's fix. One command covers every browser AdGuard finds.
const INTEGRATE: &str =
    "Protection shows AdGuard's own command that installs the integration for every browser it \
     finds.";

/// A browser store the installer reaches on its own.
const INSTALL_IN_BROWSERS: &str =
    "Protection can add it to this store for you, and shows AdGuard's own installer command, \
     which does the same.";

/// A Firefox profile the installer only reaches when it is named.
const NAMED_PROFILE: &str =
    "Protection can add it to this store for you. AdGuard's installer only reaches Firefox's \
     default profile by itself, so its command there names this one.";

/// A Flatpak Chromium store, which the installer has no way to reach.
const OUT_OF_INSTALLER_REACH: &str =
    "Protection can add it to this store for you. AdGuard's own installer does not know about \
     Flatpak browsers.";

/// What fixes the certificate, by which step it has not reached.
///
/// In [`CaTrust::unmet`]'s order, and matching the command the Protection page
/// shows for each — that page renders the command, this names it.
fn certificate_fix(ca: &CaTrust) -> &'static str {
    if !ca.generated {
        "Protection shows AdGuard's own command that generates one."
    } else if ca.stale {
        "AdGuard's installer will not replace a file of the same name, so the old certificate \
         has to be removed first. Protection shows one command that does both; it asks for \
         your password."
    } else if !ca.anchored {
        "Protection shows AdGuard's own installer command; it asks for your password."
    } else {
        "Protection shows the command that rebuilds the system's trust store."
    }
}

/// A boolean setting as a finding. Neither value is a fault: both are the
/// user's choice, and the Protection page is where they are made.
fn on_off(label: &'static str, value: Option<bool>) -> Finding {
    match value {
        Some(true) => Finding::new(label, "On", Level::Fact),
        Some(false) => Finding::new(label, "Off", Level::Fact),
        None => Finding::new(label, "Could not be read", Level::Unknown),
    }
}

/// `AdGuard CLI v1.4.13` → `1.4.13`; anything unfamiliar is kept whole, so a
/// banner that changes shape is still shown rather than lost.
fn short_version(banner: &str) -> &str {
    let banner = banner.trim();
    banner
        .rsplit(' ')
        .next()
        .map(|last| last.trim_start_matches('v'))
        .filter(|version| version.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(banner)
}

/// `4 h 12 min`, `3 d 2 h`, `45 s` — two units at most.
fn duration(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// The trust module's unmet steps are written to follow "because"; a finding
/// starts a line.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(text: &str) -> Config {
        Config::parse(text, Path::new("/home/someone/.local/share/adguard-cli/proxy.yaml"))
            .expect("fixture parses")
    }

    fn running() -> ProxyStatus {
        ProxyStatus {
            running: true,
            http_proxy: Some("127.0.0.1:3129".into()),
            socks5_proxy: Some("127.0.0.1:1081".into()),
            manual_dns_proxy: false,
            system_wide_filtering: true,
            system_dns_filtering: true,
        }
    }

    /// A healthy machine: one daemon, a live helper, traffic reaching it.
    fn healthy() -> Inputs {
        Inputs {
            binary: PathBuf::from("/home/someone/.local/bin/adguard-cli"),
            version: Ok("AdGuard CLI v1.4.13".into()),
            status: Ok(running()),
            licence: Ok("APP_ACTIVE".into()),
            config: Ok(config(
                "proxy_mode: 'auto'\nhttps_filtering:\n  enabled: true\n  \
                 http3_filtering_enabled: false\ndns_filtering:\n  enabled: true\n  listen_port: -1\n",
            )),
            daemons: Daemons::One {
                pid: 4242,
                uptime: Some(3 * 3_600 + 5 * 60),
                helper: HelperProcess::Running,
                filtering: Some(Filtering::Reaching),
            },
            helper: None,
            ca: None,
            stores: None,
            browsers: None,
        }
    }

    fn finding<'a>(report: &'a Report, section: &str, label: &str) -> &'a Finding {
        report
            .sections
            .iter()
            .find(|s| s.title == section)
            .and_then(|s| s.findings.iter().find(|f| f.label == label))
            .unwrap_or_else(|| panic!("no {label} in {section}"))
    }

    #[test]
    fn a_healthy_machine_reports_no_problems() {
        let report = Report::build(&healthy());
        let problems: Vec<_> = report.problems().collect();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(finding(&report, "AdGuard CLI", "Version").value, "1.4.13");
        assert_eq!(finding(&report, "Proxy", "Proxy process").value, "PID 4242, up 3 h 5 min");
    }

    /// The rule the whole of `access.rs` and `helper.rs` is built on, carried
    /// through: a check that could not answer is never shown as a failure.
    #[test]
    fn an_absence_of_evidence_is_never_a_problem() {
        let mut inputs = healthy();
        inputs.daemons = Daemons::One {
            pid: 1,
            uptime: None,
            helper: HelperProcess::Unseen,
            filtering: Some(Filtering::Unseen),
        };
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "Proxy", "Root helper process").level, Level::Unknown);
        assert_eq!(finding(&report, "Proxy", "Traffic").level, Level::Unknown);
        assert_eq!(report.problems().count(), 0);
    }

    #[test]
    fn a_dead_helper_and_a_bypass_are_both_shown() {
        let mut inputs = healthy();
        inputs.daemons = Daemons::One {
            pid: 1,
            uptime: None,
            helper: HelperProcess::Defunct,
            filtering: Some(Filtering::Bypassed),
        };
        let report = Report::build(&inputs);
        // Both, where the Status page would show only the corpse: here the
        // point is every input, not the one that wins.
        assert_eq!(finding(&report, "Proxy", "Root helper process").level, Level::Problem);
        assert_eq!(finding(&report, "Proxy", "Traffic").level, Level::Problem);
    }

    /// Neither fact alone, the contradiction — `orphan.rs`'s pairing.
    #[test]
    fn a_daemon_the_cli_calls_stopped_is_the_problem_and_none_is_not() {
        let mut inputs = healthy();
        inputs.status = Ok(ProxyStatus {
            running: false,
            ..running()
        });
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "Proxy", "Proxy process").level, Level::Problem);

        inputs.daemons = Daemons::None;
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "Proxy", "Proxy process").level, Level::Fact);
    }

    #[test]
    fn several_daemons_are_a_problem() {
        let mut inputs = healthy();
        inputs.daemons = Daemons::Several(vec![10, 20]);
        let report = Report::build(&inputs);
        let found = finding(&report, "Proxy", "Proxy process");
        assert_eq!(found.level, Level::Problem);
        assert!(found.value.contains("10, 20"), "{}", found.value);
        // No per-run lines: there is no saying which run they would be about.
        assert!(report.sections[1].findings.iter().all(|f| f.label != "Traffic"));
    }

    /// The certificate's reading only matters while HTTPS filtering is on.
    #[test]
    fn an_untrusted_certificate_is_a_problem_only_while_https_filtering_is_on() {
        let untrusted = CaTrust::inspect("/nonexistent/AdGuard CLI CA.pem", None, None);

        let mut inputs = healthy();
        inputs.ca = Some(untrusted.clone());
        let report = Report::build(&inputs);
        let found = finding(&report, "HTTPS filtering", "Certificate");
        assert_eq!(found.level, Level::Problem);
        assert_eq!(found.value, "No certificate has been generated");
        assert_eq!(found.remedy.map(|r| r.page), Some(Some(FixedOn::Protection)));

        inputs.config = Ok(config("https_filtering:\n  enabled: false\n"));
        let report = Report::build(&inputs);
        let found = finding(&report, "HTTPS filtering", "Certificate");
        assert_eq!(found.level, Level::Fact);
        // Nothing to fix while nothing depends on it — and the Protection page
        // hides its certificate rows in this state, so a link would land on
        // nothing.
        assert_eq!(found.remedy, None);
    }

    /// One line per browser store, under a name that leaves the profile's
    /// own name out, and a fix that names `-f` only where it is needed.
    #[test]
    fn browser_stores_are_reported_one_line_each_without_profile_names() {
        use crate::nss::Profile;

        let store = |profile: Option<Profile>, state| Store {
            browser: if profile.is_some() { "Firefox" } else { "Chromium-based browsers" },
            scanned: true,
            profile,
            database: PathBuf::from("/home/someone/.pki/nssdb/cert9.db"),
            state,
        };
        let profile = |number, default| Profile {
            name: String::from("Jan Kowalski"),
            number,
            default,
            dir: PathBuf::from("/home/someone/.mozilla/firefox/abcd.Jan Kowalski"),
        };
        let mut inputs = healthy();
        inputs.stores = Some(BrowserStores {
            stores: vec![
                store(Some(profile(1, true)), StoreState::Trusted),
                store(Some(profile(2, false)), StoreState::Missing),
                store(None, StoreState::Untrusted),
            ],
        });
        let report = Report::build(&inputs);

        let default = finding(&report, "HTTPS filtering", "Firefox profile 1 (default)");
        assert_eq!(default.level, Level::Healthy);
        let other = finding(&report, "HTTPS filtering", "Firefox profile 2");
        assert_eq!(other.level, Level::Problem);
        assert_eq!(other.remedy.map(|r| r.hint), Some(NAMED_PROFILE));
        let chromium = finding(&report, "HTTPS filtering", "Chromium-based browsers");
        assert_eq!(chromium.level, Level::Problem);
        assert_eq!(chromium.remedy.map(|r| r.hint), Some(INSTALL_IN_BROWSERS));
        assert!(!report.text("t").contains("Kowalski"));

        // Off, the same readings are facts, and nothing offers a fix.
        inputs.config = Ok(config("https_filtering:\n  enabled: false\n"));
        let report = Report::build(&inputs);
        let other = finding(&report, "HTTPS filtering", "Firefox profile 2");
        assert_eq!(other.level, Level::Fact);
        assert_eq!(other.remedy, None);
    }

    #[test]
    fn inert_dns_filtering_is_flagged() {
        let mut inputs = healthy();
        inputs.config = Ok(config(
            "proxy_mode: 'manual'\ndns_filtering:\n  enabled: true\n  listen_port: -1\n",
        ));
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "DNS", "Listener").level, Level::Problem);

        // Auto mode needs no listener, and the healthy fixture is in auto mode
        // with the same port.
        let report = Report::build(&healthy());
        assert!(report.sections[5].findings.iter().all(|f| f.label != "Listener"));
    }

    #[test]
    fn browsers_are_reported_one_line_each() {
        let home = std::env::temp_dir().join(format!("adguard-diag-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".config/chromium")).unwrap();
        let mut inputs = healthy();
        inputs.browsers = Some(BrowserIntegration::detect_under(&home, None));
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "Browser integration", "Chromium").level, Level::Problem);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_unlicensed_install_says_so_and_an_unreadable_licence_does_not() {
        let mut inputs = healthy();
        inputs.licence = Err(LicenceError::Unlicensed);
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "AdGuard CLI", "Licence").level, Level::Problem);

        inputs.licence = Err(LicenceError::Unreadable("timed out".into()));
        let report = Report::build(&inputs);
        assert_eq!(finding(&report, "AdGuard CLI", "Licence").level, Level::Unknown);
    }

    #[test]
    fn the_text_marks_each_line_and_carries_the_certificate_caveat() {
        let text = Report::build(&healthy()).text("1.6.1");
        assert!(text.starts_with("AdGuard UI 1.6.1 — diagnostics\n"));
        assert!(text.contains("  [ok]   State: Running\n"), "{text}");
        assert!(text.contains("         Mode: auto\n"), "{text}");
        assert!(text.contains("the Flatpak and newer Firefox locations"), "{text}");
    }

    /// The report is written for a public tracker.
    #[test]
    fn the_home_directory_is_shortened_everywhere_it_appears() {
        let text = Report::build(&healthy()).text("1.6.1");
        let redacted = redact_home(&text, Some("/home/someone/"));
        assert!(!redacted.contains("/home/someone"), "{redacted}");
        assert!(redacted.contains("Binary: ~/.local/bin/adguard-cli"), "{redacted}");
        assert!(redacted.contains("~/.local/share/adguard-cli/proxy.yaml"), "{redacted}");
    }

    #[test]
    fn a_degenerate_home_is_not_substituted() {
        assert_eq!(redact_home("/usr/bin/x", Some("/")), "/usr/bin/x");
        assert_eq!(redact_home("/usr/bin/x", Some("")), "/usr/bin/x");
        assert_eq!(redact_home("/usr/bin/x", None), "/usr/bin/x");
    }

    /// `status` echoes `listen_address`, which may be the user's LAN address.
    #[test]
    fn only_a_loopback_endpoint_is_reported_whole() {
        assert_eq!(loopback_or_port("127.0.0.1:3129"), "127.0.0.1:3129");
        assert_eq!(loopback_or_port("[::1]:3129"), "[::1]:3129");
        assert_eq!(loopback_or_port("localhost:3129"), "localhost:3129");
        assert_eq!(loopback_or_port("192.168.1.20:3129"), "port 3129, on a non-loopback address");
        assert_eq!(loopback_or_port("0.0.0.0:1081"), "port 1081, on a non-loopback address");
        assert_eq!(loopback_or_port("[fe80::1]:1081"), "port 1081, on a non-loopback address");
    }

    /// The user's complaint that prompted this: a problem with no way forward.
    #[test]
    fn every_problem_says_how_to_fix_it() {
        let home = std::env::temp_dir().join(format!("adguard-diag-fix-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".config/chromium")).unwrap();
        let inputs = Inputs {
            binary: PathBuf::from("/x/adguard-cli"),
            version: Err("gone".into()),
            status: Ok(ProxyStatus {
                running: false,
                ..running()
            }),
            licence: Err(LicenceError::Unlicensed),
            config: Ok(config(
                "proxy_mode: 'manual'\nhttps_filtering:\n  enabled: true\n\
                 dns_filtering:\n  enabled: true\n  listen_port: -1\n",
            )),
            daemons: Daemons::One {
                pid: 7,
                uptime: None,
                helper: HelperProcess::Defunct,
                filtering: Some(Filtering::Bypassed),
            },
            helper: Some(RootHelper::inspect("/bin/sh").map_err(|err| err.to_string())),
            ca: Some(CaTrust::inspect("/nonexistent/AdGuard CLI CA.pem", None, None)),
            stores: None,
            browsers: Some(BrowserIntegration::detect_under(&home, None)),
        };
        let report = Report::build(&inputs);
        std::fs::remove_dir_all(&home).unwrap();

        let problems: Vec<_> = report.problems().collect();
        assert!(problems.len() >= 8, "{problems:#?}");
        for problem in problems {
            assert!(problem.remedy.is_some(), "no fix for {problem:?}");
        }
        // And only problems carry one: a fix beside a passing line would read
        // as an instruction to change something that works.
        for section in &report.sections {
            for finding in section.findings.iter().filter(|f| f.level != Level::Problem) {
                assert_eq!(finding.remedy, None, "{finding:?}");
            }
        }
        assert!(report.text("t").contains("         Fix: Activate it on the Status page.\n"));
    }

    #[test]
    fn a_stale_certificate_says_the_old_one_has_to_go_first() {
        let ca = CaTrust {
            stale: true,
            ..CaTrust::inspect("/nonexistent/AdGuard CLI CA.pem", None, None)
        };
        let ca = CaTrust { generated: true, ..ca };
        assert!(certificate_fix(&ca).contains("removed first"), "{}", certificate_fix(&ca));
    }

    #[test]
    fn versions_and_durations_read_as_phrases() {
        assert_eq!(short_version("AdGuard CLI v1.4.13\n"), "1.4.13");
        assert_eq!(short_version("adguard-cli nightly"), "adguard-cli nightly");
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(125), "2 min");
        assert_eq!(duration(3 * 86_400 + 2 * 3_600 + 59), "3 d 2 h");
    }
}
