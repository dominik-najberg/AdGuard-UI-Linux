//! Reading the installed userscripts.
//!
//! Two sources, joined, because neither answers on its own (contract §15):
//!
//! - the `userscripts/` directory holds a `<id>.meta.json` + `<id>.user.js`
//!   pair per script and says what is **installed**;
//! - `proxy.yaml`'s `userscripts:` list says what is **enabled**, and that is
//!   the whole state model — there is no `enabled` flag in either file, and
//!   `userscripts disable` deletes the config entry while leaving the pair on
//!   disk.
//!
//! A reader consulting only the directory reports every disabled script as
//! running; one consulting only the config reports every disabled script as
//! uninstalled. Both are wrong in a way the user would notice immediately.
//!
//! ## Why the metadata file rather than `userscripts list`
//!
//! `docs/cli-contract.md` §6's argument, in a second place. The CLI's table
//! carries a title, an id and a timestamp — and **no version and no
//! description**, the first of which is exactly what [issue #9] asks to
//! display. Both are in the metadata file, along with the homepage and the
//! download URL a reinstall needs, and localised into ~40 languages besides.
//! Parsing the human table would be reading less data, less reliably.
//!
//! ## Why there is no JSON dependency
//!
//! The metadata files are JSON and this workspace has no JSON parser. It does
//! not need one: **JSON is a subset of YAML 1.2**, and `yaml-rust2` — already
//! here for `proxy.yaml` — reads these files exactly. Measured 15 August 2026
//! against the real `adguard-extra.meta.json`: the 16 KB base64 `icon` string,
//! the non-ASCII localised descriptions, the booleans, the arrays and the
//! absent keys all came back correctly. Adding `serde_json` to read a file an
//! existing dependency already reads would be a build-time cost for nothing.
//!
//! [issue #9]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/9

use std::path::Path;

use yaml_rust2::{Yaml, YamlLoader};

use crate::locale::Locale;
use crate::model::{Recommended, Userscript, INSTALL_LINKS, RECOMMENDED};

/// The suffix a metadata file carries, after the id.
const META_SUFFIX: &str = ".meta.json";

/// The suffix the script's source carries, after the id.
const SOURCE_SUFFIX: &str = ".user.js";

/// Everything the Extensions page renders, from one point in time.
///
/// `enabled` is the `meta:` paths out of `proxy.yaml`'s `userscripts:` list —
/// [`crate::Config::enabled_userscripts`] — and `dir` is where the pairs live.
/// Both are passed in rather than read here so that the caller decides which
/// install is being looked at, which is what lets a sandbox be looked at at all.
///
/// Scripts come back sorted by display name, case-insensitively. The directory
/// yields whatever order the filesystem feels like, and a list that reshuffles
/// itself between two reads of an unchanged install would be unusable.
pub fn read(dir: &Path, enabled: &[&str], locale: &Locale) -> Vec<Userscript> {
    let mut scripts: Vec<Userscript> = ids(dir)
        .into_iter()
        .map(|id| {
            let meta = Meta::read(&dir.join(format!("{id}{META_SUFFIX}")));
            let enabled = is_enabled(&id, enabled);
            meta.to_userscript(id, enabled, locale)
        })
        .collect();

    mark_ambiguous(&mut scripts);
    sort(&mut scripts);
    scripts
}

/// Order rows by what they are called on screen, case-insensitively.
///
/// Public because the caller lays the user's own names over the rows
/// ([`crate::userscript_state::Aliases::apply`]) after [`read`] has sorted
/// them, and a renamed script should sit where its new name puts it.
pub fn sort(scripts: &mut [Userscript]) {
    scripts.sort_by_key(|script| script.display_name().to_lowercase());
}

/// The installed source of a script: `<id>.user.js` beside its metadata.
pub fn source_path(dir: &Path, id: &str) -> std::path::PathBuf {
    dir.join(format!("{id}{SOURCE_SUFFIX}"))
}

/// Where a script's metadata file is, beside its source.
pub fn meta_path(dir: &Path, id: &str) -> std::path::PathBuf {
    dir.join(format!("{id}{META_SUFFIX}"))
}

/// The pages a script runs on: its metadata's `match` and `include` lists,
/// in that order, as AdGuard recorded them at install.
///
/// Read on demand rather than kept on [`Userscript`]: only *Edit with AI*
/// wants it, to tell the agent where the script runs, and every row carrying
/// the lists would be a read of every list on every reload for one dialog.
pub fn run_patterns(dir: &Path, id: &str) -> Vec<String> {
    let meta = Meta::read(&meta_path(dir, id));
    ["match", "include"]
        .iter()
        .flat_map(|key| meta.0[*key].as_vec().cloned().unwrap_or_default())
        .filter_map(|value| value.as_str().map(str::trim).map(str::to_owned))
        .filter(|value| !value.is_empty())
        .collect()
}

/// The page a script was installed from, worked out from its `downloadURL`.
///
/// **The homepage is not this, and #9's screenshot is why the cog needs
/// both.** A script's `@homepage` is whatever its author says it is — for a
/// Greasy Fork downloader it was the author's own product site — while the
/// person who installed it remembers the listing they clicked *Install* on.
/// AdGuard records only the address it downloaded from, which for every host
/// worth handling here is a fixed transformation of that listing's address:
///
/// | `downloadURL` | page |
/// | --- | --- |
/// | `update.greasyfork.org/scripts/<n>/<file>` | `greasyfork.org/scripts/<n>` |
/// | `greasyfork.org/scripts/<n>/code/<file>` | `greasyfork.org/scripts/<n>` |
/// | the same two on `sleazyfork.org` | `sleazyfork.org/scripts/<n>` |
/// | `openuserjs.org/install/<user>/<file>.user.js` | `openuserjs.org/scripts/<user>/<file>` |
/// | `raw.githubusercontent.com/<o>/<r>/<ref>/<path>` | `github.com/<o>/<r>/blob/<ref>/<path>` |
/// | `github.com/<o>/<r>/raw/<ref>/<path>` | `github.com/<o>/<r>/blob/<ref>/<path>` |
/// | `github.com/<o>/<r>/releases/download/<tag>/<file>` | `github.com/<o>/<r>/releases/tag/<tag>` |
/// | `gist.githubusercontent.com/<user>/<id>/raw/…` | `gist.github.com/<user>/<id>` |
///
/// `None` for any other host, and the caller offers the download address
/// itself instead — which is still where the script came from, just not a
/// page about it. Path segments are carried over still escaped: they were
/// valid in the address AdGuard fetched, and decoding them could only make
/// them less so.
pub fn script_page(download_url: &str) -> Option<String> {
    let (host, path) = split_url(download_url)?;
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    let host = host.to_ascii_lowercase();
    let at = |index: usize| segments.get(index).copied();

    match host.as_str() {
        "greasyfork.org" | "update.greasyfork.org" | "sleazyfork.org" | "update.sleazyfork.org" => {
            let site = host.trim_start_matches("update.");
            // A language prefix (`/en/scripts/…`) is legal on the listing
            // host, so the number is found after `scripts` wherever it is.
            let scripts = segments.iter().position(|segment| *segment == "scripts")?;
            let number = at(scripts + 1)?;
            Some(format!("https://{site}/scripts/{number}"))
        }
        "openuserjs.org" => {
            // `/install/<user>/<file>.user.js`, and the older
            // `/src/scripts/<user>/<file>.user.js` that serves the same file.
            let (user, file) = match (at(0)?, at(1)?) {
                ("install", user) => (user, at(2)?),
                ("src", "scripts") => (at(2)?, at(3)?),
                _ => return None,
            };
            let name = file.strip_suffix(".user.js").unwrap_or(file);
            Some(format!("https://openuserjs.org/scripts/{user}/{name}"))
        }
        "raw.githubusercontent.com" => {
            let (owner, repo) = (at(0)?, at(1)?);
            // `refs/heads/<branch>` and `refs/tags/<tag>` are the long form of
            // the ref segment GitHub's own *Raw* button now links to.
            let (reference, rest) = match (at(2)?, at(3)) {
                ("refs", Some("heads" | "tags")) => (at(4)?, 5),
                (reference, _) => (reference, 3),
            };
            let file = segments.get(rest..).filter(|file| !file.is_empty())?.join("/");
            Some(format!("https://github.com/{owner}/{repo}/blob/{reference}/{file}"))
        }
        "github.com" => {
            let (owner, repo) = (at(0)?, at(1)?);
            match (at(2)?, at(3)?) {
                ("raw", reference) => {
                    let file = segments.get(4..).filter(|file| !file.is_empty())?.join("/");
                    Some(format!("https://github.com/{owner}/{repo}/blob/{reference}/{file}"))
                }
                ("releases", "download") => {
                    let tag = at(4)?;
                    Some(format!("https://github.com/{owner}/{repo}/releases/tag/{tag}"))
                }
                _ => None,
            }
        }
        "gist.githubusercontent.com" | "gist.github.com" => {
            let (user, gist) = (at(0)?, at(1)?);
            // Only a raw address: anything else on `gist.github.com` is
            // already a page, and is not what AdGuard downloads from.
            (at(2)? == "raw").then(|| format!("https://gist.github.com/{user}/{gist}"))
        }
        _ => None,
    }
}

/// The host of an http(s) address, for the dim note beside a menu item —
/// `greasyfork.org`, not the whole escaped address.
///
/// Lowercased, without a port, a login or a leading `www.`: it is there to be
/// recognised at a glance. `None` for anything that is not http or https.
pub fn host(url: &str) -> Option<String> {
    let (host, _) = split_url(url)?;
    let host = host.to_ascii_lowercase();
    Some(host.strip_prefix("www.").map_or(host.clone(), str::to_owned))
}

/// An http(s) address cut into its host and its path, the query and fragment
/// dropped.
///
/// Hand-rolled rather than a URL crate's, for the reason the module header
/// gives about JSON: this workspace has no URL parser and these addresses are
/// ones AdGuard has already fetched, so the only question is where the host
/// ends. A login (`user@`) and a port are stripped from the host.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let url = url.trim();
    let rest = ["https://", "http://"]
        .iter()
        .find_map(|scheme| {
            url.get(..scheme.len())
                .filter(|head| head.eq_ignore_ascii_case(scheme))
                .map(|_| &url[scheme.len()..])
        })?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let path = tail.split(['?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // A port, but not the colons of a bracketed IPv6 address.
    let host = match host.rfind(':') {
        Some(colon) if !host[colon..].contains(']') => &host[..colon],
        _ => host,
    };
    (!host.is_empty()).then_some((host, path))
}

/// The recommended scripts the user does not have.
///
/// The Extensions page offers these and nothing else — a script already
/// installed has a real row, with its true version and its own switch, so
/// leaving it in the catalogue would list it twice and offer to fetch something
/// already on disk. An empty answer therefore means *you have all four*, which
/// is a reading worth having.
///
/// Matched on the id rather than the URL: a script the user installed by hand
/// from AdGuard's URL is the same script, and one whose recorded `downloadURL`
/// has since moved is still not something to offer again.
pub fn recommended(installed: &[Userscript]) -> Vec<&'static Recommended> {
    RECOMMENDED
        .iter()
        .filter(|entry| !installed.iter().any(|script| script.id == entry.id))
        .collect()
}

/// Where this application's own script for browser install links stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    /// Not installed: [`INSTALL_LINKS`] is offered.
    Missing(&'static Recommended),
    /// Installed and switched off, which is as good as missing — AdGuard does
    /// not inject it, so *Install* on Greasy Fork opens the script as text —
    /// and until 1.10 nothing said so. `ambiguous` is
    /// [`Userscript::ambiguous`]: the CLI cannot be told to switch it on.
    Off { ambiguous: bool },
    On,
}

impl Helper {
    /// Whether clicking *Install* in a browser reaches this application.
    pub fn works(self) -> bool {
        self == Self::On
    }
}

/// [`Helper`] for these scripts.
///
/// Matched on the id, as [`recommended`] is and for its reason.
pub fn install_links(installed: &[Userscript]) -> Helper {
    match installed.iter().find(|script| script.id == INSTALL_LINKS.id) {
        None => Helper::Missing(&INSTALL_LINKS),
        Some(script) if script.enabled => Helper::On,
        Some(script) => Helper::Off {
            ambiguous: script.ambiguous,
        },
    }
}

/// The ids installed in `dir`, from the metadata files present.
///
/// Keyed on `<id>.meta.json` and **not** on `<id>.user.js`, though a healthy
/// install has both: the metadata is the file every field is read from, so a
/// pair missing it would render as a script with no name, no version and no
/// description — a row that says nothing. A pair missing the JavaScript
/// instead is AdGuard's problem to report, and the row still describes what
/// the user installed.
///
/// An unreadable directory answers with nothing, which is also what an install
/// that has never had a script installed looks like. The two are not
/// distinguished because no caller could act differently on them.
fn ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(META_SUFFIX)?;
            (!id.is_empty()).then(|| id.to_owned())
        })
        .collect()
}

/// Is `id` among the enabled `meta:` paths?
///
/// The config holds a path (`userscripts/adguard-extra.meta.json`) where this
/// module holds an id, so the comparison is on the filename rather than on the
/// whole string. The directory component is deliberately not checked: it is
/// AdGuard's to choose, `config show` has been seen to write it both quoted and
/// unquoted, and a script is not less enabled for having been written with a
/// different prefix.
fn is_enabled(id: &str, enabled: &[&str]) -> bool {
    let wanted = format!("{id}{META_SUFFIX}");
    enabled.iter().any(|meta| {
        Path::new(meta.trim())
            .file_name()
            .is_some_and(|name| name == wanted.as_str())
    })
}

/// Flag every script the CLI cannot be made to name.
///
/// `enable`, `disable` and `remove` match a **case-insensitive substring**
/// against both the id and the title, and offer no exact-match flag. So a
/// script is unreachable when its id appears inside *another* script's id or
/// title — measured with `hello` and `hello-world` installed, where passing the
/// exact id `hello` is refused with `Multiple userscripts match 'hello'`
/// (contract §15).
///
/// Both fields of the other script are checked, not just its id. A narrower
/// rule looking only at ids would miss the case where one script's *title*
/// contains another's id, which the CLI matches just as readily — and the whole
/// value of computing this is that it agrees with what the CLI will do.
///
/// Compared against the other script's id and title but never against its own:
/// every id contains itself, so a self-comparison would mark every script on
/// the page unreachable.
///
/// **Against [`Userscript::name`], never a user's alias.** A rename lives in
/// this application alone ([`crate::userscript_state::Aliases`]); the CLI
/// has never heard of it and goes on matching the title the metadata carries,
/// so that is the only title that can collide.
fn mark_ambiguous(scripts: &mut [Userscript]) {
    let others: Vec<(String, String)> = scripts
        .iter()
        .map(|script| (script.id.to_lowercase(), script.name.to_lowercase()))
        .collect();

    for (index, script) in scripts.iter_mut().enumerate() {
        let needle = script.id.to_lowercase();
        script.ambiguous = others.iter().enumerate().any(|(other, (id, name))| {
            other != index && (id.contains(&needle) || name.contains(&needle))
        });
    }
}

/// One parsed metadata file.
///
/// A newtype over the document rather than a struct of fields, for the reason
/// [`crate::config`] gives at length: these files are written by whoever wrote
/// the userscript, nothing validates them, and a strict deserialise would fail
/// a whole script on one mistyped key. Every field is read tolerantly and an
/// unreadable one costs itself alone.
struct Meta(Yaml);

impl Meta {
    /// Read and parse, or hold nothing.
    ///
    /// A missing, unreadable or malformed file is [`Yaml::BadValue`], from
    /// which every accessor below answers `None`. That yields a row carrying
    /// its id and nothing else — which is honest, and better than dropping a
    /// script the user can see in their own directory.
    fn read(path: &Path) -> Self {
        let document = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| YamlLoader::load_from_str(&text).ok())
            .and_then(|documents| documents.into_iter().next())
            .unwrap_or(Yaml::BadValue);
        Self(document)
    }

    /// A non-empty string at `key`, trimmed.
    fn string(&self, key: &str) -> Option<String> {
        let value = self.0[key].as_str()?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// The first non-empty string among `keys`.
    fn first(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|key| self.string(key))
    }

    /// A localised field, resolved the way the filter catalogue resolves its
    /// own — full tag, then bare language, then the unsuffixed key.
    ///
    /// **The tag form differs from the databases', and that is the whole reason
    /// this is not a one-liner.** `filter_localisation.lang` is POSIX with an
    /// underscore (`pt_BR`), which is what [`Locale`] answers in; these files
    /// use BCP-47 with a hyphen (`pt-PT`, `zh-HK`). Looking up `Locale`'s
    /// answer verbatim would silently match nothing and quietly fall back to
    /// English on every non-English machine — the same trap `locale.rs`
    /// documents in the other direction.
    fn localised(&self, key: &str, locale: &Locale) -> String {
        let candidates = [
            format!("{key}:{}", hyphenated(locale.primary())),
            format!("{key}:{}", hyphenated(locale.fallback())),
        ];
        candidates
            .iter()
            .find_map(|candidate| self.string(candidate))
            .or_else(|| self.string(key))
            .unwrap_or_default()
    }

    /// Everything one file contributes to a row.
    ///
    /// The locale is the caller's rather than this process's — `Locale::from_env`
    /// is not consulted here. The Extensions page resolves it once and passes it
    /// down, exactly as the Filters page does, which is also what lets a test
    /// name a language without setting an environment variable.
    fn to_userscript(&self, id: String, enabled: bool, locale: &Locale) -> Userscript {
        Userscript {
            name: self.localised("name", locale),
            description: self.localised("description", locale),
            version: self.string("version"),
            // `supportURL` is the fallback rather than an equal: a homepage
            // describes the script, an issue tracker is where to complain about
            // it, and the row offers one link.
            homepage: self.first(&["homepageURL", "supportURL"]),
            download_url: self.string("downloadURL"),
            update_url: self.string("updateURL"),
            id,
            enabled,
            // Set by `mark_ambiguous` once every sibling is known; it cannot be
            // decided from one file.
            ambiguous: false,
            // Both this application's, not AdGuard's: the caller lays them
            // over the row once it has read its own files.
            alias: None,
            edited: false,
        }
    }
}

/// `pt_BR` -> `pt-BR`, the form the metadata files' localised keys take.
fn hyphenated(tag: &str) -> String {
    tag.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `match` first, then `include`, blanks dropped: what *Edit with AI*
    /// tells an agent about where a script runs.
    #[test]
    fn run_patterns_read_match_then_include() {
        let dir = std::env::temp_dir().join(format!("run-patterns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            meta_path(&dir, "s"),
            r#"{"match": ["https://a/*", " "], "include": ["*://b/*"], "exclude": ["x"]}"#,
        )
        .unwrap();
        assert_eq!(run_patterns(&dir, "s"), vec!["https://a/*", "*://b/*"]);
        assert!(run_patterns(&dir, "missing").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use crate::model::Userscript;

    fn meta(json: &str) -> Meta {
        Meta(
            YamlLoader::load_from_str(json)
                .expect("the fixture is valid JSON")
                .into_iter()
                .next()
                .expect("one document"),
        )
    }

    fn script(id: &str, name: &str) -> Userscript {
        Userscript {
            id: id.to_owned(),
            name: name.to_owned(),
            description: String::new(),
            version: None,
            homepage: None,
            download_url: None,
            update_url: None,
            enabled: false,
            ambiguous: false,
            alias: None,
            edited: false,
        }
    }

    /// The shape the CLI writes for a script installed from a plain URL.
    const MINIMAL: &str = r#"{
        "name": "Hello Sandbox",
        "description": "A do-nothing script.",
        "version": "0.2.1",
        "updateURL": "",
        "downloadURL": "http://127.0.0.1:8731/hello.user.js",
        "homepageURL": "https://example.org/hello",
        "supportURL": "",
        "icon": ""
    }"#;

    #[test]
    fn reads_the_fields_the_page_renders() {
        let script = meta(MINIMAL).to_userscript("hello".to_owned(), true, &Locale::english());
        assert_eq!(script.name, "Hello Sandbox");
        assert_eq!(script.version.as_deref(), Some("0.2.1"));
        assert_eq!(script.homepage.as_deref(), Some("https://example.org/hello"));
        assert_eq!(
            script.download_url.as_deref(),
            Some("http://127.0.0.1:8731/hello.user.js")
        );
        assert!(script.enabled);
    }

    /// The CLI stores `""` for a script whose source carried no `@version`, and
    /// #9 asks for the version *when it is available* — so an empty string has
    /// to arrive as `None` rather than as a version that renders blank.
    #[test]
    fn an_empty_version_is_absent_rather_than_empty() {
        let script = meta(r#"{"name": "X", "version": ""}"#)
            .to_userscript("x".to_owned(), false, &Locale::english());
        assert_eq!(script.version, None);
    }

    /// `supportURL` stands in only when there is no homepage — and an empty
    /// `homepageURL` counts as none, which is how the CLI writes an absent one.
    #[test]
    fn support_url_is_the_homepage_fallback() {
        let both = meta(r#"{"homepageURL": "https://home", "supportURL": "https://support"}"#)
            .to_userscript("x".to_owned(), false, &Locale::english());
        assert_eq!(both.homepage.as_deref(), Some("https://home"));

        let support = meta(r#"{"homepageURL": "", "supportURL": "https://support"}"#)
            .to_userscript("x".to_owned(), false, &Locale::english());
        assert_eq!(support.homepage.as_deref(), Some("https://support"));

        let neither = meta(r#"{"homepageURL": "", "supportURL": ""}"#)
            .to_userscript("x".to_owned(), false, &Locale::english());
        assert_eq!(neither.homepage, None);
    }

    /// A file that is missing, empty or not JSON at all must still yield a row
    /// — the user can see the script in their own directory, and a page that
    /// silently dropped it would be lying about what is installed.
    #[test]
    fn an_unreadable_file_still_yields_a_named_row() {
        let script = Meta(Yaml::BadValue).to_userscript("orphan".to_owned(), true, &Locale::english());
        assert_eq!(script.display_name(), "orphan");
        assert_eq!(script.version, None);
        assert!(script.enabled, "the config still says it is switched on");
    }

    /// The localised keys are hyphenated where `Locale` answers with an
    /// underscore. Looking the tag up verbatim is the bug this test exists for:
    /// it would fall through to English on every machine with a region.
    #[test]
    fn localisation_converts_the_tag_to_bcp47() {
        let json = r#"{"name": "AdGuard Extra", "name:pt-PT": "Extra Portugues", "name:pt": "Extra"}"#;
        assert_eq!(meta(json).localised("name", &Locale::parse("pt_PT")), "Extra Portugues");
    }

    /// Region first, then the bare language, then the unsuffixed key — the same
    /// walk `filters.rs` does in SQL.
    #[test]
    fn localisation_falls_back_through_language_to_plain() {
        let json = r#"{"name": "Plain", "name:pt": "Portugues"}"#;
        // No `name:pt-BR`, so the bare language answers.
        assert_eq!(meta(json).localised("name", &Locale::parse("pt_BR")), "Portugues");
        // Neither, so the unsuffixed key does.
        assert_eq!(meta(json).localised("name", &Locale::parse("de_DE")), "Plain");
    }

    /// A present-but-empty translation must fall through rather than render as
    /// a blank name, exactly as `FILTER_SELECT`'s `NULLIF` makes it.
    #[test]
    fn an_empty_translation_falls_through() {
        let json = r#"{"name": "Plain", "name:de": ""}"#;
        assert_eq!(meta(json).localised("name", &Locale::parse("de_DE")), "Plain");
    }

    /// The measured trap: `hello` cannot be named while `hello-world` is
    /// installed, and the CLI refuses the exact id.
    #[test]
    fn an_id_inside_another_id_is_ambiguous() {
        let mut scripts = vec![script("hello", "Hello Sandbox"), script("hello-world", "Hello World")];
        mark_ambiguous(&mut scripts);
        assert!(scripts[0].ambiguous, "hello is a substring of hello-world");
        assert!(!scripts[1].ambiguous, "nothing contains hello-world");
    }

    /// The match runs against titles too, so a rule that only compared ids
    /// would miss this and offer a switch that fails at exit 0.
    #[test]
    fn an_id_inside_another_title_is_ambiguous() {
        let mut scripts = vec![script("extra", "Extra"), script("adguard-extra", "AdGuard Extra")];
        mark_ambiguous(&mut scripts);
        assert!(scripts[0].ambiguous, "extra is a substring of the other title");
    }

    /// Case is ignored by the CLI, so it must be ignored here.
    #[test]
    fn ambiguity_ignores_case() {
        let mut scripts = vec![script("extra", "Extra"), script("other", "The EXTRA Big One")];
        mark_ambiguous(&mut scripts);
        assert!(scripts[0].ambiguous);
    }

    /// Every id contains itself. Comparing a script against itself would mark
    /// the whole page unreachable, including the one-script install everybody
    /// has.
    #[test]
    fn a_script_is_never_ambiguous_with_itself() {
        let mut scripts = vec![script("adguard-extra", "AdGuard Extra")];
        mark_ambiguous(&mut scripts);
        assert!(!scripts[0].ambiguous);
        assert!(scripts[0].actionable());
    }

    /// Two unrelated scripts — the ordinary case, and the one that must not
    /// regress into caution.
    #[test]
    fn unrelated_scripts_are_both_actionable() {
        let mut scripts = vec![
            script("adguard-extra", "AdGuard Extra"),
            script("sponsorblock", "SponsorBlock"),
        ];
        mark_ambiguous(&mut scripts);
        assert!(scripts.iter().all(Userscript::actionable));
    }

    /// The config holds a path and this module holds an id; the join is on the
    /// filename, so the directory component cannot break it.
    #[test]
    fn enabled_matches_on_the_filename() {
        let enabled = ["userscripts/adguard-extra.meta.json"];
        assert!(is_enabled("adguard-extra", &enabled));
        assert!(!is_enabled("adguard", &enabled), "a prefix is not the same file");
        assert!(!is_enabled("hello", &enabled));
    }

    /// Quoting and an absolute path are both things AdGuard has been seen to
    /// write; neither makes a script less enabled.
    #[test]
    fn enabled_tolerates_how_the_path_was_written() {
        assert!(is_enabled("hello", &["  userscripts/hello.meta.json  "]));
        assert!(is_enabled("hello", &["/home/someone/.local/share/adguard-cli/userscripts/hello.meta.json"]));
    }

    /// An install with nothing switched on — the state the CLI writes as
    /// `userscripts: []` once the last script is disabled.
    #[test]
    fn nothing_enabled_leaves_every_script_off() {
        assert!(!is_enabled("adguard-extra", &[]));
    }

    /// A scratch `userscripts/` directory holding the pairs named.
    fn dir(name: &str, scripts: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "adguard-ui-userscripts-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        for (id, json) in scripts {
            std::fs::write(root.join(format!("{id}{META_SUFFIX}")), json).expect("write meta");
            std::fs::write(root.join(format!("{id}.user.js")), "// nothing").expect("write js");
        }
        root
    }

    /// The whole join, end to end: the directory says what is installed, the
    /// config says what is on, and the metadata fills the row.
    #[test]
    fn read_joins_the_directory_with_the_config() {
        let root = dir(
            "join",
            &[
                ("hello", MINIMAL),
                ("zebra", r#"{"name": "Zebra", "version": "2.0"}"#),
            ],
        );
        let scripts = read(&root, &["userscripts/hello.meta.json"], &Locale::english());

        assert_eq!(scripts.len(), 2);
        // Sorted by display name: "Hello Sandbox" before "Zebra".
        assert_eq!(scripts[0].id, "hello");
        assert!(scripts[0].enabled, "the config lists it");
        assert_eq!(scripts[1].id, "zebra");
        assert!(
            !scripts[1].enabled,
            "installed but absent from the config is the disabled state"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The locale the caller passes is the one used. This is the bug the
    /// warning caught: `read` took a `Locale` and then resolved names against
    /// `Locale::from_env()` instead, so every caller's argument was discarded
    /// and the page would have been English on a machine that is not.
    #[test]
    fn read_honours_the_locale_it_is_given() {
        let root = dir(
            "locale",
            &[("x", r#"{"name": "Plain", "name:pl": "Polski"}"#)],
        );
        let scripts = read(&root, &[], &Locale::parse("pl_PL"));
        assert_eq!(scripts[0].name, "Polski");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ambiguity is decided across the whole directory, so it can only be
    /// settled by the function that has read all of it.
    #[test]
    fn read_marks_the_colliding_pair() {
        let root = dir(
            "collide",
            &[
                ("hello", r#"{"name": "Hello Sandbox"}"#),
                ("hello-world", r#"{"name": "Hello World"}"#),
            ],
        );
        let scripts = read(&root, &[], &Locale::english());
        let hello = scripts.iter().find(|s| s.id == "hello").expect("hello");
        let world = scripts.iter().find(|s| s.id == "hello-world").expect("world");
        assert!(hello.ambiguous, "hello cannot be named while hello-world exists");
        assert!(!world.ambiguous);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `.user.js` with no metadata beside it is not a row: every field the
    /// page renders comes from the JSON, so keying on it is deliberate.
    #[test]
    fn read_keys_on_the_metadata_file() {
        let root = dir("keying", &[]);
        std::fs::write(root.join("lonely.user.js"), "// no meta").expect("write");
        assert!(read(&root, &[], &Locale::english()).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory that does not exist is an install that has never had a
    /// script — not an error, and not a reason to fail the page.
    #[test]
    fn read_of_a_missing_directory_is_empty() {
        let missing = std::env::temp_dir().join("adguard-ui-userscripts-nowhere-at-all");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(read(&missing, &[], &Locale::english()).is_empty());
    }

    // --- the catalogue of AdGuard's own scripts ---

    /// Each entry's id must be the stem of its URL's filename, because that is
    /// what AdGuard names the installed pair after.
    ///
    /// The one error in this table that would not show up as a crash or a
    /// missing row: a wrong id leaves the entry in the catalogue forever,
    /// offering to install a script the user already has, with no symptom
    /// anywhere else. Measured ids: `adguard-extra`, `popupblocker`,
    /// `assistant`, `wot`.
    #[test]
    fn recommended_ids_match_their_urls() {
        for entry in &RECOMMENDED {
            let stem = entry
                .url
                .rsplit('/')
                .next()
                .and_then(|file| file.strip_suffix(".user.js"))
                .unwrap_or_else(|| panic!("{} has no .user.js filename", entry.url));
            assert_eq!(
                entry.id, stem,
                "{}'s id must be the stem of its URL",
                entry.name
            );
        }
    }

    /// Every URL is one of AdGuard's own, over https.
    ///
    /// These are the only addresses this application ever puts in front of a
    /// one-click install, so the table is not a place a third-party host may
    /// arrive by an edit nobody looked at twice.
    #[test]
    fn recommended_urls_are_adguards_own_over_https() {
        for entry in &RECOMMENDED {
            assert!(
                entry.url.starts_with("https://userscripts.adtidy.org/"),
                "{} points somewhere else: {}",
                entry.name,
                entry.url
            );
        }
    }

    /// The script this repository ships for install links (#29).
    const INSTALL_LINKS_SOURCE: &str =
        include_str!("../../../data/userscripts/adguard-ui-install-links.user.js");

    /// Installed is not enough: a helper that is switched off is not injected,
    /// so the browser's *Install* opens the script as text, exactly as if it
    /// were missing (11 October 2026, on the reference machine).
    #[test]
    fn a_helper_that_is_switched_off_does_not_work() {
        assert_eq!(install_links(&[]), Helper::Missing(&INSTALL_LINKS));
        assert!(!install_links(&[]).works());

        let mut helper = script(INSTALL_LINKS.id, INSTALL_LINKS.name);
        assert_eq!(install_links(&[helper.clone()]), Helper::Off { ambiguous: false });
        assert!(!install_links(&[helper.clone()]).works());

        helper.enabled = true;
        assert_eq!(install_links(&[helper.clone()]), Helper::On);
        assert!(install_links(&[helper]).works());

        // Another script being on says nothing about the helper.
        let mut other = script("adguard-extra", "AdGuard Extra");
        other.enabled = true;
        assert!(matches!(install_links(&[other]), Helper::Missing(_)));
    }

    /// The one-click address is this repository's own file, on `main`, and
    /// lands under the id the catalogue expects — the same two pins
    /// [`RECOMMENDED`] has, for the same reasons.
    #[test]
    fn install_links_is_this_repositorys_file() {
        let prefix =
            "https://raw.githubusercontent.com/dominik-najberg/AdGuard-UI-Linux/main/data/userscripts/";
        let file = INSTALL_LINKS
            .url
            .strip_prefix(prefix)
            .unwrap_or_else(|| panic!("points somewhere else: {}", INSTALL_LINKS.url));
        assert_eq!(file, format!("{}.user.js", INSTALL_LINKS.id));
    }

    /// The row offered before installation says what the installed one will:
    /// the name and description come from the script's own metadata block.
    #[test]
    fn install_links_matches_its_metadata_block() {
        let tag = |key: &str| {
            INSTALL_LINKS_SOURCE
                .lines()
                .find_map(|line| line.strip_prefix(&format!("// @{key}")))
                .map(str::trim)
                .unwrap_or_else(|| panic!("no @{key}"))
        };
        assert_eq!(tag("name"), INSTALL_LINKS.name);
        assert_eq!(tag("description"), INSTALL_LINKS.description);
        // Where AdGuard re-fetches it from, which has to be where it was
        // installed from or an update would come from somewhere else.
        assert_eq!(tag("downloadURL"), INSTALL_LINKS.url);
        assert_eq!(tag("updateURL"), INSTALL_LINKS.url);
    }

    /// Installed next to all four of AdGuard's scripts, every one of the five
    /// can still be switched and removed: no id is a substring of another's id
    /// or title (see [`mark_ambiguous`]).
    #[test]
    fn install_links_collides_with_nothing_in_the_catalogue() {
        let entries = RECOMMENDED.iter().chain(std::iter::once(&INSTALL_LINKS));
        let mut scripts: Vec<Userscript> = entries
            .map(|entry| script(entry.id, entry.name))
            .collect();
        mark_ambiguous(&mut scripts);
        assert!(scripts.iter().all(|script| !script.ambiguous));
    }

    #[test]
    fn install_links_is_offered_until_installed() {
        assert!(matches!(install_links(&[]), Helper::Missing(entry) if entry.id == INSTALL_LINKS.id));
        let installed = script(INSTALL_LINKS.id, INSTALL_LINKS.name);
        assert!(!matches!(install_links(&[installed]), Helper::Missing(_)));
    }

    /// The four AdGuard's own applications ship, in the state they ship them.
    #[test]
    fn the_catalogue_matches_what_adguard_bundles() {
        let on: Vec<&str> = RECOMMENDED
            .iter()
            .filter(|entry| entry.enabled_by_default)
            .map(|entry| entry.id)
            .collect();
        assert_eq!(on, ["adguard-extra", "popupblocker"]);
        assert_eq!(RECOMMENDED.len(), 4);
    }

    /// No two entries share an id, which would make one of them permanently
    /// un-droppable from the catalogue.
    #[test]
    fn recommended_ids_are_distinct() {
        let mut ids: Vec<_> = RECOMMENDED.iter().map(|entry| entry.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two entries share an id");
    }

    /// **The four can coexist**, which a catalogue offering them together may
    /// not assume: AdGuard matches by substring across ids *and* titles, so a
    /// bundle whose members collided would install four scripts and leave some
    /// of them unswitchable (contract §15).
    ///
    /// Measured against the real four, and re-derived here from the same rule
    /// the page uses — so adding a fifth entry that collides fails here rather
    /// than on a user's machine.
    #[test]
    fn the_recommended_four_do_not_collide_with_each_other() {
        let mut scripts: Vec<Userscript> = RECOMMENDED
            .iter()
            .map(|entry| {
                let mut s = script(entry.id, entry.name);
                s.enabled = entry.enabled_by_default;
                s
            })
            .collect();
        mark_ambiguous(&mut scripts);
        for s in &scripts {
            assert!(
                s.actionable(),
                "{} would be unswitchable alongside the others",
                s.id
            );
        }
    }

    /// An installed script drops out of the catalogue, so nothing is listed
    /// twice and an empty catalogue means "you have them all".
    #[test]
    fn recommended_drops_what_is_installed() {
        assert_eq!(recommended(&[]).len(), 4, "a bare install is offered all four");

        let installed = vec![script("adguard-extra", "AdGuard Extra")];
        let offered = recommended(&installed);
        assert_eq!(offered.len(), 3);
        assert!(
            !offered.iter().any(|entry| entry.id == "adguard-extra"),
            "the installed one is still being offered"
        );

        let all: Vec<Userscript> = RECOMMENDED
            .iter()
            .map(|entry| script(entry.id, entry.name))
            .collect();
        assert!(recommended(&all).is_empty(), "nothing left to offer");
    }

    /// A script the user installed themselves still counts as installed — the
    /// match is on the id, not on where it came from.
    #[test]
    fn recommended_ignores_unrelated_scripts() {
        let installed = vec![script("sponsorblock", "SponsorBlock")];
        assert_eq!(recommended(&installed).len(), 4);
    }

    /// The case #9's screenshot showed: a Greasy Fork script whose homepage is
    /// its author's product site. The page it was installed from is the
    /// listing, and the address AdGuard kept is enough to find it.
    #[test]
    fn a_greasy_fork_update_address_leads_to_the_listing() {
        let url = "https://update.greasyfork.org/scripts/597631/%F0%9F%93%A5%20Free%20All-in-One.user.js";
        assert_eq!(script_page(url).as_deref(), Some("https://greasyfork.org/scripts/597631"));
        assert_eq!(
            script_page("https://update.sleazyfork.org/scripts/12/x.user.js").as_deref(),
            Some("https://sleazyfork.org/scripts/12")
        );
    }

    /// The older `code/` form, with the slug still on the number and a
    /// language in front: the listing keeps the slug, which Greasy Fork
    /// accepts, and drops the language, which it does not need.
    #[test]
    fn a_greasy_fork_code_address_leads_to_the_listing() {
        let url = "https://greasyfork.org/en/scripts/28497-remove-web-limits/code/x.user.js";
        assert_eq!(
            script_page(url).as_deref(),
            Some("https://greasyfork.org/scripts/28497-remove-web-limits")
        );
    }

    #[test]
    fn an_openuserjs_install_address_leads_to_the_script_page() {
        assert_eq!(
            script_page("https://openuserjs.org/install/someone/Nice_Script.user.js").as_deref(),
            Some("https://openuserjs.org/scripts/someone/Nice_Script")
        );
        assert_eq!(
            script_page("https://openuserjs.org/src/scripts/someone/Nice_Script.user.js").as_deref(),
            Some("https://openuserjs.org/scripts/someone/Nice_Script")
        );
    }

    /// The address of this application's own helper, among others.
    #[test]
    fn a_raw_github_address_leads_to_the_file_on_github() {
        let url = "https://raw.githubusercontent.com/dominik-najberg/AdGuard-UI-Linux/main/data/userscripts/adguard-ui-install-links.user.js";
        assert_eq!(
            script_page(url).as_deref(),
            Some("https://github.com/dominik-najberg/AdGuard-UI-Linux/blob/main/data/userscripts/adguard-ui-install-links.user.js")
        );
        // The long ref form the *Raw* button produces now.
        assert_eq!(
            script_page("https://raw.githubusercontent.com/o/r/refs/heads/dev/a/b.user.js").as_deref(),
            Some("https://github.com/o/r/blob/dev/a/b.user.js")
        );
        // An owner and a repository with no file is not a script.
        assert_eq!(script_page("https://raw.githubusercontent.com/o/r/main"), None);
    }

    #[test]
    fn github_raw_and_release_addresses_lead_to_their_pages() {
        assert_eq!(
            script_page("https://github.com/o/r/raw/main/x.user.js").as_deref(),
            Some("https://github.com/o/r/blob/main/x.user.js")
        );
        assert_eq!(
            script_page("https://github.com/o/r/releases/download/v1.2/x.user.js").as_deref(),
            Some("https://github.com/o/r/releases/tag/v1.2")
        );
        assert_eq!(script_page("https://github.com/o/r/issues/1"), None);
    }

    #[test]
    fn a_raw_gist_address_leads_to_the_gist() {
        let url = "https://gist.githubusercontent.com/someone/abc123/raw/def456/x.user.js";
        assert_eq!(script_page(url).as_deref(), Some("https://gist.github.com/someone/abc123"));
    }

    /// AdGuard's own CDN has no page per script, and a guessed one would be a
    /// link that 404s. The caller shows the download address instead.
    #[test]
    fn an_unknown_host_has_no_derived_page() {
        assert_eq!(
            script_page("https://userscripts.adtidy.org/release/assistant/4.3/assistant.user.js"),
            None
        );
        assert_eq!(script_page("not a url"), None);
        assert_eq!(script_page("file:///tmp/x.user.js"), None);
    }

    /// A query or fragment is not part of the path a page is built from.
    #[test]
    fn a_query_does_not_leak_into_the_page() {
        assert_eq!(
            script_page("https://update.greasyfork.org/scripts/1/x.user.js?v=2").as_deref(),
            Some("https://greasyfork.org/scripts/1")
        );
    }

    #[test]
    fn the_host_note_is_short_and_recognisable() {
        assert_eq!(host("https://www.Example.org:8443/a").as_deref(), Some("example.org"));
        assert_eq!(host("http://user@127.0.0.1:8731/hello.user.js").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("https://greasyfork.org").as_deref(), Some("greasyfork.org"));
        assert_eq!(host("ftp://example.org/x"), None);
    }

    /// The alias is what a row is called and sorted by; the original title is
    /// still what the CLI matches, so it alone decides ambiguity.
    #[test]
    fn an_alias_renames_the_row_but_not_what_the_cli_matches() {
        let mut scripts = vec![script("extra", "Extra"), script("other", "Other")];
        scripts[1].alias = Some("Extra stuff".to_owned());
        mark_ambiguous(&mut scripts);
        assert!(!scripts[0].ambiguous, "the alias contains `extra`, but the CLI never sees it");
        assert_eq!(scripts[1].display_name(), "Extra stuff");
        assert_eq!(scripts[1].original_name(), "Other");

        // A blank alias is no alias.
        scripts[1].alias = Some("  ".to_owned());
        assert_eq!(scripts[1].display_name(), "Other");
    }
}
