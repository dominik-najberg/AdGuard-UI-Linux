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
use crate::model::{Recommended, Userscript, RECOMMENDED};

/// The suffix a metadata file carries, after the id.
const META_SUFFIX: &str = ".meta.json";

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
    scripts.sort_by_key(|script| script.display_name().to_lowercase());
    scripts
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
            id,
            enabled,
            // Set by `mark_ambiguous` once every sibling is known; it cannot be
            // decided from one file.
            ambiguous: false,
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
            enabled: false,
            ambiguous: false,
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
}
