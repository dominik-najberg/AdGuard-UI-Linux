//! Carrying the user's edits to a userscript across an update.
//!
//! An update replaces `<id>.user.js` wholesale — AdGuard downloads the new
//! version over whatever is there, edited or not (contract §15). The Extensions
//! page keeps two copies of its own beside it: `base`, the script as its author
//! last shipped it, and `mine`, the user's edited copy. With the new upstream
//! version as the third, that is a textbook three-way merge: the change from
//! `base` to `mine` is the user's, the change from `base` to `upstream` is the
//! author's, and where the two do not touch the same lines both survive.
//!
//! ## Why a dependency, when this workspace counts them
//!
//! `userscripts.rs` reads JSON without a JSON crate because an existing
//! dependency already parses it. Nothing here already merges text, and a diff3
//! is not a weekend's code: the line diff, the hunk alignment and the conflict
//! rendering are each a place to be subtly wrong, and wrong here means quietly
//! dropping a line of someone's script. `diffy` is that and nothing else —
//! Myers diff plus the merge `git merge-file` performs — with its colour and
//! binary-patch features off by default. Its one mandatory dependency,
//! `hashbrown` 0.17, was already in `Cargo.lock` when it was added (25 November
//! 2026), so it brought in no crate but itself.
//!
//! ## What a conflict looks like
//!
//! Git's markers, in diff3 style so the original lines sit between the two
//! sides — the version the user is most likely to need when deciding:
//!
//! ```text
//! <<<<<<< your version
//! …
//! ||||||| original
//! …
//! =======
//! …
//! >>>>>>> 2.7.2
//! ```
//!
//! That text is never written over the installed script. It exists for *Open
//! both in editor*, and a script with markers in it is not JavaScript.

/// What a merge produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// Both sides' changes, combined. Ready to install.
    Clean(String),
    /// At least one place where both sides changed the same lines.
    ///
    /// `marked` is the whole script with every conflict between markers, for
    /// a person to resolve in an editor; `conflicts` is how many there are.
    Conflict { marked: String, conflicts: usize },
}

/// Merge the user's edits into a new upstream version.
///
/// `base` is the version the user edited, `mine` their edited copy, `upstream`
/// the new version. `upstream_label` names the new side in the markers — the
/// version number is the most useful thing to put there.
///
/// Two cases need no merge and are answered without one, which also keeps them
/// exact rather than merely equivalent:
///
/// - `mine == base`: nothing was edited, so the update is simply `upstream`;
/// - `upstream == base`: nothing changed upstream, so the user's copy stands.
pub fn merge(base: &str, mine: &str, upstream: &str, upstream_label: &str) -> Merged {
    if mine == base {
        return Merged::Clean(upstream.to_owned());
    }
    if upstream == base || upstream == mine {
        return Merged::Clean(mine.to_owned());
    }

    // Markers one longer than any run of `<` already opening a line in the
    // three inputs, so that a script which happens to contain `<<<<<<<` — in a
    // string, in a comment — can never be mistaken for a conflict, and the
    // count below counts ours alone.
    let length = [base, mine, upstream]
        .iter()
        .map(|text| longest_marker_run(text))
        .max()
        .unwrap_or(0)
        .max(6)
        + 1;

    let mut options = diffy::MergeOptions::new();
    options
        .set_conflict_style(diffy::ConflictStyle::Diff3)
        .set_conflict_marker_length(length)
        // Every marker starts a line, as `git merge-file` writes them: a
        // marker glued to the end of a script's last line is one an editor's
        // conflict highlighting does not recognise.
        .set_incomplete_hunk_style(diffy::IncompleteHunkStyle::Git);

    match options.merge(base, mine, upstream) {
        Ok(merged) => Merged::Clean(merged),
        Err(marked) => {
            let opening = format!("{} ", "<".repeat(length));
            let conflicts = marked.lines().filter(|line| line.starts_with(&opening)).count();
            // diffy labels its sides `ours`, `original` and `theirs`. Ours are
            // the user's words; renamed after the fact so the labels can say
            // which side is whose.
            let marked = relabel(&marked, length, upstream_label);
            Merged::Conflict { marked, conflicts: conflicts.max(1) }
        }
    }
}

/// The longest run of `<` that starts a line in `text`.
fn longest_marker_run(text: &str) -> usize {
    text.lines()
        .map(|line| line.chars().take_while(|&c| c == '<').count())
        .max()
        .unwrap_or(0)
}

/// Replace diffy's side labels with ones a person can read.
fn relabel(marked: &str, length: usize, upstream_label: &str) -> String {
    // `original` reads well as it is, so only the two sides are renamed.
    let open = "<".repeat(length);
    let close = ">".repeat(length);
    let mut out = String::with_capacity(marked.len());
    for line in marked.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        let replaced = if body == format!("{open} ours") {
            format!("{open} your version")
        } else if body == format!("{close} theirs") {
            format!("{close} {upstream_label}")
        } else {
            body.to_owned()
        };
        out.push_str(&replaced);
        out.push_str(newline);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "// ==UserScript==\n// @version 1.0\n// ==/UserScript==\nconst a = 1;\nconst b = 2;\nconst c = 3;\n";

    #[test]
    fn an_unedited_script_simply_takes_the_update() {
        let upstream = BASE.replace("1.0", "1.1").replace("b = 2", "b = 20");
        assert_eq!(merge(BASE, BASE, &upstream, "1.1"), Merged::Clean(upstream));
    }

    #[test]
    fn an_unchanged_upstream_keeps_the_edit() {
        let mine = BASE.replace("c = 3", "c = 30");
        assert_eq!(merge(BASE, &mine, BASE, "1.0"), Merged::Clean(mine));
    }

    /// The case the feature exists for: the author bumped the version and
    /// changed one line, the user changed another, and both survive.
    #[test]
    fn edits_on_different_lines_are_both_kept() {
        let mine = BASE.replace("c = 3", "c = 30");
        let upstream = BASE.replace("@version 1.0", "@version 1.1").replace("a = 1", "a = 10");
        let Merged::Clean(merged) = merge(BASE, &mine, &upstream, "1.1") else {
            panic!("no overlap, so no conflict");
        };
        assert!(merged.contains("@version 1.1"), "the author's version");
        assert!(merged.contains("a = 10"), "the author's change");
        assert!(merged.contains("c = 30"), "the user's change");
    }

    #[test]
    fn the_same_line_changed_both_ways_is_a_conflict() {
        let mine = BASE.replace("b = 2", "b = 'mine'");
        let upstream = BASE.replace("b = 2", "b = 'theirs'");
        let Merged::Conflict { marked, conflicts } = merge(BASE, &mine, &upstream, "1.1") else {
            panic!("both changed line b");
        };
        assert_eq!(conflicts, 1);
        assert!(marked.contains("<<<<<<< your version\n"));
        assert!(marked.contains("||||||| original\n"));
        assert!(marked.contains(">>>>>>> 1.1\n"));
        assert!(marked.contains("b = 'mine'") && marked.contains("b = 'theirs'") && marked.contains("b = 2"));
    }

    /// A script that itself contains a marker-like line must not be counted
    /// as a conflict, nor confuse the markers the merge adds.
    #[test]
    fn marker_text_in_the_script_is_not_a_conflict() {
        // Inside a template literal, where a line really can start with it.
        let base = format!("{BASE}const doc = `\n<<<<<<< not a marker\n`;\n");
        let mine = base.replace("c = 3", "c = 30");
        let upstream = base.replace("a = 1", "a = 10");
        assert!(matches!(merge(&base, &mine, &upstream, "1.1"), Merged::Clean(_)));

        let mine = base.replace("b = 2", "b = 'mine'");
        let upstream = base.replace("b = 2", "b = 'theirs'");
        let Merged::Conflict { marked, conflicts } = merge(&base, &mine, &upstream, "1.1") else {
            panic!("both changed line b");
        };
        assert_eq!(conflicts, 1, "the script's own `<<<<<<<` line is not counted");
        assert!(marked.contains("<<<<<<<< your version\n"), "markers lengthened past the script's");
    }

    /// A script without a final newline must still put every marker on a
    /// line of its own.
    #[test]
    fn a_missing_final_newline_keeps_markers_on_their_own_lines() {
        let base = "x = 1";
        let Merged::Conflict { marked, .. } = merge(base, "x = 2", "x = 3", "1.1") else {
            panic!("both changed the only line");
        };
        for line in marked.lines() {
            assert!(
                !(line.len() > 7 && (line.ends_with("=======") || line.contains("x = 2|||"))),
                "a marker glued to a line: {line:?}"
            );
        }
        assert!(marked.lines().any(|line| line == "x = 2"));
        assert!(marked.lines().any(|line| line == "x = 3"));
    }
}
