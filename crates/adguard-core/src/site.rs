//! One website, checked from end to end: the fourth item of [issue #21].
//!
//! The user names a site that misbehaves — does not load, loads with ads, loads
//! without its pictures — and this answers from every place that can see it:
//! whether anything is wrong with protection as a whole, what the site's name
//! resolves to, whether it can be reached, what AdGuard's log says happened to
//! requests to it and which rules decided them, whether AdGuard decrypts it,
//! and whether the browser reached it over QUIC. Each answer is a
//! [`diagnostics::Finding`], so the Diagnostics page renders it with the same
//! rows and the same links to the page that holds a fix.
//!
//! # What is new here, and what is not
//!
//! Two things are new ways of knowing, both started by the user pressing the
//! button and both about the site they typed: **a name lookup** through the
//! system resolver — so through AdGuard's DNS filtering whenever that is what
//! the system uses — and **one TCP connection** to port 443, closed as soon as
//! it opens. Nothing is sent over it. Everything else is read: the machine-wide
//! checks are [`diagnostics`]'s, the history is AdGuard's own log read in place
//! ([`requests::seen`]), and the exclusion list and the user's rules are files
//! in AdGuard's data directory. Nothing is written and nothing is kept.
//!
//! # It never says a rule is a fault
//!
//! A rule that blocked requests to the site is AdGuard doing what it was told,
//! and most sites have some — every ad on the page is one. So a rule is a
//! [`Level::Fact`] carrying the way to allow the site, never a
//! [`Level::Problem`]: the user decides whether that block is the one that
//! broke it. The same holds for HTTP/3 switched off: QUIC traffic that passes
//! unfiltered is what that setting means, and the row says so and leads to it.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::fs;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::activity::Action;
use crate::config::key;
use crate::diagnostics::{self, Finding, FixedOn, Level, Report, Section};
use crate::requests::{self, Seen};
use crate::{Catalogue, Cli, Config, FilterSet, Locale};

/// How long the connection attempt may take.
const CONNECT: Duration = Duration::from_secs(5);

/// How many addresses the lookup line names.
const ADDRESSES: usize = 3;

/// How many of the user's own rules that mention the site are listed.
const OWN_RULES: usize = 3;

/// The setting naming the exclusion list, relative to the data directory.
const EXCLUSIONS: &str = "https_filtering.exclusions";

/// The host name in what the user typed: a bare name, or an address with a
/// scheme, path or port, in any case. `None` when there is no name in it.
pub fn host(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let url = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    crate::activity::host(&url).filter(|host| host.contains('.') || host.contains(':'))
}

/// Everything the check reads, gathered in one pass on a worker thread.
///
/// Separate from the report for the reason [`diagnostics::Inputs`] is: the
/// wording can be tested against states this machine is not in.
#[derive(Debug)]
pub struct Inputs {
    pub host: String,
    /// The machine-wide checks. Only their problems are carried over.
    pub machine: diagnostics::Inputs,
    /// The system resolver's answer.
    pub resolved: Result<Vec<IpAddr>, String>,
    /// A TCP connection to port 443 of the first usable address. `None` when
    /// there was no address worth trying.
    pub connected: Option<Result<(), String>>,
    /// AdGuard's log for the site. `None` without a data directory.
    pub seen: Option<Seen>,
    /// The exclusion list entry the site matches, when the list was read.
    pub excluded: Option<Option<String>>,
    /// Lines of `user.txt` and `dns_user.txt` that mention the site.
    pub own_rules: Vec<String>,
    pub own_dns_rules: Vec<String>,
    /// Filter list names by id, for the rules.
    pub names: std::collections::HashMap<i64, String>,
}

impl Inputs {
    /// Read everything for `host`. **Blocking**: three `adguard-cli`
    /// invocations, a name lookup, a connection attempt of up to five seconds
    /// and a full pass over AdGuard's log.
    pub fn collect(cli: &Cli, host: &str) -> Self {
        let machine = diagnostics::Inputs::collect(cli);
        let resolved = (host, 443)
            .to_socket_addrs()
            .map(|addrs| {
                let mut ips: Vec<IpAddr> = Vec::new();
                for addr in addrs {
                    if !ips.contains(&addr.ip()) {
                        ips.push(addr.ip());
                    }
                }
                ips
            })
            .map_err(|err| err.to_string());
        let connected = resolved.as_ref().ok().and_then(|ips| {
            let ip = ips.iter().find(|ip| !blocked_answer(ip))?;
            Some(
                TcpStream::connect_timeout(&SocketAddr::new(*ip, 443), CONNECT)
                    .map(drop)
                    .map_err(|err| err.to_string()),
            )
        });

        let config = machine.config.as_ref().ok();
        let excluded = crate::paths::data_dir()
            .zip(config.and_then(|config| config.str_at(EXCLUSIONS)))
            .and_then(|(dir, name)| fs::read_to_string(dir.join(name.trim())).ok())
            .map(|list| exclusion(host, &list));

        let mentions = |path: Option<std::path::PathBuf>, limit: usize| -> Vec<String> {
            path.and_then(|path| fs::read_to_string(path).ok())
                .map(|text| mentioning(&text, host, limit))
                .unwrap_or_default()
        };

        let mut names = std::collections::HashMap::new();
        if let Ok(catalogue) = Catalogue::open_set(FilterSet::Http) {
            let locale = Locale::from_env();
            for filter in catalogue.filters(&locale).unwrap_or_default() {
                names.insert(filter.id, filter.name);
            }
            if let Ok(Some(user)) = catalogue.user_rules(&locale) {
                names.insert(user.id, "Your rules".to_owned());
            }
        }

        Self {
            host: host.to_owned(),
            resolved,
            connected,
            seen: crate::access::path().map(|live| requests::seen(&live, host)),
            excluded,
            own_rules: mentions(crate::paths::user_rules_file(), OWN_RULES),
            own_dns_rules: mentions(crate::paths::dns_user_rules_file(), OWN_RULES),
            names,
            machine,
        }
    }
}

/// The report for one site. Its sections are shown below the site's name, in
/// the order a failing page is usually traced.
pub fn report(inputs: &Inputs) -> Report {
    let config = inputs.machine.config.as_ref().ok();
    Report {
        sections: vec![
            machine_section(inputs),
            lookup_section(inputs, config),
            filtering_section(inputs),
            https_section(inputs, config),
            quic_section(inputs, config),
        ],
    }
}

/// Anything the Diagnostics checks found wrong, since it breaks every site.
fn machine_section(inputs: &Inputs) -> Section {
    let machine = Report::build(&inputs.machine);
    let mut findings: Vec<Finding> = machine.problems().cloned().collect();
    if findings.is_empty() {
        findings.push(Finding::new(
            "Protection",
            "Nothing wrong found on this computer",
            Level::Healthy,
        ));
    }
    Section {
        title: "This computer",
        note: Some("A problem here affects every site, so it comes first."),
        findings,
    }
}

fn lookup_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let dns = config.and_then(|config| config.bool_at(key::DNS_FILTERING));
    let mut findings = Vec::new();

    findings.push(match &inputs.resolved {
        Ok(ips) if !ips.is_empty() && ips.iter().all(blocked_answer) => {
            let finding = Finding::new(
                "Name lookup",
                format!("Answered with {} — a blocked name, not a real address", list(ips)),
                Level::Problem,
            );
            if dns == Some(true) {
                finding.fixed(DNS_BLOCKED, Some(FixedOn::DnsFilters))
            } else {
                finding.fixed(DNS_BLOCKED_ELSEWHERE, None)
            }
        }
        Ok(ips) if ips.is_empty() => {
            Finding::new("Name lookup", "No address came back", Level::Problem)
                .fixed(NO_ADDRESS, (dns == Some(true)).then_some(FixedOn::DnsFilters))
        }
        Ok(ips) => Finding::new("Name lookup", list(ips), Level::Healthy),
        Err(err) => Finding::new("Name lookup", format!("Failed: {err}"), Level::Problem)
            .fixed(NO_ADDRESS, (dns == Some(true)).then_some(FixedOn::DnsFilters)),
    });
    findings.push(match &inputs.connected {
        Some(Ok(())) => Finding::new("Connection", "Port 443 answered", Level::Healthy),
        Some(Err(err)) => Finding::new("Connection", format!("Port 443 did not answer: {err}"), Level::Problem)
            .fixed(NO_CONNECTION, None),
        None => Finding::new("Connection", "Not tried — no address to try", Level::Unknown),
    });

    findings.push(match dns {
        Some(true) => Finding::new("DNS filtering", "On", Level::Fact),
        Some(false) => Finding::new("DNS filtering", "Off", Level::Fact),
        None => Finding::new("DNS filtering", "Could not be read", Level::Unknown),
    });
    for rule in &inputs.own_dns_rules {
        findings.push(
            Finding::new("Your DNS rule", rule.clone(), Level::Fact)
                .fixed("Your own DNS rules are on the DNS page.", Some(FixedOn::DnsFilters)),
        );
    }

    Section {
        title: "Name and connection",
        note: Some(
            "Looked up through this computer's resolver, as a browser would, and one connection \
             opened to port 443 and closed. Nothing was sent over it.",
        ),
        findings,
    }
}

fn filtering_section(inputs: &Inputs) -> Section {
    let mut findings = Vec::new();
    match &inputs.seen {
        None => findings.push(Finding::new(
            "Requests",
            "AdGuard's log could not be found",
            Level::Unknown,
        )),
        Some(seen) if seen.total == 0 => findings.push(Finding::new(
            "Requests",
            format!(
                "None in AdGuard's log{}. Nothing has asked for this site since, or its traffic \
                 does not pass through AdGuard — an app set to bypass it, or a browser using its \
                 own proxy.",
                since(seen.reaches_back)
            ),
            Level::Unknown,
        )),
        Some(seen) => {
            let counts = &seen.counts;
            let mut parts = vec![format!("{} passed", counts.passed)];
            for (count, word) in [
                (counts.blocked, "blocked"),
                (counts.modified, "modified"),
                (counts.allowed, "allowed by an exception"),
                (counts.uninspected, "logged with no action"),
            ] {
                if count > 0 {
                    parts.push(format!("{count} {word}"));
                }
            }
            findings.push(
                Finding::new(
                    "Requests",
                    format!("{}{}: {}", seen.total, since(seen.reaches_back), parts.join(", ")),
                    Level::Fact,
                )
                .fixed("Every one of them is on the Activity page.", Some(FixedOn::Activity)),
            );
            for (filter, rule, action, count) in &seen.rules {
                let list = inputs
                    .names
                    .get(filter)
                    .cloned()
                    .unwrap_or_else(|| format!("filter list {filter}"));
                let finding = Finding::new(
                    "Rule",
                    format!("{rule} — {list}, {} {count} time{}", verb(*action), plural(*count)),
                    Level::Fact,
                );
                findings.push(match action {
                    Action::Blocked | Action::Modified => {
                        finding.fixed(ALLOW, Some(FixedOn::WebFilters))
                    }
                    _ => finding,
                });
            }
        }
    }
    for rule in &inputs.own_rules {
        findings.push(
            Finding::new("Your rule", rule.clone(), Level::Fact)
                .fixed("Your own rules are on the Filters page.", Some(FixedOn::WebFilters)),
        );
    }
    Section {
        title: "Filtering",
        note: Some(
            "From AdGuard's own log, which keeps a few days. The site and every name under it \
             are counted.",
        ),
        findings,
    }
}

fn https_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let enabled = config.and_then(|config| config.bool_at(key::HTTPS_FILTERING));
    let mut findings = vec![match enabled {
        Some(true) => Finding::new("HTTPS filtering", "On", Level::Fact),
        Some(false) => Finding::new("HTTPS filtering", "Off", Level::Fact),
        None => Finding::new("HTTPS filtering", "Could not be read", Level::Unknown),
    }];
    match &inputs.excluded {
        Some(Some(entry)) => findings.push(Finding::new(
            "Exclusions",
            format!(
                "Listed as {entry}, so AdGuard does not decrypt it — only its name can be \
                 filtered"
            ),
            Level::Fact,
        )),
        Some(None) => findings.push(Finding::new("Exclusions", "Not listed", Level::Fact)),
        None => findings.push(Finding::new(
            "Exclusions",
            "AdGuard's exclusion list could not be read",
            Level::Unknown,
        )),
    }
    if let Some(seen) = &inputs.seen {
        if seen.decrypted > 0 {
            findings.push(Finding::new(
                "Decrypted",
                format!("Yes — AdGuard saw the full address of {} request{}", seen.decrypted, plural(seen.decrypted)),
                Level::Healthy,
            ));
        } else if seen.tls > 0 {
            findings.push(Finding::new(
                "Decrypted",
                format!(
                    "No — AdGuard saw {} encrypted connection{} and never a full address, so it \
                     filtered them by name only. An exclusion, an app set to bypass HTTPS \
                     filtering, or a site AdGuard declines to decrypt does that.",
                    seen.tls,
                    plural(seen.tls)
                ),
                Level::Fact,
            ));
        }
    }
    Section {
        title: "HTTPS",
        note: None,
        findings,
    }
}

fn quic_section(inputs: &Inputs, config: Option<&Config>) -> Section {
    let enabled = config.and_then(|config| config.bool_at(key::HTTPS_HTTP3));
    let mut findings = vec![match enabled {
        Some(true) => Finding::new("HTTP/3 filtering", "On", Level::Fact),
        Some(false) => Finding::new("HTTP/3 filtering", "Off", Level::Fact),
        None => Finding::new("HTTP/3 filtering", "Could not be read", Level::Unknown),
    }];
    match &inputs.seen {
        Some(seen) if seen.quic > 0 => {
            let value = format!(
                "{} connection{}: {} blocked, {} logged with no action",
                seen.quic,
                plural(seen.quic),
                seen.quic_blocked,
                seen.quic_uninspected
            );
            let finding = Finding::new("QUIC", value, Level::Fact);
            findings.push(if enabled == Some(false) {
                finding.fixed(QUIC_OFF, Some(FixedOn::AdvancedHttp3))
            } else if seen.quic_uninspected > 0 {
                finding.fixed(QUIC_UNINSPECTED, None)
            } else {
                finding
            });
        }
        Some(_) => findings.push(Finding::new("QUIC", "None to this site in the log", Level::Fact)),
        None => {}
    }
    Section {
        title: "HTTP/3",
        note: Some(
            "Browsers reach many sites over QUIC, the transport HTTP/3 runs on, and switch to it \
             without asking.",
        ),
        findings,
    }
}

/// An answer that is a block rather than an address: what DNS filters
/// answer with by default.
fn blocked_answer(ip: &IpAddr) -> bool {
    ip.is_unspecified() || ip.is_loopback()
}

/// The entry of AdGuard's exclusion list that covers `host`, as written.
///
/// Entries are domain names, one per line, and cover the names under them;
/// some are quoted, some carry a `$app=` modifier, a few hold a `*` for one
/// label. Read that way, from the list shipped with 1.4.13. An entry limited
/// to one app is still reported, since the list does not say which app is
/// asking.
pub fn exclusion(host: &str, list: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with(['!', '#']))
        .find(|line| {
            let entry = line.split('$').next().unwrap_or(line).trim().trim_matches('"');
            covers(&entry.to_lowercase(), &host)
        })
        .map(str::to_owned)
}

/// Whether an exclusion entry covers `host`: the name itself or one under
/// it, with `*` standing for exactly one label.
fn covers(entry: &str, host: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    let entry: Vec<&str> = entry.split('.').collect();
    let host: Vec<&str> = host.split('.').collect();
    if host.len() < entry.len() {
        return false;
    }
    host[host.len() - entry.len()..]
        .iter()
        .zip(&entry)
        .all(|(label, pattern)| *pattern == "*" || label == pattern)
}

/// Lines of a rules file that mention `host`, at most `limit`, skipping
/// comments.
fn mentioning(text: &str, host: &str, limit: usize) -> Vec<String> {
    let host = host.to_ascii_lowercase();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('!') && !line.starts_with('#'))
        .filter(|line| line.to_ascii_lowercase().contains(&host))
        .take(limit)
        .map(str::to_owned)
        .collect()
}

fn list(ips: &[IpAddr]) -> String {
    let mut shown: Vec<String> = ips.iter().take(ADDRESSES).map(IpAddr::to_string).collect();
    if ips.len() > ADDRESSES {
        shown.push(format!("{} more", ips.len() - ADDRESSES));
    }
    shown.join(", ")
}

/// ` since Sun 27 Sep`, from a microsecond timestamp.
fn since(micros: Option<i64>) -> String {
    let Some(micros) = micros else { return String::new() };
    let time = micros.div_euclid(1_000_000) as libc::time_t;
    // SAFETY: a zeroed `tm` is valid, and `localtime_r` writes only into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        " since {} {} {:02}:{:02}",
        tm.tm_mday,
        MONTHS.get(tm.tm_mon as usize).unwrap_or(&"?"),
        tm.tm_hour,
        tm.tm_min
    )
}

fn verb(action: Action) -> &'static str {
    match action {
        Action::Blocked => "blocked",
        Action::Modified => "modified",
        Action::Allowed => "allowed",
        Action::Passed | Action::Uninspected => "matched",
    }
}

fn plural(count: u64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

const DNS_BLOCKED: &str = "A DNS filter or one of your DNS rules blocks this name. The DNS page \
                           lists both; an exception rule there (@@||name^) lets it through.";
const DNS_BLOCKED_ELSEWHERE: &str = "AdGuard's DNS filtering is off, so something else answered \
                                     with a block: another DNS filter, or the hosts file.";
const NO_ADDRESS: &str = "The name does not resolve. Check its spelling; a DNS filter set to \
                          answer that blocked names do not exist looks the same.";
const NO_CONNECTION: &str = "The site, or the network between here and it, did not answer. A \
                             firewall or a site that is down looks the same from here.";
const ALLOW: &str = "If this is what breaks the site, an exception rule in your own rules on the \
                     Filters page lets it through: @@||name^ allows the whole site.";
const QUIC_OFF: &str = "With HTTP/3 filtering off, AdGuard lets QUIC through unfiltered, so ads \
                        and trackers reached over it are not blocked. The switch is on Advanced.";
const QUIC_UNINSPECTED: &str = "What AdGuard means by a QUIC line with no action is not \
                                documented. If ads still show on this site, a browser setting \
                                that turns QUIC off makes it use a connection AdGuard filters.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_taken_from_whatever_was_typed() {
        assert_eq!(host("example.com").as_deref(), Some("example.com"));
        assert_eq!(host("  https://WWW.Example.com:8443/a?b#c ").as_deref(), Some("www.example.com"));
        assert_eq!(host("example.com/path").as_deref(), Some("example.com"));
        assert_eq!(host("example.com.").as_deref(), Some("example.com"));
        assert_eq!(host(""), None);
        assert_eq!(host("localhost"), None, "a single label is not a site");
        assert_eq!(host("exa mple.com"), None);
    }

    #[test]
    fn an_exclusion_covers_its_name_and_the_names_under_it() {
        let list = "api.example.com\n\"Quoted.example.org\"\nping.*.example.net\n\
                    chat.example.io$app=Discord.exe\n! a comment\n";
        assert_eq!(exclusion("api.example.com", list).as_deref(), Some("api.example.com"));
        assert_eq!(exclusion("v2.api.example.com", list).as_deref(), Some("api.example.com"));
        assert_eq!(exclusion("example.com", list), None, "a parent is not covered");
        assert_eq!(exclusion("notapi.example.com", list), None);
        assert_eq!(exclusion("quoted.EXAMPLE.org", list).as_deref(), Some("\"Quoted.example.org\""));
        assert_eq!(exclusion("ping.eu.example.net", list).as_deref(), Some("ping.*.example.net"));
        assert_eq!(exclusion("ping.example.net", list), None);
        assert_eq!(
            exclusion("chat.example.io", list).as_deref(),
            Some("chat.example.io$app=Discord.exe")
        );
    }

    #[test]
    fn own_rules_that_mention_the_site_are_found_and_comments_are_not() {
        let text = "! example.com is great\n||ads.example.com^\n@@||Example.com^$document\nother.org\n";
        assert_eq!(
            mentioning(text, "example.com", 3),
            ["||ads.example.com^", "@@||Example.com^$document"]
        );
        assert_eq!(mentioning(text, "example.com", 1).len(), 1);
    }

    #[test]
    fn a_block_answer_is_recognised() {
        assert!(blocked_answer(&"0.0.0.0".parse().unwrap()));
        assert!(blocked_answer(&"::".parse().unwrap()));
        assert!(blocked_answer(&"127.0.0.1".parse().unwrap()));
        assert!(!blocked_answer(&"192.0.2.1".parse().unwrap()));
    }

    /// Built from parts, so every branch is reachable without the network.
    fn inputs(seen: Option<Seen>, http3: bool) -> Inputs {
        let config = diagnostics::tests::config(&format!(
            "dns_filtering:\n  enabled: true\nhttps_filtering:\n  enabled: true\n  http3_filtering_enabled: {http3}\n"
        ));
        let mut machine = diagnostics::tests::healthy();
        machine.config = Ok(config);
        Inputs {
            host: "example.com".to_owned(),
            machine,
            resolved: Ok(vec!["0.0.0.0".parse().unwrap()]),
            connected: None,
            seen,
            excluded: Some(Some("example.com".to_owned())),
            own_rules: vec!["||example.com^".to_owned()],
            own_dns_rules: Vec::new(),
            names: [(2, "AdGuard Base filter".to_owned())].into(),
        }
    }

    fn find<'a>(report: &'a Report, section: &str, label: &str) -> &'a Finding {
        report
            .sections
            .iter()
            .find(|found| found.title == section)
            .and_then(|found| found.findings.iter().find(|finding| finding.label == label))
            .unwrap_or_else(|| panic!("no {label} in {section}: {report:#?}"))
    }

    #[test]
    fn a_dns_block_is_a_problem_that_leads_to_the_dns_page() {
        let report = report(&inputs(None, true));
        let lookup = find(&report, "Name and connection", "Name lookup");
        assert_eq!(lookup.level, Level::Problem);
        assert_eq!(lookup.remedy.unwrap().page, Some(FixedOn::DnsFilters));
        assert_eq!(find(&report, "Name and connection", "Connection").level, Level::Unknown);
        assert_eq!(find(&report, "This computer", "Protection").level, Level::Healthy);
    }

    #[test]
    fn a_blocking_rule_is_a_fact_with_the_way_to_allow_the_site() {
        let seen = Seen {
            total: 3,
            counts: crate::activity::Counts { passed: 1, blocked: 2, ..Default::default() },
            rules: vec![(2, "||ads.example.com^".to_owned(), Action::Blocked, 2)],
            ..Seen::default()
        };
        let report = report(&inputs(Some(seen), true));
        let rule = find(&report, "Filtering", "Rule");
        assert_eq!(rule.level, Level::Fact);
        assert!(rule.value.contains("AdGuard Base filter, blocked 2 times"), "{}", rule.value);
        assert_eq!(rule.remedy.unwrap().page, Some(FixedOn::WebFilters));
        let requests = find(&report, "Filtering", "Requests");
        assert!(requests.value.contains("1 passed, 2 blocked"), "{}", requests.value);
        assert_eq!(requests.remedy.unwrap().page, Some(FixedOn::Activity));
        assert!(find(&report, "HTTPS", "Exclusions").value.starts_with("Listed as example.com"));
        assert_eq!(find(&report, "Filtering", "Your rule").value, "||example.com^");
    }

    #[test]
    fn quic_with_http3_off_leads_to_the_switch() {
        let seen = Seen { total: 4, quic: 4, quic_uninspected: 3, ..Seen::default() };
        let off = report(&inputs(Some(seen.clone()), false));
        assert_eq!(find(&off, "HTTP/3", "QUIC").remedy.unwrap().page, Some(FixedOn::AdvancedHttp3));
        let on = report(&inputs(Some(seen), true));
        let quic = find(&on, "HTTP/3", "QUIC");
        assert_eq!(quic.remedy.unwrap().hint, QUIC_UNINSPECTED);
        assert_eq!(quic.level, Level::Fact, "never a fault: see the module header");
    }

    #[test]
    fn a_site_the_log_never_saw_is_unknown_rather_than_fine() {
        let report = report(&inputs(Some(Seen::default()), true));
        assert_eq!(find(&report, "Filtering", "Requests").level, Level::Unknown);
    }
}
