//! What a userscript says about itself, read before anyone agrees to install it.
//!
//! An install link (#29) carries nothing but a URL, and a URL is a poor thing
//! to decide on: Greasy Fork's read
//! `…/1682/Google%20Hit%20Hider%20by%20Domain%20%28Search%20Filter%20%20Block%20Sites%29.user.js`.
//! Every userscript opens with a `// ==UserScript==` block that names it, gives
//! its version and description, and lists the sites it runs on — the same
//! fields the Extensions page shows once it is installed. So the confirmation
//! fetches the head of the script and shows those instead.
//!
//! **One request, before the user has said yes**, and only because they have
//! just clicked *Install* in a browser and accepted its prompt to open this
//! application. It reads at most [`LIMIT`] bytes, runs nothing, and sends
//! nothing about the user; AdGuard fetches the script again on *Add*.
//!
//! **That fetch is given the address the script was read from, not the one
//! clicked.** AdGuard CLI does not follow redirects: measured against 1.4.13 on
//! #29's install-counter link, which answers `302` with the script's
//! `raw.githubusercontent.com` address, it logs `Download failed with status
//! code: 302` and installs nothing. This fetch does follow them, so it reports
//! where it ended up ([`Fetched::url`]) — which is also the host the dialog has
//! to name, since that is whose code it is — and *Add* hands AdGuard that.
//!
//! **Everything in the block was written by the script's author.** A hostile
//! script can call itself *AdGuard Extra*, so the dialog never shows a name
//! without the host it came from, and every field is passed through [`clean`]:
//! control characters and the Unicode direction overrides that can make text
//! read differently from what it is are removed, and lengths are capped.

use std::io::Read;
use std::time::Duration;

/// Generous for a few kilobytes, and short, because a dialog is waiting on it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How much of the script is read. The metadata block comes first and is a few
/// kilobytes at most — Greasy Fork's whole script for #29's example is 162 KB,
/// and its block ends inside the first 2 KB.
pub const LIMIT: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("could not reach {0}")]
    Unreachable(String),

    #[error("the server answered {0}")]
    Status(u16),

    #[error("the address does not lead to a userscript")]
    NotAUserscript,
}

/// A script's metadata block, and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub preview: Preview,
    /// The address the script was served from: the one asked for, or where its
    /// redirects led. The one to give `userscripts install`, which follows
    /// none.
    pub url: String,
}

/// The fields of a metadata block the confirmation shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub runs_on: RunsOn,
}

/// Where a script says it runs, from its `@match` and `@include` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunsOn {
    /// A pattern that matches every site: `*://*/*`, `<all_urls>`, `*`.
    Everywhere,
    /// Sites, in the order the block names them, without repeats.
    Sites(Vec<String>),
    /// No `@match` or `@include` at all. Userscript managers differ on what
    /// that means, and the generous reading is *every site*, so it is not
    /// presented as *nowhere*.
    Unstated,
}

/// Fetch the head of the script at `url` and read its metadata block.
///
/// Blocking, for a worker thread.
pub fn fetch(url: &str) -> Result<Fetched, Error> {
    use ureq::ResponseExt;

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        // To tell a redirect from no redirect. The final URI alone cannot: it
        // is the request's own reparsed, and that is not always the string
        // that went in — which is the string to install when nothing moved.
        .save_redirect_history(true)
        // The application and its version, as the release check sends, and
        // nothing about the user.
        .user_agent(concat!("AdGuard-UI-Linux/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let host = host(url).unwrap_or_else(|| url.to_owned());
    let mut response = agent.get(url).call().map_err(|err| match err {
        ureq::Error::StatusCode(status) => Error::Status(status),
        _ => Error::Unreachable(host.clone()),
    })?;

    let moved = response.get_redirect_history().is_some_and(|history| history.len() > 1);
    let served = if moved { response.get_uri().to_string() } else { url.to_owned() };

    // `take` rather than the body's own limit, which fails a body longer than
    // the limit instead of stopping at it — and the block is at the top.
    let mut head = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(LIMIT)
        .read_to_end(&mut head)
        .map_err(|_| Error::Unreachable(host))?;

    let preview = parse(&String::from_utf8_lossy(&head)).ok_or(Error::NotAUserscript)?;
    Ok(Fetched { preview, url: served })
}

/// Read a `// ==UserScript==` block, or `None` if there is none.
///
/// A block cut off by [`LIMIT`] is read as far as it goes rather than refused:
/// the fields that arrived are still the script's own.
pub fn parse(source: &str) -> Option<Preview> {
    let mut lines = source.lines().map(str::trim);
    lines.find(|line| line.starts_with("//") && line.contains("==UserScript=="))?;

    let mut name = None;
    let mut english_name = None;
    let mut version = None;
    let mut description = None;
    let mut english_description = None;
    let mut patterns = Vec::new();

    for line in lines {
        if line.starts_with("//") && line.contains("==/UserScript==") {
            break;
        }
        let Some(tag) = line.strip_prefix("//").map(str::trim_start) else {
            continue;
        };
        let Some(tag) = tag.strip_prefix('@') else {
            continue;
        };
        let (key, value) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let value = value.trim();
        match key {
            "name" => name = name.or(clean(value, 120)),
            "name:en" => english_name = english_name.or(clean(value, 120)),
            "version" => version = version.or(clean(value, 40)),
            "description" => description = description.or(clean(value, 300)),
            "description:en" => english_description = english_description.or(clean(value, 300)),
            "match" | "include" => patterns.push(value.to_owned()),
            _ => {}
        }
    }

    Some(Preview {
        name: name.or(english_name),
        version,
        description: description.or(english_description),
        runs_on: runs_on(&patterns),
    })
}

/// Where a list of `@match`/`@include` patterns runs.
fn runs_on(patterns: &[String]) -> RunsOn {
    if patterns.is_empty() {
        return RunsOn::Unstated;
    }
    let mut sites: Vec<String> = Vec::new();
    for pattern in patterns {
        match site(pattern) {
            None => return RunsOn::Everywhere,
            Some(site) => {
                if !sites.contains(&site) {
                    sites.push(site);
                }
            }
        }
    }
    // `news.google.*` beside `google.*` says nothing a reader needs: the
    // second already reads as Google. Only for display — the patterns AdGuard
    // matches are the script's own.
    let listed = sites.clone();
    sites.retain(|site| {
        !listed
            .iter()
            .any(|other| other != site && site.ends_with(&format!(".{other}")))
    });
    RunsOn::Sites(sites)
}

/// The site a pattern names, or `None` for one that matches every site.
///
/// `https://*.google.com/*` and `*://www.google.com/search*` are both
/// `google.com`; `*://*/*` and `http*://*` are every site. A regular expression
/// `@include` (`/…/`) is shown as written, since reading one is not this
/// function's job and hiding it would be worse.
fn site(pattern: &str) -> Option<String> {
    let pattern = pattern.trim();
    if pattern == "*" || pattern == "<all_urls>" {
        return None;
    }
    if pattern.len() > 1 && pattern.starts_with('/') && pattern.ends_with('/') {
        return clean(pattern, 60);
    }
    let rest = pattern.split_once("://").map_or(pattern, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = host.split(':').next().unwrap_or("");
    let host = host.trim_start_matches("*.").trim_start_matches("www.");
    if host.is_empty() || host.chars().all(|c| c == '*') {
        return None;
    }
    clean(&host.to_lowercase(), 60)
}

/// The host a URL points at, as the browser would connect to it.
///
/// Credentials before an `@` are dropped, which is the point:
/// `https://greasyfork.org@example.net/x.user.js` is example.net's file.
pub fn host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal keeps its brackets and loses only the port.
        Some(v6) => format!("[{}]", v6.split(']').next()?),
        None => authority.split(':').next()?.to_owned(),
    };
    clean(&host.to_lowercase(), 255)
}

/// The URL's file name, decoded: what a person would call the file.
pub fn file_name(url: &str) -> Option<String> {
    let path = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = path.split(['?', '#']).next()?;
    let (_, last) = path.split_once('/')?;
    let last = last.rsplit('/').next()?;
    clean(&percent_decode(last), 160)
}

/// `%XX` escapes to bytes, then to text. A malformed escape is left as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Author-written text, made safe to show: no control characters, no
/// direction overrides, whitespace collapsed, at most `max` characters.
/// `None` when nothing is left.
pub fn clean(text: &str, max: usize) -> Option<String> {
    let kept: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !is_direction_control(*c))
        .collect();
    let collapsed = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(match collapsed.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", collapsed[..cut].trim_end()),
        None => collapsed,
    })
}

/// The characters that change the order text is displayed in — the trick
/// behind a file name that reads `…txt.exe` as `…exe.txt`.
fn is_direction_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The head of #29's example, as Greasy Fork serves it (abridged).
    const HIT_HIDER: &str = "// ==UserScript==
// @name           Google Hit Hider by Domain (Search Filter / Block Sites)
// @name:de        Google Treffer-Verstecker
// @author         Jefferson \"jscher2000\" Scher
// @namespace      JeffersonScher
// @version        2.4.1
// @description    Block unwanted sites from your Google, DuckDuckGo, Startpage.com, Bing and Yahoo search results.
// @include        http*://www.google.*/*
// @include        https://www.google.com/*
// @match          https://duckduckgo.com/*
// @match          https://*.startpage.com/*
// @match          https://www.bing.com/*
// @match          https://*.search.yahoo.com/*
// @exclude        https://www.google.com/maps*
// @grant          GM_getValue
// ==/UserScript==
(function () { 'use strict'; /* … */ })();
";

    #[test]
    fn reads_the_fields_the_dialog_shows() {
        let preview = parse(HIT_HIDER).expect("a userscript");
        assert_eq!(
            preview.name.as_deref(),
            Some("Google Hit Hider by Domain (Search Filter / Block Sites)")
        );
        assert_eq!(preview.version.as_deref(), Some("2.4.1"));
        assert!(preview.description.unwrap().starts_with("Block unwanted sites"));
        assert_eq!(
            preview.runs_on,
            RunsOn::Sites(
                ["google.*", "google.com", "duckduckgo.com", "startpage.com", "bing.com", "search.yahoo.com"]
                    .map(String::from)
                    .to_vec()
            )
        );
    }

    #[test]
    fn a_subdomain_of_a_listed_site_is_not_listed_again() {
        let source = "// ==UserScript==
// @include http*://www.google.*/*
// @include http*://news.google.*/*
// @include http*://encrypted.google.*/*
// @match https://maps.example.org/*
// @match https://*.search.yahoo.com/*
// ==/UserScript==";
        assert_eq!(
            parse(source).unwrap().runs_on,
            RunsOn::Sites(["google.*", "maps.example.org", "search.yahoo.com"].map(String::from).to_vec())
        );
    }

    #[test]
    fn any_pattern_for_every_site_makes_it_every_site() {
        for pattern in ["*://*/*", "<all_urls>", "*", "http*://*", "https://*/*", "*://*.*/*"] {
            let source = format!(
                "// ==UserScript==\n// @match https://example.org/*\n// @match {pattern}\n// ==/UserScript=="
            );
            assert_eq!(parse(&source).unwrap().runs_on, RunsOn::Everywhere, "{pattern}");
        }
    }

    #[test]
    fn a_block_naming_no_sites_is_not_read_as_nowhere() {
        let source = "// ==UserScript==\n// @name Bare\n// ==/UserScript==";
        assert_eq!(parse(source).unwrap().runs_on, RunsOn::Unstated);
    }

    #[test]
    fn anything_without_a_block_is_not_a_userscript() {
        assert_eq!(parse("<!doctype html><title>Not found</title>"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_localised_name_is_the_fallback_not_the_choice() {
        let source = "// ==UserScript==\n// @name:en English\n// @name Plain\n// ==/UserScript==";
        assert_eq!(parse(source).unwrap().name.as_deref(), Some("Plain"));
        let source = "// ==UserScript==\n// @name:en English\n// ==/UserScript==";
        assert_eq!(parse(source).unwrap().name.as_deref(), Some("English"));
    }

    /// A block cut short by the read limit still yields what arrived.
    #[test]
    fn a_truncated_block_keeps_what_arrived() {
        let source = "// ==UserScript==\n// @name Cut\n// @version 1";
        let preview = parse(source).unwrap();
        assert_eq!(preview.name.as_deref(), Some("Cut"));
        assert_eq!(preview.version.as_deref(), Some("1"));
    }

    /// The text a hostile script could use to make its name read as something
    /// else is gone, and so is anything that would break the dialog's layout.
    #[test]
    fn author_text_is_cleaned() {
        assert_eq!(
            clean("AdGuard\u{202E}txt.exe\u{202C} Extra", 100).as_deref(),
            Some("AdGuardtxt.exe Extra")
        );
        assert_eq!(clean("two\nlines\tand  gaps", 100).as_deref(), Some("two lines and gaps"));
        assert_eq!(clean(" \u{200F} ", 100), None);
        assert_eq!(clean("abcdef", 3).as_deref(), Some("abc…"));
    }

    #[test]
    fn the_file_name_is_decoded() {
        let url = "https://update.greasyfork.org/scripts/1682/Google%20Hit%20Hider%20by%20Domain\
                   %20%28Search%20Filter%20%20Block%20Sites%29.user.js";
        assert_eq!(
            file_name(url).as_deref(),
            Some("Google Hit Hider by Domain (Search Filter Block Sites).user.js")
        );
        assert_eq!(file_name("https://example.org/a.user.js?x=1#y").as_deref(), Some("a.user.js"));
        assert_eq!(file_name("https://example.org/100%zz.user.js").as_deref(), Some("100%zz.user.js"));
        assert_eq!(file_name("https://example.org/%E2%80%AEsj.resu").as_deref(), Some("sj.resu"));
    }

    #[test]
    fn the_host_is_the_one_connected_to() {
        assert_eq!(
            host("https://update.greasyfork.org/scripts/1/x.user.js").as_deref(),
            Some("update.greasyfork.org")
        );
        assert_eq!(
            host("https://greasyfork.org@example.net/x.user.js").as_deref(),
            Some("example.net")
        );
        assert_eq!(host("http://127.0.0.1:8765/x.user.js").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("http://[::1]:8765/x.user.js").as_deref(), Some("[::1]"));
        assert_eq!(host("HTTPS://GreasyFork.ORG/x").as_deref(), Some("greasyfork.org"));
        assert_eq!(host("not a url"), None);
    }

    /// Over loopback: the head is read, the rest of a long script is not
    /// needed, and a page that is not a userscript says so.
    #[test]
    fn fetches_and_reads_the_head_only() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let bodies = [
                // Longer than LIMIT, with the block at the top.
                format!("{HIT_HIDER}{}", "x".repeat(LIMIT as usize * 2)),
                "<html>404</html>".to_owned(),
            ];
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });

        let url = format!("http://127.0.0.1:{port}/a.user.js");
        let fetched = fetch(&url).expect("reads the head");
        assert_eq!(fetched.preview.version.as_deref(), Some("2.4.1"));
        assert_eq!(fetched.url, url, "nothing moved, so the address is the one given");
        assert_eq!(
            fetch(&format!("http://127.0.0.1:{port}/b.user.js")),
            Err(Error::NotAUserscript)
        );
        server.join().unwrap();
    }

    /// #29: an install counter that answers `302` with the script's real
    /// address. The head is read from there, and that is the address reported
    /// — the one AdGuard, which follows no redirects, can install from.
    #[test]
    fn a_redirect_reports_where_the_script_is() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let responses = [
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/raw/a.user.js\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HIT_HIDER}",
                    HIT_HIDER.len()
                ),
            ];
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let fetched = fetch(&format!("http://127.0.0.1:{port}/install/a.user.js")).expect("follows it");
        assert_eq!(fetched.url, format!("http://127.0.0.1:{port}/raw/a.user.js"));
        assert_eq!(fetched.preview.version.as_deref(), Some("2.4.1"));
        server.join().unwrap();
    }
}
