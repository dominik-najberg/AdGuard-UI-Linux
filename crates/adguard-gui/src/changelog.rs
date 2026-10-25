//! *What's new*: the recent releases of AdGuard UI, read from `CHANGELOG.md`.
//!
//! **Built in, not fetched.** The changelog is compiled into the binary, so the
//! About page can show it at any time — not only after a check found a newer
//! release — and showing it sends nothing anywhere. The release check stays
//! the one request this application makes by itself.
//!
//! The parser knows exactly the Markdown this project's changelog uses: `## `
//! releases, `### ` headings, paragraphs (hard-wrapped in older entries), `- `
//! lists, and `*`, `**`, `` ` `` and `[text](url)` inline. Anything else is
//! shown as text rather than guessed at.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// How many releases the dialog shows. Enough to cover a few skipped updates;
/// the rest is in the repository.
const SHOWN: usize = 5;

/// Where the full history is, for the line at the foot of the dialog.
const FULL_HISTORY: &str =
    "https://github.com/dominik-najberg/AdGuard-UI-Linux/blob/main/CHANGELOG.md";

#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub date: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, PartialEq)]
pub struct Section {
    /// Empty for text that comes before any `### ` heading.
    pub title: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, PartialEq)]
pub enum Block {
    Paragraph(String),
    Item(String),
}

/// The releases in `text`, newest first, as the file orders them.
pub fn parse(text: &str) -> Vec<Release> {
    let mut releases: Vec<Release> = Vec::new();
    let mut fenced = false;
    // The block being joined across hard-wrapped lines.
    let mut open: Option<Block> = None;

    fn flush(releases: &mut [Release], open: &mut Option<Block>) {
        let Some(block) = open.take() else { return };
        let Some(release) = releases.last_mut() else { return };
        if release.sections.is_empty() {
            release.sections.push(Section { title: String::new(), blocks: Vec::new() });
        }
        release.sections.last_mut().expect("just ensured").blocks.push(block);
    }

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            flush(&mut releases, &mut open);
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            flush(&mut releases, &mut open);
            let (version, date) = match heading.split_once(" — ") {
                Some((version, date)) => (version.trim(), date.trim()),
                None => (heading.trim(), ""),
            };
            releases.push(Release { version: version.to_owned(), date: date.to_owned(), sections: Vec::new() });
            continue;
        }
        if releases.is_empty() {
            continue; // the preamble above the first release
        }
        if let Some(title) = line.strip_prefix("### ") {
            flush(&mut releases, &mut open);
            let release = releases.last_mut().expect("checked above");
            release.sections.push(Section { title: title.trim().to_owned(), blocks: Vec::new() });
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            flush(&mut releases, &mut open);
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            flush(&mut releases, &mut open);
            open = Some(Block::Item(item.to_owned()));
            continue;
        }
        match &mut open {
            Some(Block::Paragraph(text) | Block::Item(text)) => {
                text.push(' ');
                text.push_str(trimmed);
            }
            None => open = Some(Block::Paragraph(trimmed.to_owned())),
        }
    }
    flush(&mut releases, &mut open);
    releases
}

/// Pango markup for one block of changelog text: escaped, then `**bold**`,
/// `*italic*`, `` `code` `` and `[text](url)` (the text alone — the dialog is
/// not a browser).
pub fn markup(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str("<tt>");
                out.push_str(&gtk::glib::markup_escape_text(&after[..end]));
                out.push_str("</tt>");
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                out.push_str("<b>");
                out.push_str(&markup(&after[..end]));
                out.push_str("</b>");
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('*') {
            if let Some(end) = after.find('*').filter(|end| *end > 0) {
                out.push_str("<i>");
                out.push_str(&markup(&after[..end]));
                out.push_str("</i>");
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('[') {
            if let Some((label, tail)) = after.split_once("](") {
                if let Some(close) = tail.find(')') {
                    if !label.contains(']') {
                        out.push_str(&markup(label));
                        rest = &tail[close + 1..];
                        continue;
                    }
                }
            }
        }
        out.push_str(&gtk::glib::markup_escape_text(&c.to_string()));
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Show the recent releases in a dialog over `parent`.
pub fn present(parent: &impl IsA<gtk::Widget>) {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();

    let releases = parse(CHANGELOG);
    for (index, release) in releases.iter().take(SHOWN).enumerate() {
        if index > 0 {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.set_margin_top(12);
            separator.set_margin_bottom(6);
            content.append(&separator);
        }
        content.append(&release_heading(release));
        for section in &release.sections {
            if !section.title.is_empty() {
                let title = label(&markup(&section.title));
                title.add_css_class("heading");
                title.set_margin_top(6);
                content.append(&title);
            }
            for block in &section.blocks {
                content.append(&match block {
                    Block::Paragraph(text) => label(&markup(text)),
                    Block::Item(text) => label(&format!("•  {}", markup(text))),
                });
            }
        }
    }


    let clamp = adw::Clamp::builder().maximum_size(640).child(&content).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();
    let view = adw::ToolbarView::builder()
        .content(&scroller)
        .top_bar_style(adw::ToolbarStyle::Raised)
        .bottom_bar_style(adw::ToolbarStyle::Raised)
        .build();
    view.add_top_bar(&adw::HeaderBar::new());
    // In a bar of its own rather than at the foot of the text: it is the one
    // focusable widget in the dialog, and at the foot the first focus scrolled
    // the dialog to it — opening on the oldest release instead of the newest.
    let more = gtk::LinkButton::builder()
        .uri(FULL_HISTORY)
        .label("Full history on GitHub")
        .halign(gtk::Align::Center)
        .build();
    view.add_bottom_bar(&more);

    adw::Dialog::builder()
        .title("What's New in AdGuard UI")
        .content_width(640)
        .content_height(640)
        .child(&view)
        .build()
        .present(Some(parent));
}

/// The version large, the date beside it, and *Installed* on this copy's own.
fn release_heading(release: &Release) -> gtk::Box {
    let heading = gtk::Box::builder().spacing(12).build();
    let version = gtk::Label::builder().label(&release.version).xalign(0.0).build();
    version.add_css_class("title-2");
    heading.append(&version);
    if release.version == env!("CARGO_PKG_VERSION") {
        let installed = gtk::Label::builder()
            .label("Installed")
            .valign(gtk::Align::Center)
            .build();
        installed.add_css_class("accent");
        installed.add_css_class("caption-heading");
        heading.append(&installed);
    }
    let date = gtk::Label::builder()
        .label(&release.date)
        .hexpand(true)
        .xalign(1.0)
        .valign(gtk::Align::Center)
        .build();
    date.add_css_class("dim-label");
    heading.append(&date);
    heading
}

fn label(markup: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(markup)
        .use_markup(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        // Not selectable: the dialog's first focus would land on the first
        // label and open with its text highlighted.
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_changelog_starts_with_this_version() {
        let releases = parse(CHANGELOG);
        assert_eq!(releases[0].version, env!("CARGO_PKG_VERSION"));
        assert!(!releases[0].sections.is_empty());
    }

    #[test]
    fn releases_headings_and_wrapped_paragraphs() {
        let text = "# Changelog\n\nPreamble.\n\n---\n\n## 2.0.0 — 1 January 2027\n\n### New\n\nOne line\nwrapped.\n\n- an item\n  continued\n- another\n\n```\ncode\n```\n\n## 1.0.0 — 1 January 2026\n\nNo heading.\n";
        let releases = parse(text);
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].version, "2.0.0");
        assert_eq!(releases[0].date, "1 January 2027");
        assert_eq!(
            releases[0].sections,
            vec![Section {
                title: "New".to_owned(),
                blocks: vec![
                    Block::Paragraph("One line wrapped.".to_owned()),
                    Block::Item("an item continued".to_owned()),
                    Block::Item("another".to_owned()),
                ],
            }]
        );
        assert_eq!(releases[1].sections[0].title, "");
        assert_eq!(releases[1].sections[0].blocks, vec![Block::Paragraph("No heading.".to_owned())]);
    }

    #[test]
    fn inline_markdown_becomes_escaped_markup() {
        assert_eq!(
            markup("*Check* **Allow** `@@||a^` & [docs](https://x) <b>"),
            "<i>Check</i> <b>Allow</b> <tt>@@||a^</tt> &amp; docs &lt;b&gt;"
        );
        assert_eq!(markup("a * b"), "a * b");
    }
}
