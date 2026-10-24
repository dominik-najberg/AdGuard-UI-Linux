//! Which of the filter rules that are switched on would decide an address,
//! read from the lists themselves: the half of the website check that does not
//! need the site to have been visited.
//!
//! AdGuard CLI has no command that answers "what would you do with this
//! address" — `--help-all` on 1.5.0 lists none — and its log knows only the
//! requests that were made, for as long as it keeps them. So this reads the
//! rule text AdGuard stores for every list that is on (`rules_list` in
//! `agflm_standard.db`, [`Catalogue::installed_rules`]) and `user.txt`, and
//! matches the address against them with AdGuard's basic-rule syntax as its
//! knowledge base describes it. **It is this application's reading of that
//! syntax, not AdGuard's engine**, and the page that shows it says so; where
//! the log has the site, the log is what happened.
//!
//! # The question it answers
//!
//! *Would opening this address as a page be blocked, and by which rule?* A
//! page has a request type — document — is first-party to itself and is
//! fetched with GET, so `$domain=` is read against the address's own name and
//! `$third-party` rules do not decide it. A rule that matches the address but
//! only in another shape — as a script, as a pop-up, loaded by another site —
//! is still listed, with when it applies, because "a pop-up blocker closed the
//! tab" and "a site's script was blocked" are both what a user calls a blocked
//! page.
//!
//! # Priority, measured
//!
//! An exception beats a blocking rule, and an `$important` blocking rule beats
//! an exception unless the exception is `$important` too. The second half was
//! measured on 24 October 2026 against v1.5.0, through the manual proxy with
//! rules in `user.txt`: `||example.com^$important` blocked the page with
//! `@@||example.com^$document` beside it, and stopped blocking it once that
//! exception was `$document,important` (contract §6, *Allowing a site*).
//!
//! # What it leaves out, and says it leaves out
//!
//! - **Cosmetic, HTML and scriptlet rules** — they change a page rather than
//!   block it, and most lines in a list are one.
//! - **Rules that modify rather than block**: `$csp`, `$redirect`,
//!   `$removeparam`, `$replace` and the rest. They can break a page; they
//!   cannot stop it opening.
//! - **Regular-expression rules**, 469 of the 129,138 network rules on the
//!   reference machine. They are counted, and the count is shown.
//! - **Rules limited to named programs** by `$app=`. The page is opened by a
//!   browser, and on the reference machine those rules name Android apps —
//!   AdGuard Mobile Ads carries `@@*$app=com.cinemark.mobile`, which read as
//!   applying allowed every address there is. A rule that only excludes
//!   programs (`$app=~…`) still applies.
//! - **DNS filtering**, which is another engine with its own lists. The
//!   website check's name lookup is what sees it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use crate::filters::RulesStamp;
use crate::model::Filter;
use crate::{Catalogue, FilterSet, Locale};

/// The user's own rules, as the log and the catalogue number them.
pub const USER_RULES: i64 = Filter::USER_RULES_ID;

/// What [`Lists::current`] compares: the catalogue's stamp, and `user.txt`'s
/// modification time and length.
type Stamp = (Vec<RulesStamp>, Option<(SystemTime, u64)>);

/// How many rules besides the deciding ones [`Check::matches`] keeps.
const MATCHES: usize = 8;

/// An address to check: what was typed, with a scheme and a path supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// As matched, with the scheme and host in lower case.
    pub url: String,
    host_start: usize,
    host_end: usize,
}

impl Address {
    /// `example.com`, `https://Example.com/a?b` and `example.com/path` all
    /// work; a bare name gets `https://` and `/`. `None` when there is no
    /// site's name in it, by [`crate::site::host`]'s measure.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let host = crate::site::host(input)?;
        let (scheme, rest) = match input.split_once("://") {
            Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
            None => ("https".to_owned(), input),
        };
        let cut = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(cut);
        let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        let port = match authority.rsplit_once(':') {
            Some((_, port)) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => {
                format!(":{port}")
            }
            _ => String::new(),
        };
        let tail = if tail.starts_with('/') { tail.to_owned() } else { format!("/{tail}") };
        let (open, close) = if host.contains(':') { ("[", "]") } else { ("", "") };
        let host_start = scheme.len() + 3 + open.len();
        Some(Self {
            url: format!("{scheme}://{open}{host}{close}{port}{tail}"),
            host_start,
            host_end: host_start + host.len(),
        })
    }

    pub fn host(&self) -> &str {
        &self.url[self.host_start..self.host_end]
    }
}

/// One rule that matches the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The list it is in: a filter id, or [`USER_RULES`].
    pub filter: i64,
    pub rule: String,
    /// An `@@` rule, which lets the address through rather than blocking it.
    pub exception: bool,
    pub important: bool,
    /// Whether it decides the address opened as a page. When it does not,
    /// [`Self::when`] says when it applies instead.
    pub page: bool,
    kinds: u32,
    third_party_only: bool,
}

impl Match {
    /// When the rule applies, for a rule that does not decide the page itself:
    /// "only as a script or image", "only when another site loads it".
    pub fn when(&self) -> Option<String> {
        if self.page {
            return None;
        }
        let every = ALL & !POPUP & !DOCUMENT;
        let kinds = self.kinds & !DOCUMENT;
        let words: Vec<&str> = KINDS
            .iter()
            .filter(|(bit, _)| kinds & bit != 0)
            .map(|(_, word)| *word)
            .collect();
        let shape = match (kinds & every == every, words.as_slice()) {
            (true, _) | (_, []) => None,
            (false, [only]) => Some(format!("only as a {only}")),
            (false, [rest @ .., last]) => Some(format!("only as a {} or {last}", rest.join(", "))),
        };
        Some(match (shape, self.third_party_only) {
            (Some(shape), true) => format!("{shape}, loaded by another site"),
            (Some(shape), false) => shape,
            (None, true) => "only when another site loads it".to_owned(),
            (None, false) => "not when opened as a page".to_owned(),
        })
    }
}

/// What the lists decide about the address opened as a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Blocked(Match),
    /// A blocking rule matches and an exception overrides it.
    Allowed { block: Match, exception: Match },
    /// No blocking rule decides the page.
    Clear,
}

/// The result of matching one address against every list that is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub verdict: Verdict,
    /// The other rules that match, ones that decide the page first, at most
    /// [`MATCHES`]. The verdict's own rules are not repeated here.
    pub matches: Vec<Match>,
    /// How many more matched than [`Self::matches`] holds.
    pub more: usize,
    /// How many rules are regular expressions, which are not read.
    pub regex: usize,
    /// How many rules were read, regular expressions included.
    pub rules: usize,
}

/// The rules of every installed list and `user.txt`. Those switched off are
/// kept to say which list a logged rule is in ([`Self::source`]) and left out
/// of matching ([`Self::check`]).
#[derive(Debug, Default)]
pub struct Lists {
    lists: Vec<(i64, String)>,
    /// The lists in [`Self::lists`] that are switched off.
    off: HashSet<i64>,
}

impl Lists {
    /// Read the lists from AdGuard's database and `user.txt`. **Blocking**: a
    /// few tens of megabytes of text.
    pub fn read() -> Result<Self, String> {
        let catalogue = Catalogue::open_set(FilterSet::Http).map_err(|err| err.to_string())?;
        let mut lists = Vec::new();
        let mut off = HashSet::new();
        for (id, enabled, text) in catalogue.installed_rules().map_err(|err| err.to_string())? {
            if !enabled {
                off.insert(id);
            }
            lists.push((id, text));
        }
        let user_rules_on = catalogue
            .user_rules(&Locale::from_env())
            .ok()
            .flatten()
            .is_some_and(|filter| filter.enabled);
        if let Some(text) = crate::paths::user_rules_file().and_then(|path| std::fs::read_to_string(path).ok()) {
            if !user_rules_on {
                off.insert(USER_RULES);
            }
            lists.push((USER_RULES, text));
        }
        Ok(Self { lists, off })
    }

    /// From lists already in hand, all switched on, for tests.
    pub fn from_lists(lists: Vec<(i64, String)>) -> Self {
        Self { lists, off: HashSet::new() }
    }

    /// As [`Self::from_lists`], with the lists `off` switched off.
    pub fn with_off(mut self, off: impl IntoIterator<Item = i64>) -> Self {
        self.off.extend(off);
        self
    }

    /// The lists that are switched on, for matching.
    fn on(&self) -> impl Iterator<Item = &(i64, String)> {
        self.lists.iter().filter(|(id, _)| !self.off.contains(id))
    }

    /// [`Self::read`], kept for as long as the lists that are on and
    /// `user.txt` are unchanged. The Activity page asks on every refresh and
    /// every search, and a read takes a tenth of a second to a second.
    ///
    /// Unchanged is [`Catalogue::installed_rules_stamp`] — a list installed,
    /// switched on or off, updated or with a rule switched off in it — and
    /// `user.txt`'s modification time and length. Callers wait for a read already under
    /// way rather than start a second one.
    pub fn current() -> Result<Arc<Self>, String> {
        static CACHE: Mutex<Option<(Stamp, Arc<Lists>)>> = Mutex::new(None);
        let catalogue = Catalogue::open_set(FilterSet::Http).map_err(|err| err.to_string())?;
        let user = crate::paths::user_rules_file()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        let stamp = (catalogue.installed_rules_stamp().map_err(|err| err.to_string())?, user);
        drop(catalogue);
        let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((seen, lists)) = cache.as_ref() {
            if *seen == stamp {
                return Ok(lists.clone());
            }
        }
        let lists = Arc::new(Self::read()?);
        *cache = Some((stamp, lists.clone()));
        Ok(lists)
    }

    /// The list a rule from AdGuard's log is really in.
    ///
    /// **The log's `ID=` is not always the list.** Measured 24 October 2026 on
    /// v1.5.0: a rule written to `user.txt` was logged as `ID=2`, which is
    /// AdGuard Base's id, while rules from the catalogue's lists were logged
    /// under their own ids. So a rule the logged list does not hold and
    /// `user.txt` does is the user's own. `None` when neither holds it any
    /// more — an exception the user allowed and took back is in the log under
    /// `ID=2` for days afterwards. A list switched off since still holds its
    /// rules: the log is history, and the rule was that list's.
    pub fn source(&self, logged: i64, rule: &str) -> Option<i64> {
        let holds = |id: i64| {
            self.lists
                .iter()
                .filter(|(list, _)| *list == id)
                .any(|(_, text)| text.lines().any(|line| line.trim() == rule))
        };
        if holds(logged) {
            Some(logged)
        } else if holds(USER_RULES) {
            Some(USER_RULES)
        } else {
            None
        }
    }

    pub fn check(&self, address: &Address) -> Check {
        let lower = address.url.to_ascii_lowercase();
        let disabled = self.badfiltered();
        let mut found: Vec<Match> = Vec::new();
        let mut regex = 0;
        let mut rules = 0;
        for (filter, text) in self.on() {
            for line in text.lines() {
                let line = line.trim();
                let Some(rule) = Rule::parse(line) else { continue };
                rules += 1;
                if rule.regex {
                    regex += 1;
                    continue;
                }
                if rule.badfilter || disabled.contains(line) {
                    continue;
                }
                if let Some(found_match) = rule.decide(*filter, line, address, &lower) {
                    found.push(found_match);
                }
            }
        }
        let verdict = verdict(&found);
        let mut matches: Vec<Match> = found
            .into_iter()
            .filter(|found| match &verdict {
                Verdict::Blocked(block) => found != block,
                Verdict::Allowed { block, exception } => found != block && found != exception,
                Verdict::Clear => true,
            })
            .collect();
        matches.sort_by_key(|found| !found.page);
        let more = matches.len().saturating_sub(MATCHES);
        matches.truncate(MATCHES);
        Check { verdict, matches, more, regex, rules }
    }

    /// The rules a `$badfilter` rule switches off, as their text.
    fn badfiltered(&self) -> HashSet<String> {
        let mut off = HashSet::new();
        for (_, text) in self.on() {
            for line in text.lines().map(str::trim).filter(|line| line.contains("badfilter")) {
                let Some((head, modifiers)) = line.rsplit_once('$') else { continue };
                let kept: Vec<&str> = modifiers.split(',').filter(|m| m.trim() != "badfilter").collect();
                if kept.len() == modifiers.split(',').count() {
                    continue;
                }
                off.insert(if kept.is_empty() { head.to_owned() } else { format!("{head}${}", kept.join(",")) });
            }
        }
        off
    }
}

/// Which rule decides the page: an `$important` exception over an
/// `$important` block over any exception over a block. An exception from the
/// user's own rules is preferred when several would do, since that is the one
/// the user wrote.
fn verdict(found: &[Match]) -> Verdict {
    let page = |exception: bool, important: bool| {
        let mut candidates = found
            .iter()
            .filter(move |found| found.page && found.exception == exception && (found.important || !important));
        let first = candidates.clone().next();
        candidates.find(|found| found.filter == USER_RULES).or(first).cloned()
    };
    let decided = |block: Match, exception: Option<Match>| match exception {
        Some(exception) => Verdict::Allowed { block, exception },
        None => Verdict::Blocked(block),
    };
    if let Some(block) = page(false, true) {
        decided(block, page(true, true))
    } else if let Some(block) = page(false, false) {
        decided(block, page(true, false))
    } else {
        Verdict::Clear
    }
}

const DOCUMENT: u32 = 1 << 0;
const SUBDOCUMENT: u32 = 1 << 1;
const SCRIPT: u32 = 1 << 2;
const IMAGE: u32 = 1 << 3;
const STYLESHEET: u32 = 1 << 4;
const OBJECT: u32 = 1 << 5;
const XHR: u32 = 1 << 6;
const FONT: u32 = 1 << 7;
const MEDIA: u32 = 1 << 8;
const WEBSOCKET: u32 = 1 << 9;
const PING: u32 = 1 << 10;
const OTHER: u32 = 1 << 11;
const POPUP: u32 = 1 << 12;
const ALL: u32 = (1 << 13) - 1;

/// The request types in the words [`Match::when`] uses, in its order.
const KINDS: [(u32, &str); 12] = [
    (SCRIPT, "script"),
    (IMAGE, "image"),
    (STYLESHEET, "style sheet"),
    (SUBDOCUMENT, "frame"),
    (XHR, "page's own request"),
    (FONT, "font"),
    (MEDIA, "media file"),
    (OBJECT, "plugin"),
    (WEBSOCKET, "WebSocket"),
    (PING, "ping"),
    (OTHER, "other request"),
    (POPUP, "pop-up"),
];

/// A network rule, as far as deciding an address needs it.
#[derive(Debug, Default)]
struct Rule<'a> {
    exception: bool,
    pattern: &'a str,
    regex: bool,
    /// Request types it is limited to, and excluded from. `0` for no limit.
    types: u32,
    not_types: u32,
    third_party: Option<bool>,
    domains: Option<&'a str>,
    to: Option<&'a str>,
    denyallow: Option<&'a str>,
    methods: Option<&'a str>,
    important: bool,
    match_case: bool,
    badfilter: bool,
}

impl<'a> Rule<'a> {
    /// `None` for anything that is not a network rule this can decide with:
    /// comments, cosmetic rules, rules that modify rather than block, and rules
    /// with a modifier it does not know, which AdGuard would refuse too.
    fn parse(line: &'a str) -> Option<Self> {
        if line.is_empty() || line.starts_with(['!', '[', '#']) || cosmetic(line) {
            return None;
        }
        let (exception, body) = match line.strip_prefix("@@") {
            Some(body) => (true, body),
            None => (false, line),
        };
        let mut rule = Rule { exception, ..Rule::default() };
        let modifiers;
        if body.starts_with('/') && body.len() > 1 && (body.ends_with('/') || body.contains("/$")) {
            rule.regex = true;
            rule.pattern = body;
            return Some(rule);
        }
        match body.rsplit_once('$') {
            Some((pattern, tail)) => {
                rule.pattern = pattern;
                modifiers = tail;
            }
            None => {
                rule.pattern = body;
                modifiers = "";
            }
        }
        // Exceptions that only switch off hiding or injection do not let a
        // request through, so they decide nothing here — unless they also
        // name `$document` or `$urlblock`.
        let mut hides_only = false;
        let mut unblocks = false;
        for modifier in modifiers.split(',').map(str::trim).filter(|m| !m.is_empty()) {
            let (negated, modifier) = match modifier.strip_prefix('~') {
                Some(rest) => (true, rest),
                None => (false, modifier),
            };
            let (name, value) = match modifier.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (modifier, None),
            };
            let kind = match name {
                "document" | "doc" => DOCUMENT,
                "subdocument" | "frame" => SUBDOCUMENT,
                "script" => SCRIPT,
                "image" => IMAGE,
                "stylesheet" | "css" => STYLESHEET,
                "object" => OBJECT,
                "xmlhttprequest" | "xhr" => XHR,
                "font" => FONT,
                "media" => MEDIA,
                "websocket" => WEBSOCKET,
                "ping" => PING,
                "other" => OTHER,
                "popup" => POPUP,
                "all" => ALL,
                _ => 0,
            };
            if kind != 0 {
                if negated {
                    rule.not_types |= kind;
                } else {
                    rule.types |= kind;
                }
                if kind & DOCUMENT != 0 {
                    unblocks = true;
                }
                continue;
            }
            match (name, value) {
                ("third-party" | "3p", None) => rule.third_party = Some(!negated),
                ("first-party" | "1p", None) => rule.third_party = Some(negated),
                ("strict-third-party" | "strict3p", None) => rule.third_party = Some(true),
                ("strict-first-party" | "strict1p", None) => rule.third_party = Some(false),
                ("domain" | "from", Some(value)) => rule.domains = Some(value),
                ("to", Some(value)) => rule.to = Some(value),
                ("denyallow", Some(value)) => rule.denyallow = Some(value),
                ("method", Some(value)) => rule.methods = Some(value),
                ("important", None) => rule.important = true,
                ("match-case", None) => rule.match_case = !negated,
                ("badfilter", None) => rule.badfilter = true,
                ("app", Some(value)) => {
                    if value.split('|').any(|app| !app.trim().starts_with('~')) {
                        return None;
                    }
                }
                ("urlblock", None) if exception => unblocks = true,
                (
                    "elemhide" | "ehide" | "generichide" | "ghide" | "specifichide" | "shide"
                    | "jsinject" | "content" | "extension" | "stealth" | "genericblock",
                    _,
                ) if exception => hides_only = true,
                _ => return None,
            }
        }
        if hides_only && !unblocks {
            return None;
        }
        Some(rule)
    }

    /// The match, if this rule — `line` — applies to `address`. `lower` is
    /// its URL in lower case.
    fn decide(&self, filter: i64, line: &str, address: &Address, lower: &str) -> Option<Match> {
        let url = if self.match_case { address.url.as_str() } else { lower };
        if !self.pattern_matches(url, address) {
            return None;
        }
        let host = address.host();
        if let Some(domains) = self.domains {
            if !listed_or_unlimited(domains, host) {
                return None;
            }
        }
        if let Some(to) = self.to {
            if !listed_or_unlimited(to, host) {
                return None;
            }
        }
        if let Some(denyallow) = self.denyallow {
            if denyallow.split('|').any(|entry| domain_matches(entry, host)) {
                return None;
            }
        }
        if let Some(methods) = self.methods {
            if !listed_or_unlimited_word(methods, "get") {
                return None;
            }
        }
        let kinds = if self.types == 0 { ALL & !POPUP } else { self.types } & !self.not_types;
        if kinds == 0 {
            return None;
        }
        let third_party_only = self.third_party == Some(true);
        Some(Match {
            filter,
            rule: line.to_owned(),
            exception: self.exception,
            important: self.important,
            page: kinds & DOCUMENT != 0 && !third_party_only,
            kinds,
            third_party_only,
        })
    }

    fn pattern_matches(&self, url: &str, address: &Address) -> bool {
        let mut pattern = self.pattern;
        let whole = pattern.len() > 1 && pattern.ends_with('|') && !pattern.ends_with("||");
        if whole {
            pattern = &pattern[..pattern.len() - 1];
        }
        if pattern.is_empty() || pattern == "*" {
            return true;
        }
        // The longest stretch of plain text has to be in the address, which
        // rules out almost every rule before any matching is done.
        let literal = pattern
            .split(['*', '^', '|'])
            .max_by_key(|part| part.len())
            .unwrap_or("");
        if !literal.is_empty() {
            let found = if self.match_case || !literal.bytes().any(|byte| byte.is_ascii_uppercase()) {
                url.contains(literal)
            } else {
                url.contains(&literal.to_ascii_lowercase())
            };
            if !found {
                return false;
            }
        }
        let fold = !self.match_case;
        let url = url.as_bytes();
        if let Some(rest) = pattern.strip_prefix("||") {
            let host = &address.url.as_bytes()[address.host_start..address.host_end];
            let starts = std::iter::once(address.host_start).chain(
                host.iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'.')
                    .map(|(at, _)| address.host_start + at + 1),
            );
            for start in starts {
                if glob(rest.as_bytes(), &url[start..], whole, fold) {
                    return true;
                }
            }
            false
        } else if let Some(rest) = pattern.strip_prefix('|') {
            glob(rest.as_bytes(), url, whole, fold)
        } else {
            (0..url.len()).any(|start| glob(pattern.as_bytes(), &url[start..], whole, fold))
        }
    }
}

/// Whether `pattern` matches at the start of `text` — the whole of it when
/// `whole`. `*` is any run of characters, `^` a separator or the end.
fn glob(pattern: &[u8], text: &[u8], whole: bool, fold: bool) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if p == pattern.len() {
            if !whole || t == text.len() {
                return true;
            }
        } else {
            let want = pattern[p];
            if want == b'*' {
                star = Some((p, t));
                p += 1;
                continue;
            }
            if want == b'^' && t == text.len() {
                p += 1;
                continue;
            }
            if t < text.len() {
                let got = text[t];
                let same = if want == b'^' {
                    separator(got)
                } else if fold {
                    want.to_ascii_lowercase() == got
                } else {
                    want == got
                };
                if same {
                    p += 1;
                    t += 1;
                    continue;
                }
            }
        }
        match star {
            Some((at, from)) if from < text.len() => {
                star = Some((at, from + 1));
                p = at + 1;
                t = from + 1;
            }
            _ => return false,
        }
    }
}

/// AdGuard's separator: anything but a letter, a digit, `_`, `-`, `.` or `%`.
fn separator(byte: u8) -> bool {
    !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'%'))
}

/// A cosmetic, HTML-filtering or scriptlet rule, by its marker.
fn cosmetic(line: &str) -> bool {
    const MARKERS: [&str; 12] =
        ["##", "#@#", "#?#", "#@?#", "#$#", "#@$#", "#$?#", "#@$?#", "#%#", "#@%#", "$$", "$@$"];
    (line.contains('#') || line.contains("$$") || line.contains("$@$"))
        && MARKERS.iter().any(|marker| line.contains(marker))
}

/// A `$domain=`-style list: true when `host` is among the names it lists, or
/// when it lists only exclusions and `host` is not one of them.
fn listed_or_unlimited(list: &str, host: &str) -> bool {
    let mut positive = false;
    let mut listed = false;
    for entry in list.split('|').map(str::trim).filter(|entry| !entry.is_empty()) {
        match entry.strip_prefix('~') {
            Some(excluded) => {
                if domain_matches(excluded, host) {
                    return false;
                }
            }
            None => {
                positive = true;
                listed |= domain_matches(entry, host);
            }
        }
    }
    !positive || listed
}

/// As [`listed_or_unlimited`], for `$method=`'s plain words.
fn listed_or_unlimited_word(list: &str, word: &str) -> bool {
    let mut positive = false;
    let mut listed = false;
    for entry in list.split('|').map(str::trim) {
        match entry.strip_prefix('~') {
            Some(excluded) if excluded.eq_ignore_ascii_case(word) => return false,
            Some(_) => {}
            None => {
                positive = true;
                listed |= entry.eq_ignore_ascii_case(word);
            }
        }
    }
    !positive || listed
}

/// Whether a `$domain=` entry covers `host`: the name or one under it, and
/// `example.*` for `example` under any top-level domain. A regular-expression
/// entry covers nothing here.
fn domain_matches(entry: &str, host: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    if entry.is_empty() || entry.starts_with('/') {
        return false;
    }
    if let Some(base) = entry.strip_suffix('*') {
        // `example.` must start the name or follow a dot, with something after.
        return host
            .match_indices(base)
            .any(|(at, _)| (at == 0 || host.as_bytes()[at - 1] == b'.') && host.len() > at + base.len());
    }
    host == entry || host.strip_suffix(entry.as_str()).is_some_and(|head| head.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(input: &str) -> Address {
        Address::parse(input).unwrap_or_else(|| panic!("{input} did not parse"))
    }

    fn lists(user: &str) -> Lists {
        Lists::from_lists(vec![
            (
                2,
                "! Title: AdGuard Base filter\n||ads.example.net^\n||tracker.example.org^$third-party\n\
                 ||example.com/banner/*$image,script\n||popups.example^$popup\nexample.com##.ad\n\
                 /banner[0-9]+/\n||blocked.example^$important\n||gone.example^\n||gone.example^$badfilter\n\
                 ||csp.example^$csp=script-src 'none'\n||limited.example^$domain=other.example\n\
                 ||posted.example^$method=post\n@@*$app=com.example.android\n||apps.example^$app=~chrome\n"
                    .to_owned(),
            ),
            (USER_RULES, user.to_owned()),
        ])
    }

    #[test]
    fn an_address_gets_a_scheme_and_a_path() {
        assert_eq!(address("example.com").url, "https://example.com/");
        assert_eq!(address("HTTP://Example.COM:8080/A?b").url, "http://example.com:8080/A?b");
        assert_eq!(address("example.com?q").url, "https://example.com/?q");
        assert_eq!(address("www.example.com/x").host(), "www.example.com");
        assert_eq!(Address::parse("localhost"), None);
    }

    #[test]
    fn a_domain_anchor_matches_the_name_and_names_under_it_only() {
        let check = lists("").check(&address("https://sub.ads.example.net/x"));
        let Verdict::Blocked(block) = check.verdict else { panic!("{check:?}") };
        assert_eq!(block.filter, 2);
        assert!(block.page);
        assert_eq!(lists("").check(&address("notads.example.net")).verdict, Verdict::Clear);
    }

    #[test]
    fn a_third_party_rule_is_listed_but_does_not_block_the_page() {
        let check = lists("").check(&address("tracker.example.org"));
        assert_eq!(check.verdict, Verdict::Clear);
        let only = &check.matches[0];
        assert!(!only.page);
        assert_eq!(only.when().as_deref(), Some("only when another site loads it"));
    }

    #[test]
    fn a_rule_limited_to_kinds_says_which() {
        let check = lists("").check(&address("example.com/banner/top.png"));
        assert_eq!(check.verdict, Verdict::Clear);
        assert_eq!(check.matches[0].when().as_deref(), Some("only as a script or image"));
        let popup = lists("").check(&address("popups.example"));
        assert_eq!(popup.matches[0].when().as_deref(), Some("only as a pop-up"));
    }

    #[test]
    fn an_exception_beats_a_block_and_an_important_block_beats_a_plain_exception() {
        let allowed = lists("@@||ads.example.net^$document").check(&address("ads.example.net"));
        let Verdict::Allowed { block, exception } = allowed.verdict else { panic!("{allowed:?}") };
        assert_eq!(block.filter, 2);
        assert_eq!(exception.filter, USER_RULES);

        // Measured: see the module header.
        let plain = lists("@@||blocked.example^$document").check(&address("blocked.example"));
        assert!(matches!(plain.verdict, Verdict::Blocked(_)), "{plain:?}");
        let important = lists(&crate::allowlist::rule("blocked.example")).check(&address("blocked.example"));
        assert!(matches!(important.verdict, Verdict::Allowed { .. }), "{important:?}");
    }

    #[test]
    fn hiding_exceptions_let_nothing_through() {
        let check = lists("@@||ads.example.net^$elemhide").check(&address("ads.example.net"));
        assert!(matches!(check.verdict, Verdict::Blocked(_)), "{check:?}");
    }

    #[test]
    fn what_cannot_be_decided_is_left_out_and_regular_expressions_are_counted() {
        let check = lists("").check(&address("csp.example"));
        assert_eq!(check.verdict, Verdict::Clear);
        assert!(check.matches.is_empty(), "$csp modifies, it does not block: {check:?}");
        assert_eq!(check.regex, 1);
        assert_eq!(lists("").check(&address("gone.example")).verdict, Verdict::Clear, "$badfilter");
        assert_eq!(lists("").check(&address("limited.example")).verdict, Verdict::Clear, "$domain");
        assert_eq!(lists("").check(&address("posted.example")).verdict, Verdict::Clear, "$method");
        assert!(
            matches!(lists("").check(&address("ads.example.net")).verdict, Verdict::Blocked(_)),
            "an exception for a named app does not apply to a browser"
        );
        assert!(
            matches!(lists("").check(&address("apps.example")).verdict, Verdict::Blocked(_)),
            "a rule that only excludes an app applies"
        );
    }

    #[test]
    fn glob_reads_wildcards_separators_and_anchors() {
        assert!(glob(b"example.com^", b"example.com/", false, true));
        assert!(glob(b"example.com^", b"example.com", false, true), "^ matches the end");
        assert!(!glob(b"example.com^", b"example.company", false, true));
        assert!(glob(b"a*c", b"abbbc", true, true));
        assert!(!glob(b"a*c", b"abbbcd", true, true));
        assert!(glob(b"ADS", b"ads", false, true));
        assert!(!glob(b"ADS", b"ads", false, false));
    }

    #[test]
    fn domain_entries_cover_names_under_them_and_any_top_level_domain() {
        assert!(domain_matches("example.com", "www.example.com"));
        assert!(!domain_matches("example.com", "badexample.com"));
        assert!(domain_matches("example.*", "www.example.co.uk"));
        assert!(!domain_matches("example.*", "myexample.com"));
        assert!(listed_or_unlimited("~other.com", "example.com"));
        assert!(!listed_or_unlimited("a.com|~b.a.com", "b.a.com"));
    }

    #[test]
    fn a_logged_user_rule_is_attributed_to_the_user() {
        let lists = lists("||example.com^");
        assert_eq!(lists.source(2, "||example.com^"), Some(USER_RULES), "measured: logged as ID=2");
        assert_eq!(lists.source(2, "||ads.example.net^"), Some(2));
        assert_eq!(lists.source(2, "@@||taken.back^$document,important"), None);
    }

    /// A list switched off decides nothing, and still holds the rules the log
    /// has from it; so do the user's rules, switched off.
    #[test]
    fn a_list_switched_off_still_holds_its_rules() {
        let off = lists("||mine.example^").with_off([2, USER_RULES]);
        assert_eq!(off.check(&address("ads.example.net")).verdict, Verdict::Clear);
        assert_eq!(off.check(&address("mine.example")).verdict, Verdict::Clear);
        assert_eq!(off.source(2, "||ads.example.net^"), Some(2));
        assert_eq!(off.source(2, "||mine.example^"), Some(USER_RULES));
    }
}
