//! `adguard-ui://install-userscript?url=…` — what a browser hands this
//! application when someone clicks a userscript's *Install* button (#29).
//!
//! AdGuard for Windows and Android catch those clicks in their own proxy: the
//! response for a `.user.js` URL is swapped for an install prompt. AdGuard CLI
//! does not, and nothing this application can do reaches into its proxy.
//! Measured 3 October 2026 against 1.4.13: a Greasy Fork script fetched
//! through `127.0.0.1:3129` comes back byte-identical to a direct fetch,
//! `text/javascript`, with nothing injected — not an install page, and not
//! the userscripts AdGuard does inject into every HTML page. So the click has
//! to be caught on the page that *links* to the script, by a userscript of our
//! own (`data/userscripts/`), and passed here through a URL scheme, which is
//! the only way a web page can address a desktop application at all.
//!
//! **Anything on the web can open this link**, not only that userscript — a
//! page needs no permission to navigate to a custom scheme, only the browser's
//! "open with…" prompt, which most people answer once and forget. So a link is
//! never acted on: it fills in a confirmation that names the URL in full, and
//! defaults to *Cancel*.

use adguard_core::preview::{self, Preview, RunsOn};
use gtk4::glib;

/// The scheme registered in the `.desktop` file as `x-scheme-handler/…`.
pub const SCHEME: &str = "adguard-ui";

/// The one thing a link can ask for. A host rather than a path, so the link
/// reads `adguard-ui://install-userscript?…` the way people expect a URL to.
const INSTALL: &str = "install-userscript";

/// Why an argument was not taken as an install link.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not our scheme at all — some other argument.
    NotALink,
    /// Our scheme, asking for something this version does not do.
    UnknownAction(String),
    /// No `url=` parameter, or an empty one.
    NoUrl,
    /// A `url=` that `userscripts install` would refuse anyway: it fetches
    /// over the web and rejects paths and `file://` (contract §15). Refused
    /// here so the dialog never offers something that cannot work.
    NotWeb(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotALink => write!(f, "not an {SCHEME}:// link"),
            Self::UnknownAction(action) => write!(f, "unknown action \"{action}\""),
            Self::NoUrl => write!(f, "the link carries no url= to install from"),
            Self::NotWeb(url) => write!(f, "{url} is not an http or https address"),
        }
    }
}

/// The userscript URL an install link carries.
pub fn parse(link: &str) -> Result<String, Refused> {
    // Split encoded, then decode the parameters once: decoding the whole link
    // first would turn an `&` inside the carried URL into a separator.
    let Ok((scheme, _, host, _, _, query, _)) = glib::Uri::split(link, glib::UriFlags::ENCODED)
    else {
        return Err(Refused::NotALink);
    };
    if !scheme.is_some_and(|scheme| scheme.eq_ignore_ascii_case(SCHEME)) {
        return Err(Refused::NotALink);
    }

    let host = host.map(|host| host.to_string()).unwrap_or_default();
    if host != INSTALL {
        return Err(Refused::UnknownAction(host));
    }

    let url = query
        .as_deref()
        .and_then(|query| query.split('&').find_map(|param| param.strip_prefix("url=")))
        .and_then(|url| glib::Uri::unescape_string(url, None))
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
        .ok_or(Refused::NoUrl)?;

    match glib::Uri::peek_scheme(&url).as_deref() {
        Some("http" | "https") => Ok(url),
        _ => Err(Refused::NotWeb(url)),
    }
}

/// How much the confirmation knows about the script so far.
pub enum Details {
    /// The head of the script is being fetched.
    Reading,
    /// Its metadata block (`adguard_core::preview`).
    Read(Preview),
    /// It could not be read, and why — the dialog falls back to the file name.
    Unread(String),
}

/// The confirmation's body, as Pango markup.
///
/// **The name never appears without the host.** The name, version and
/// description are whatever the script's author wrote, and a hostile one can
/// call itself anything; the host is the one thing on the page that says whose
/// code this is, so it sits on the line directly under the name. Every
/// author-written string is escaped — `&` and `<` are ordinary in a
/// description, and unescaped they would fail the whole body.
pub fn body(url: &str, details: &Details) -> String {
    let esc = |text: &str| glib::markup_escape_text(text).to_string();
    let host = preview::host(url).unwrap_or_else(|| url.to_owned());
    let file = preview::file_name(url).unwrap_or_else(|| host.clone());

    let mut parts = Vec::new();
    match details {
        Details::Reading => {
            parts.push(format!("<b>{}</b>\nfrom {}", esc(&file), esc(&host)));
            parts.push("Reading what the script says about itself…".to_owned());
        }
        Details::Unread(why) => {
            parts.push(format!("<b>{}</b>\nfrom {}", esc(&file), esc(&host)));
            parts.push(format!(
                "Its details could not be read — {} — so this is only the name of the file.",
                esc(why)
            ));
        }
        Details::Read(preview) => {
            let name = preview.name.as_deref().unwrap_or(&file);
            let origin = match &preview.version {
                Some(version) => format!("version {} · from {}", esc(version), esc(&host)),
                None => format!("from {}", esc(&host)),
            };
            parts.push(format!("<b>{}</b>\n{origin}", esc(name)));
            if let Some(description) = &preview.description {
                parts.push(esc(description));
            }
            parts.push(match &preview.runs_on {
                RunsOn::Everywhere => "Runs on <b>every site</b>.".to_owned(),
                RunsOn::Unstated => {
                    "It names no sites, so it may run on <b>every site</b>.".to_owned()
                }
                sites => format!("Runs on {}.", esc(&sites.summary().unwrap_or_default())),
            });
        }
    }
    parts.push(
        "It will be able to read and change what you see on the sites it runs on. Add it \
         only if you just clicked an install link and trust where it came from."
            .to_owned(),
    );
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIT_HIDER: &str = "https://update.greasyfork.org/scripts/1682/Google%20Hit%20Hider\
                             %20by%20Domain%20%28Search%20Filter%20%20Block%20Sites%29.user.js";

    #[test]
    fn a_read_script_is_shown_by_its_own_name_and_its_host() {
        let details = Details::Read(Preview {
            name: Some("Google Hit Hider by Domain (Search Filter / Block Sites)".to_owned()),
            version: Some("2.4.1".to_owned()),
            description: Some("Block unwanted sites from your search results.".to_owned()),
            runs_on: RunsOn::Sites(vec!["google.com".to_owned(), "bing.com".to_owned()]),
        });
        let body = body(HIT_HIDER, &details);
        assert!(body.starts_with(
            "<b>Google Hit Hider by Domain (Search Filter / Block Sites)</b>\n\
             version 2.4.1 · from update.greasyfork.org\n\n\
             Block unwanted sites from your search results.\n\n\
             Runs on google.com, bing.com."
        ), "{body}");
    }

    /// Before the head arrives, and if it never does: the decoded file name,
    /// never the escaped URL.
    #[test]
    fn otherwise_the_decoded_file_name_and_its_host() {
        for details in [Details::Reading, Details::Unread("could not reach it".to_owned())] {
            let body = body(HIT_HIDER, &details);
            assert!(body.starts_with(
                "<b>Google Hit Hider by Domain (Search Filter Block Sites).user.js</b>\n\
                 from update.greasyfork.org"
            ), "{body}");
            assert!(!body.contains("%20"));
        }
    }

    #[test]
    fn author_text_cannot_inject_markup() {
        let details = Details::Read(Preview {
            name: Some("<span size='xx-large'>AdGuard Extra</span> & co".to_owned()),
            version: None,
            description: Some("<a href='x'>click</a>".to_owned()),
            runs_on: RunsOn::Everywhere,
        });
        let body = body("https://example.net/x.user.js", &details);
        assert!(body.starts_with(
            "<b>&lt;span size=&apos;xx-large&apos;&gt;AdGuard Extra&lt;/span&gt; &amp; co</b>\n\
             from example.net"
        ), "{body}");
        assert!(body.contains("&lt;a href=&apos;x&apos;&gt;click&lt;/a&gt;"));
        assert!(body.contains("Runs on <b>every site</b>."));
    }

    /// What the userscript in `data/userscripts/` builds: the script URL
    /// through `encodeURIComponent`.
    #[test]
    fn carries_an_encoded_url_through_intact() {
        let link = "adguard-ui://install-userscript?url=https%3A%2F%2Fupdate.greasyfork.org\
                    %2Fscripts%2F1682%2FGoogle%2520Hit%2520Hider.user.js%3Fa%3D1%26b%3D2";
        assert_eq!(
            parse(link),
            Ok("https://update.greasyfork.org/scripts/1682/Google%20Hit%20Hider.user.js?a=1&b=2"
                .to_owned())
        );
    }

    #[test]
    fn other_arguments_are_not_links() {
        assert_eq!(parse("--background"), Err(Refused::NotALink));
        assert_eq!(parse("https://example.org/x.user.js"), Err(Refused::NotALink));
        assert_eq!(parse(""), Err(Refused::NotALink));
    }

    #[test]
    fn only_install_is_understood() {
        assert_eq!(
            parse("adguard-ui://remove-userscript?url=https%3A%2F%2Fexample.org%2Fx.user.js"),
            Err(Refused::UnknownAction("remove-userscript".to_owned()))
        );
    }

    #[test]
    fn a_link_without_a_url_is_refused() {
        assert_eq!(parse("adguard-ui://install-userscript"), Err(Refused::NoUrl));
        assert_eq!(parse("adguard-ui://install-userscript?url="), Err(Refused::NoUrl));
        assert_eq!(parse("adguard-ui://install-userscript?other=1"), Err(Refused::NoUrl));
    }

    /// The launcher entry is what registers the scheme, and `%u` is what makes
    /// the link an argument at all. Without either, a click does nothing and
    /// nothing says why.
    #[test]
    fn the_desktop_entry_registers_the_scheme() {
        let entry = include_str!("../../../data/io.github.dominik-najberg.AdGuardUI.desktop");
        let key = |name: &str| {
            entry
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name}=")))
                .unwrap_or_else(|| panic!("no {name}= line"))
        };
        assert!(key("MimeType").split(';').any(|t| t == format!("x-scheme-handler/{SCHEME}")));
        assert_eq!(key("Exec"), "adguard-ui %u");
    }

    /// The browser half builds exactly the link this half reads.
    #[test]
    fn the_userscript_builds_a_link_this_parses() {
        let script =
            include_str!("../../../data/userscripts/adguard-ui-install-links.user.js");
        let prefix = format!("'{SCHEME}://{INSTALL}?url=' + encodeURIComponent(");
        assert!(script.contains(&prefix), "the userscript no longer builds {prefix}");
    }

    /// The CLI refuses these too, but with a sentence that explains nothing,
    /// and only after the user has said yes to a dialog naming them.
    #[test]
    fn only_web_addresses_are_offered() {
        for url in ["file%3A%2F%2F%2Ftmp%2Fx.user.js", "%2Ftmp%2Fx.user.js", "javascript%3Aalert(1)"]
        {
            let link = format!("adguard-ui://install-userscript?url={url}");
            assert!(matches!(parse(&link), Err(Refused::NotWeb(_))), "{link}");
        }
    }
}
