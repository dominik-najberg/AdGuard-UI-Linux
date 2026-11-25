//! The userscripts that make `check-update` crash, found before it does.
//!
//! Measured on AdGuard CLI 1.5.1 (contract §15, *What `check-update` does to a
//! userscript*): an **enabled** userscript whose own `==UserScript==` block
//! names neither `@updateURL` nor `@downloadURL` makes `check-update` die of a
//! segmentation fault in its userscripts step. Safe Browsing, CRLite and the
//! application check after it never run, and the output stops mid-report.
//!
//! ## Why the script's source, and not AdGuard's metadata file
//!
//! The metadata file cannot tell the two apart. AdGuard records the address a
//! script was installed from as its `downloadURL` whether or not the source
//! declared one, and leaves `updateURL` empty in both cases — measured, the
//! `.meta.json` of a script that crashes the update and of one that does not
//! differed in nothing but the URL itself. Only the block at the top of
//! `<id>.user.js` says which kind it is, so that is what is read: its first
//! [`preview::LIMIT`] bytes, as the install dialog reads a script's head.
//!
//! Either address on its own is enough to be safe: a script declaring only
//! `@updateURL` was measured not to crash, and one declaring only
//! `@downloadURL` is the ordinary empty-`updateURL` case.
//!
//! A disabled script is skipped by `check-update` entirely, so it is never a
//! suspect, and switching one off is a complete remedy.

use std::io::Read;
use std::path::Path;

use crate::model::Userscript;
use crate::preview;

/// The enabled scripts in `dir` that `check-update` is expected to crash on.
///
/// `scripts` is what [`crate::userscripts::read`] returned for the same
/// directory. A script whose source cannot be read is **not** reported: this
/// names a cause, and an unreadable file is no evidence of one.
pub fn suspects<'a>(dir: &Path, scripts: &'a [Userscript]) -> Vec<&'a Userscript> {
    scripts
        .iter()
        .filter(|script| script.enabled)
        .filter(|script| {
            head(&dir.join(format!("{}.user.js", script.id)))
                .is_some_and(|source| names_no_update_address(&source))
        })
        .collect()
}

/// Whether a script's metadata block names neither update address.
///
/// `false` for text with no block at all: AdGuard does not install such a
/// thing, so whatever this is, it is not the measured case.
pub fn names_no_update_address(source: &str) -> bool {
    let mut lines = source.lines().map(str::trim);
    if !lines.any(|line| line.starts_with("//") && line.contains("==UserScript==")) {
        return false;
    }
    for line in lines {
        if line.starts_with("//") && line.contains("==/UserScript==") {
            break;
        }
        let Some(tag) = line.strip_prefix("//").map(str::trim_start).and_then(|tag| tag.strip_prefix('@')) else {
            continue;
        };
        let (key, value) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        if matches!(key, "updateURL" | "downloadURL") && !value.trim().is_empty() {
            return false;
        }
    }
    true
}

/// The first [`preview::LIMIT`] bytes of a file, as text.
fn head(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(preview::LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(extra: &str) -> String {
        format!("// ==UserScript==\n// @name Probe\n// @version 1.0\n{extra}// ==/UserScript==\n(function(){{}})();\n")
    }

    #[test]
    fn a_block_with_neither_address_is_the_crashing_kind() {
        assert!(names_no_update_address(&block("")));
        // An empty value names nothing.
        assert!(names_no_update_address(&block("// @updateURL\n// @downloadURL   \n")));
    }

    #[test]
    fn either_address_alone_is_enough() {
        assert!(!names_no_update_address(&block("// @updateURL https://x/a.meta.js\n")));
        assert!(!names_no_update_address(&block("// @downloadURL https://x/a.user.js\n")));
    }

    /// An address mentioned after the block, in the code, is not in the
    /// metadata and does not count.
    #[test]
    fn only_the_block_is_read() {
        let source = format!("{}// @updateURL https://x/a.meta.js\n", block(""));
        assert!(names_no_update_address(&source));
    }

    #[test]
    fn text_without_a_block_is_not_a_suspect() {
        assert!(!names_no_update_address("console.log(1);\n"));
        assert!(!names_no_update_address(""));
    }

    #[test]
    fn only_enabled_readable_scripts_are_suspects() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-update-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bare.user.js"), block("")).unwrap();
        std::fs::write(dir.join("off.user.js"), block("")).unwrap();
        std::fs::write(dir.join("safe.user.js"), block("// @updateURL https://x/a.meta.js\n")).unwrap();

        let script = |id: &str, enabled: bool| Userscript {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            version: Some("1.0".to_owned()),
            homepage: None,
            download_url: None,
            enabled,
            ambiguous: false,
        };
        let scripts = vec![script("bare", true), script("off", false), script("safe", true), script("gone", true)];
        let found: Vec<&str> = suspects(&dir, &scripts).iter().map(|s| s.id.as_str()).collect();
        assert_eq!(found, ["bare"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
