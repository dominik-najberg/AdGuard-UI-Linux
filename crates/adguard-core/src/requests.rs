//! Single requests, found in AdGuard's own access log: the search on the
//! Activity page.
//!
//! The first item of [issue #21] asked for a searchable request history. The
//! history this application keeps is counts and never requests — the owner's
//! decision, and [`crate::activity`]'s header — so a search cannot be over
//! anything this application has stored. It is over **the log AdGuard already
//! keeps**, read in place every time, and nothing it finds is written
//! anywhere. It reaches back exactly as far as AdGuard does, a few days, and
//! [`Found::reaches_back`] says how far that is on this machine today.
//!
//! What a hit carries is what the log line carries, including a page's full
//! address, because the point of a search is to find *the* request — the one a
//! site broke on. That is a different thing from keeping it: the log is
//! AdGuard's, on the same disk, and this reads it the way `less` would.
//!
//! The parse is [`crate::activity`]'s, with its guards, so a line that page
//! would count as unread is not a hit here either.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::path::Path;

use crate::activity::{self, Action, Clock};

/// How many hits a search returns when the caller does not say.
///
/// A page of rows, not a report: past fifty the answer to "why did this
/// break" is a narrower search, and the page says the list was cut.
pub const LIMIT: usize = 50;

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Words that must all appear, case-insensitively, in the request's
    /// address, its program or the rule that decided it. Empty matches every
    /// request, which is the newest requests first.
    pub text: String,
    /// Only requests that ended this way, or any.
    pub action: Option<Action>,
    /// At most this many hits, newest first. Zero means [`LIMIT`].
    pub limit: usize,
}

/// One request, as the log has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Microseconds since the epoch.
    pub at: i64,
    /// The program, as AdGuard names it.
    pub client: String,
    /// `HTTP1`, `HTTP2`, `TLS`, `IQUIC`, …
    pub protocol: String,
    /// A full URL for HTTP, a bare host for TLS and QUIC, `None` for neither.
    pub target: Option<String>,
    pub host: Option<String>,
    pub status: Option<u16>,
    pub action: Action,
    /// The filter list's id in `agflm_standard.db`, when a rule decided it.
    pub filter: Option<i64>,
    pub rule: Option<String>,
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// Newest first.
    pub hits: Vec<Hit>,
    /// The search stopped at its limit, so older matches were not looked for.
    pub more: bool,
    /// Lines read, matching or not.
    pub scanned: u64,
    /// The oldest request AdGuard still has, in microseconds: the earliest a
    /// hit could possibly be. `None` when there is no log.
    pub reaches_back: Option<i64>,
}

/// Search the log whose live file is `live`, newest request first.
///
/// A log that is not there is an empty result, not an error: there is nothing
/// to search on a machine where AdGuard has never run.
pub fn search(live: &Path, query: &Query) -> Found {
    let limit = if query.limit == 0 { LIMIT } else { query.limit };
    let terms: Vec<String> = query
        .text
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let mut found = Found::default();
    let mut clock = Clock::default();

    let generations = activity::generations(live);
    // The oldest generation's first line is the oldest request there is. Read
    // separately, because the search below usually stops long before it.
    if let Some(oldest) = generations.first() {
        found.reaches_back = first_request(&oldest.file, &mut clock);
    }

    // Newest generation first, and each one from its end.
    'files: for generation in generations.iter().rev() {
        let Some(bytes) = activity::read_from(&generation.file, 0) else {
            continue;
        };
        for line in bytes.split(|&byte| byte == b'\n').rev() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            found.scanned += 1;
            // The cheap test first: a line that does not contain every word
            // anywhere cannot match in any one field. Most lines stop here.
            if !terms.is_empty() {
                let lower = line.to_ascii_lowercase();
                if !terms.iter().all(|term| contains(&lower, term.as_bytes())) {
                    continue;
                }
            }
            let Ok(text) = std::str::from_utf8(line) else { continue };
            let Some(request) = activity::parse(text, &mut clock) else {
                continue;
            };
            if query.action.is_some_and(|action| action != request.action) {
                continue;
            }
            // The exact test: every word in a field a user can mean — never
            // the date, the referrer or the upstream, which the cheap test
            // could have matched on.
            if !terms.iter().all(|term| matches(&request, term)) {
                continue;
            }
            if found.hits.len() == limit {
                found.more = true;
                break 'files;
            }
            found.hits.push(Hit {
                at: request.at,
                client: request.client.to_owned(),
                protocol: request.protocol.to_owned(),
                target: request.target.map(str::to_owned),
                host: request.host,
                status: request.status,
                action: request.action,
                filter: request.filter,
                rule: request.rule.map(str::to_owned),
            });
        }
    }
    found
}

/// Whether one lower-cased word is in a field the user can mean.
fn matches(request: &activity::Request, term: &str) -> bool {
    let fields = [
        request.target,
        request.host.as_deref(),
        Some(request.client),
        request.rule,
        Some(request.protocol),
    ];
    fields
        .into_iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(term))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|window| window == needle)
}

/// The timestamp of the first readable line in a file.
fn first_request(file: &std::fs::File, clock: &mut Clock) -> Option<i64> {
    let mut head = vec![0; 64 * 1024];
    let read = std::os::unix::fs::FileExt::read_at(file, &mut head, 0).ok()?;
    head.truncate(read);
    head.split(|&byte| byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .find_map(|line| activity::parse(line, clock))
        .map(|request| request.at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Lines in the shape `activity.rs`'s fixtures use, with reserved names.
    fn line(clock: &str, client: &str, url: &str, action: &str, rule: Option<(i64, &str)>) -> String {
        let (id, text) = match rule {
            Some((id, text)) => (format!("ID={id}"), format!(" {text}")),
            None => ("-".to_owned(), String::new()),
        };
        format!(
            "25.08.2026 {clock}.000001 \"{client}\" HTTP2 GET {url} - 200 xhr {action} 0 {id} 192.0.2.1:443 10b 1ms --{text}"
        )
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("adguard-ui-requests-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, lines: &[String]) -> &Self {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            fs::write(self.0.join(name), text).unwrap();
            self
        }

        fn live(&self) -> PathBuf {
            self.0.join("access.log")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.to_owned(),
            ..Query::default()
        }
    }

    fn targets(found: &Found) -> Vec<&str> {
        found.hits.iter().map(|hit| hit.target.as_deref().unwrap_or("-")).collect()
    }

    #[test]
    fn newest_first_across_generations() {
        let scratch = Scratch::new("order");
        scratch
            .write("access.log.1", &[
                line("09:00:00", "firefox", "https://a.example/", "NONE", None),
                line("09:30:00", "firefox", "https://b.example/", "NONE", None),
            ])
            .write("access.log", &[line("10:00:00", "firefox", "https://c.example/", "NONE", None)]);
        let found = search(&scratch.live(), &Query::default());
        assert_eq!(targets(&found), ["https://c.example/", "https://b.example/", "https://a.example/"]);
        assert!(!found.more);
        assert_eq!(found.scanned, 3);
        assert_eq!(found.reaches_back, Some(crate::access::epoch("25.08.2026", "09:00:00").unwrap() * 1_000_000 + 1));
    }

    #[test]
    fn every_word_must_match_and_case_does_not_matter() {
        let scratch = Scratch::new("words");
        scratch.write("access.log", &[
            line("10:00:00", "firefox", "https://ads.example.com/banner", "BLOCKED", Some((2, "||ads.example.com^"))),
            line("10:00:01", "chrome", "https://ads.example.com/pixel", "BLOCKED", Some((2, "||ads.example.com^"))),
            line("10:00:02", "chrome", "https://www.example.org/", "NONE", None),
        ]);
        assert_eq!(targets(&search(&scratch.live(), &query("ADS chrome"))), ["https://ads.example.com/pixel"]);
        assert_eq!(search(&scratch.live(), &query("example")).hits.len(), 3);
    }

    #[test]
    fn a_rule_is_searchable_and_is_carried_with_its_list() {
        let scratch = Scratch::new("rule");
        scratch.write("access.log", &[
            line("10:00:00", "firefox", "https://tracker.example/", "BLOCKED", Some((7, "/tracking-pixel/"))),
            line("10:00:01", "firefox", "https://www.example/", "NONE", None),
        ]);
        let found = search(&scratch.live(), &query("tracking-pixel"));
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].filter, Some(7));
        assert_eq!(found.hits[0].rule.as_deref(), Some("/tracking-pixel/"));
        assert_eq!(found.hits[0].action, Action::Blocked);
    }

    /// The cheap test reads the whole line; the exact one must not, or a word
    /// that only appears in the date, the referrer or the upstream would be a
    /// hit on every line.
    #[test]
    fn words_outside_the_searchable_fields_do_not_match() {
        let scratch = Scratch::new("fields");
        scratch.write("access.log", &[line("10:00:00", "firefox", "https://www.example/", "NONE", None)]);
        for elsewhere in ["25.08.2026", "192.0.2.1", "xhr"] {
            assert!(search(&scratch.live(), &query(elsewhere)).hits.is_empty(), "{elsewhere}");
        }
    }

    #[test]
    fn the_action_filter_narrows() {
        let scratch = Scratch::new("action");
        scratch.write("access.log", &[
            line("10:00:00", "firefox", "https://a.example/", "BLOCKED", Some((2, "||a.example^"))),
            line("10:00:01", "firefox", "https://b.example/", "NONE", None),
            line("10:00:02", "firefox", "https://c.example/", "WHITELISTED", Some((2, "@@||c.example^"))),
        ]);
        let only = |action| search(&scratch.live(), &Query { action: Some(action), ..Query::default() });
        assert_eq!(targets(&only(Action::Blocked)), ["https://a.example/"]);
        assert_eq!(targets(&only(Action::Allowed)), ["https://c.example/"]);
    }

    #[test]
    fn the_limit_stops_the_search_and_says_so() {
        let scratch = Scratch::new("limit");
        let lines: Vec<String> = (0..10)
            .map(|second| line(&format!("10:00:{second:02}"), "firefox", "https://a.example/", "NONE", None))
            .collect();
        scratch.write("access.log", &lines);
        let found = search(&scratch.live(), &Query { limit: 4, ..Query::default() });
        assert_eq!(found.hits.len(), 4);
        assert!(found.more);
        let exact = search(&scratch.live(), &Query { limit: 10, ..Query::default() });
        assert!(!exact.more, "exactly the limit is not more");
    }

    #[test]
    fn an_unreadable_line_is_never_a_hit() {
        let scratch = Scratch::new("unread");
        scratch.write("access.log", &["garbage mentioning a.example".to_owned()]);
        assert!(search(&scratch.live(), &query("a.example")).hits.is_empty());
    }

    #[test]
    fn no_log_is_nothing_found() {
        let scratch = Scratch::new("absent");
        assert_eq!(search(&scratch.live(), &Query::default()), Found::default());
    }
}
