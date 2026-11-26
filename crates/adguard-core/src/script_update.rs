//! Whether one installed userscript has a newer version, asked of its author's
//! server rather than of AdGuard.
//!
//! ## Why not `check-update`
//!
//! `check-update` is the only thing in AdGuard CLI that updates a userscript,
//! and it is the wrong tool for a badge on one row (contract §15, *What
//! `check-update` does to a userscript*):
//!
//! - it does not check, it **installs** — and refreshes filters, DNS filters,
//!   Safe Browsing and CRLite on the way, so a badge would cost the user's
//!   whole content update;
//! - it says `1 userscript(s) updated` and never which, and it says it **on
//!   every run** while any enabled script has an empty `updateURL`, because
//!   that script is downloaded again whether it changed or not;
//! - it **skips disabled scripts entirely**, so a script the user switched off
//!   never learns it is out of date;
//! - and it **crashes** — a segfault, exit 139 — when an enabled script carries
//!   neither `@updateURL` nor `@downloadURL`, taking the rest of the run down
//!   with it.
//!
//! So this module asks the one question itself, read-only: fetch the address a
//! userscript manager would poll, read the `@version` in its metadata block,
//! compare. Nothing is written and nothing is installed; *Update* is the
//! Extensions page's reinstall, which is where the user's edits are merged.
//!
//! ## Which address
//!
//! `updateURL` first: by convention it serves the metadata block alone
//! (`.meta.js`), which is what a version check wants, and it is what AdGuard
//! itself polls — measured, a script with one is fetched at its `updateURL`
//! and nowhere else until that says something newer. `downloadURL` when it is
//! empty, which is how AdGuard records a script whose source declared none:
//! the whole script is served there, and [`preview::fetch`] reads only its head
//! ([`preview::LIMIT`]), where the block is.
//!
//! The fetch is [`preview::fetch`]'s, unchanged, so it follows redirects the
//! way the install dialog does and sends the same user agent and nothing else.

use std::cmp::Ordering;

use crate::preview;

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The server's version is not newer than the installed one. An *older*
    /// one counts too: offering a downgrade as an update would be wrong, and
    /// a server that went backwards is not the user's problem to solve here.
    UpToDate,
    /// The server has this version, newer than the installed one.
    Available(String),
    /// Nothing could be concluded, and why — in words for a tooltip. A failed
    /// check is ordinary (a server down, a script that states no version) and
    /// is never shown as an error on the row.
    Unknown(String),
}

/// Check one script.
///
/// `installed` is the version in its metadata file — `None` when the script
/// states none — and the two addresses are its `updateURL` and `downloadURL`,
/// empty strings counting as absent, which is how AdGuard writes a missing one.
///
/// Blocking, for a worker thread: one request, [`preview`]'s timeout.
pub fn check(installed: Option<&str>, update_url: Option<&str>, download_url: Option<&str>) -> Check {
    let Some(url) = source(update_url, download_url) else {
        return Check::Unknown("this userscript does not say where it is updated from".to_owned());
    };
    let fetched = match preview::fetch(url) {
        Ok(fetched) => fetched,
        Err(err) => return Check::Unknown(err.to_string()),
    };
    verdict(installed, fetched.preview.version.as_deref())
}

/// The address to poll: `updateURL`, else `downloadURL`, else nothing.
fn source<'a>(update_url: Option<&'a str>, download_url: Option<&'a str>) -> Option<&'a str> {
    let present = |url: Option<&'a str>| url.map(str::trim).filter(|url| !url.is_empty());
    present(update_url).or_else(|| present(download_url))
}

/// Compare what is installed with what the server said.
fn verdict(installed: Option<&str>, remote: Option<&str>) -> Check {
    let Some(remote) = remote.map(str::trim).filter(|v| !v.is_empty()) else {
        return Check::Unknown("the server's copy states no version".to_owned());
    };
    // With nothing installed to compare against, *newer* means nothing. The
    // reinstall is still on the menu; it just is not offered as an update.
    let Some(installed) = installed.map(str::trim).filter(|v| !v.is_empty()) else {
        return Check::Unknown("the installed copy states no version".to_owned());
    };
    match compare(remote, installed) {
        Ordering::Greater => Check::Available(remote.to_owned()),
        Ordering::Equal | Ordering::Less => Check::UpToDate,
    }
}

/// Order two userscript versions the way the managers do.
///
/// Dot-separated parts compared left to right; a missing part is `0`, so
/// `1.0` and `1.0.0` are the same version. Each part is a number and whatever
/// follows it: the numbers decide first, and between equal numbers a bare one
/// is **newer** than one with a suffix, because the suffix is almost always a
/// pre-release — `2.0` is after `2.0-beta` and `2.0b1`. Two suffixes compare as
/// text, which orders `alpha` < `beta` < `rc` without knowing the words.
///
/// A leading `v` is ignored (`v1.2` is `1.2`); everything else is taken as
/// written, because a version is author text and nothing validates it.
///
/// `-` is also a part separator, but a *weak* one: `1.2.3-4` would otherwise
/// read as `1.2.3` with the suffix `-4`, a pre-release of itself, where every
/// script that writes it means a build number after `1.2.3`. So it splits a
/// part only when what follows starts with a digit.
pub fn compare(a: &str, b: &str) -> Ordering {
    let a = parts(a);
    let b = parts(b);
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).cloned().unwrap_or_default();
        let right = b.get(index).cloned().unwrap_or_default();
        let order = left.cmp_part(&right);
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

/// One part of a version: its leading number, and any text after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Part {
    number: u64,
    suffix: String,
}

impl Part {
    fn parse(text: &str) -> Self {
        let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
        Self {
            // Saturating: a part longer than u64 is a garbage version, and
            // garbage compared as huge is no worse than garbage compared as 0.
            number: text[..digits].parse().unwrap_or(if digits == 0 { 0 } else { u64::MAX }),
            suffix: text[digits..].trim_start_matches(['-', '_', '+']).to_lowercase(),
        }
    }

    fn cmp_part(&self, other: &Self) -> Ordering {
        self.number.cmp(&other.number).then_with(|| match (self.suffix.is_empty(), other.suffix.is_empty()) {
            (true, true) => Ordering::Equal,
            // A bare number is the release its suffixed sibling leads up to.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.suffix.cmp(&other.suffix),
        })
    }
}

fn parts(version: &str) -> Vec<Part> {
    let version = version.trim();
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    let mut out = Vec::new();
    for dotted in version.split('.') {
        // `-` followed by a digit is a separator; any other `-` is part of
        // the suffix it introduces.
        let mut rest = dotted;
        loop {
            let split = rest
                .char_indices()
                .find(|&(i, c)| c == '-' && rest[i + 1..].starts_with(|n: char| n.is_ascii_digit()) && i > 0);
            match split {
                Some((i, _)) if rest[..i].chars().all(|c| c.is_ascii_digit()) => {
                    out.push(Part::parse(&rest[..i]));
                    rest = &rest[i + 1..];
                }
                _ => {
                    out.push(Part::parse(rest));
                    break;
                }
            }
        }
    }
    // Trailing zero parts carry nothing: `1.0.0` is `1`. Dropped so that a
    // longer list of zeroes cannot make a version look newer by length.
    while out.last().is_some_and(|part| *part == Part::default()) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    #[test]
    fn numbers_compare_as_numbers_not_text() {
        assert_eq!(compare("1.10", "1.9"), Greater);
        assert_eq!(compare("2.5.117", "2.5.20"), Greater);
        assert_eq!(compare("1.1.38", "1.1.41"), Less);
    }

    #[test]
    fn missing_parts_are_zero() {
        assert_eq!(compare("1.0", "1.0.0"), Equal);
        assert_eq!(compare("1", "1.0.0.0"), Equal);
        assert_eq!(compare("1.0.1", "1.0"), Greater);
    }

    #[test]
    fn a_pre_release_comes_before_its_release() {
        assert_eq!(compare("2.0-beta", "2.0"), Less);
        assert_eq!(compare("2.0b1", "2.0"), Less);
        assert_eq!(compare("2.0.0-rc.1", "2.0.0"), Less);
        assert_eq!(compare("2.0-alpha", "2.0-beta"), Less);
        assert_eq!(compare("2.0-rc", "2.0-beta"), Greater);
    }

    /// A build number after a dash is after the release, not before it.
    #[test]
    fn a_dash_then_a_digit_is_a_further_part() {
        assert_eq!(compare("1.2.3-4", "1.2.3"), Greater);
        assert_eq!(compare("1.2.3-4", "1.2.4"), Less);
    }

    #[test]
    fn a_leading_v_and_whitespace_are_ignored() {
        assert_eq!(compare("v1.2", "1.2"), Equal);
        assert_eq!(compare(" 1.2 ", "V1.2"), Equal);
    }

    /// Author text is not validated, so nonsense must order somehow and never
    /// panic.
    #[test]
    fn nonsense_does_not_panic() {
        for (a, b) in [("", ""), ("abc", "1"), ("...", "1.."), ("99999999999999999999999", "1"), ("1.-", "1")] {
            let _ = compare(a, b);
        }
        assert_eq!(compare("abc", "1"), Less, "a version with no number is below any release");
    }

    #[test]
    fn update_url_is_preferred_and_empty_counts_as_absent() {
        assert_eq!(source(Some("https://u/a.meta.js"), Some("https://d/a.user.js")), Some("https://u/a.meta.js"));
        assert_eq!(source(Some(""), Some("https://d/a.user.js")), Some("https://d/a.user.js"));
        assert_eq!(source(Some("  "), None), None);
        assert_eq!(source(None, Some("")), None);
    }

    #[test]
    fn only_a_newer_version_is_offered() {
        assert_eq!(verdict(Some("2.7.1"), Some("2.7.2")), Check::Available("2.7.2".to_owned()));
        assert_eq!(verdict(Some("2.7.1"), Some("2.7.1")), Check::UpToDate);
        assert_eq!(verdict(Some("2.7.1"), Some("2.7.0")), Check::UpToDate, "never a downgrade");
        assert!(matches!(verdict(None, Some("1.0")), Check::Unknown(_)));
        assert!(matches!(verdict(Some("1.0"), Some("")), Check::Unknown(_)));
    }

    /// Over loopback, the way the page will call it: the `.meta.js` at the
    /// `updateURL` answers, and its version is compared with the installed one.
    #[test]
    fn checks_against_the_served_metadata() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let body = "// ==UserScript==\n// @name Probe\n// @version 1.1\n// ==/UserScript==\n";
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });

        let url = format!("http://127.0.0.1:{port}/a.meta.js");
        assert_eq!(check(Some("1.0"), Some(&url), None), Check::Available("1.1".to_owned()));
        server.join().unwrap();

        assert!(matches!(check(Some("1.0"), Some(""), Some("")), Check::Unknown(_)));
    }
}
