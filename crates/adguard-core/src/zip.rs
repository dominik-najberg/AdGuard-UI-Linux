//! Reading a zip's **file list**, and nothing else.
//!
//! # Why this exists
//!
//! `export-settings` and `export-logs` write to the **same default filename**
//! (`cli-contract.md` §13), and `import-settings` accepts a logs zip at exit 0
//! with wording identical to the correct case, producing a partial install. So
//! a file picker wired straight to `import-settings` is unsafe, and the zip's
//! own manifest is the only thing that tells the two apart.
//!
//! # Why it is hand-rolled
//!
//! The owner's call, 2 August 2026, from the three options `handoff.md` §3 item
//! 10 put up: the `zip` crate, shelling out to `unzip`, or reading the central
//! directory here. The third. It keeps the workspace's dependency list to GTK
//! and rusqlite, and — the reason it is cheap — **nothing here decompresses
//! anything**. A listing needs the central directory and no more, which is the
//! easy half of the format: fixed-size records, little-endian, no compression
//! involved even when every entry in the archive is deflated.
//!
//! Shelling out to `unzip` was the option to avoid for a measured reason rather
//! than a stylistic one. It would be an **undeclared runtime dependency**, which
//! is exactly the trap contract §5 records AdGuard itself falling into with
//! `gdbus`: present on the developer's machine, absent on someone else's, and
//! failing at the moment the user needs it.
//!
//! # What it deliberately does not do
//!
//! No extraction, no decompression, no writing. Reading a manifest is a
//! listing, not a protocol — contract §13's words — and this module is the
//! whole of that boundary.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The End of Central Directory record: `PK\x05\x06`.
const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
/// A central-directory file header: `PK\x01\x02`.
const CENTRAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
/// EOCD without a comment.
const EOCD_MIN: usize = 22;
/// The archive comment length is a `u16`, so the EOCD starts at most this far
/// from the end of the file.
const MAX_COMMENT: usize = u16::MAX as usize;
/// Fixed part of a central-directory header, before the variable-length name.
const CENTRAL_FIXED: usize = 46;

/// A zip this application will not read, and why — in words a row can show.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Could not be opened or read at all.
    Unreadable(String),
    /// No End of Central Directory record. Not a zip, or truncated.
    NotAZip,
    /// Structurally a zip, but the central directory does not parse.
    Damaged(&'static str),
    /// Zip64. Refused rather than guessed — see [`entries`].
    Zip64,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "the file could not be read: {why}"),
            Self::NotAZip => write!(f, "this is not a zip file"),
            Self::Damaged(what) => write!(f, "this zip file is damaged: {what}"),
            Self::Zip64 => write!(f, "this zip file uses a format AdGuard's exports do not"),
        }
    }
}

impl std::error::Error for Error {}

/// Which AdGuard bundle this is, decided on the manifest alone.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Bundle {
    /// `export-settings`. Safe to import.
    Settings,
    /// `export-logs`. Importable — at exit 0, with a partial install — and the
    /// case the whole module exists to catch.
    Logs,
    /// A zip, but not one AdGuard wrote.
    Neither,
}

/// Every entry name in the archive's central directory, in stored order.
///
/// **Zip64 is refused rather than guessed at.** When an archive holds 65,535
/// entries or passes 4 GiB, the real values move into a Zip64 record and the
/// fields read here hold `0xFFFF`/`0xFFFFFFFF` sentinels. AdGuard's own exports
/// are nine entries and 14.9 MB (contract §13), so this is not a shape they
/// take; treating a sentinel as a real offset would produce a confident wrong
/// answer, which is worse than a refusal a row can word.
pub fn entries(path: &Path) -> Result<Vec<String>, Error> {
    let mut file = File::open(path).map_err(|err| Error::Unreadable(err.to_string()))?;
    let len = file
        .metadata()
        .map_err(|err| Error::Unreadable(err.to_string()))?
        .len();
    if (len as usize) < EOCD_MIN {
        return Err(Error::NotAZip);
    }

    // The EOCD is last, but a trailing archive comment can push it up to 64 KiB
    // from the end — so it is found by scanning backwards for the signature
    // rather than by seeking to a fixed offset.
    let tail_len = (EOCD_MIN + MAX_COMMENT).min(len as usize);
    let tail_start = len - tail_len as u64;
    let mut tail = vec![0u8; tail_len];
    file.seek(SeekFrom::Start(tail_start))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|err| Error::Unreadable(err.to_string()))?;

    // Backwards, because a *stored* entry could contain these four bytes as its
    // own data. The last match is the real record.
    let eocd = (0..=tail_len - EOCD_MIN)
        .rev()
        .find(|&i| tail[i..i + 4] == EOCD_SIGNATURE)
        .ok_or(Error::NotAZip)?;
    let eocd = &tail[eocd..];

    let count = u16(&eocd[10..12]);
    let dir_offset = u32(&eocd[16..20]);
    if count == u16::MAX || dir_offset == u32::MAX {
        return Err(Error::Zip64);
    }
    if u64::from(dir_offset) >= len {
        return Err(Error::Damaged("the central directory is past the end"));
    }

    let mut dir = Vec::new();
    file.seek(SeekFrom::Start(u64::from(dir_offset)))
        .and_then(|_| file.read_to_end(&mut dir))
        .map_err(|err| Error::Unreadable(err.to_string()))?;

    let mut names = Vec::with_capacity(count as usize);
    let mut at = 0usize;
    for _ in 0..count {
        if at + CENTRAL_FIXED > dir.len() || dir[at..at + 4] != CENTRAL_SIGNATURE {
            return Err(Error::Damaged("an entry header is missing or misplaced"));
        }
        let name_len = u16(&dir[at + 28..at + 30]) as usize;
        let extra_len = u16(&dir[at + 30..at + 32]) as usize;
        let comment_len = u16(&dir[at + 32..at + 34]) as usize;
        let name_at = at + CENTRAL_FIXED;
        let name_end = name_at + name_len;
        if name_end > dir.len() {
            return Err(Error::Damaged("an entry name runs past the directory"));
        }
        // Zip names are CP437 or UTF-8 depending on a flag. Every name AdGuard
        // writes is ASCII, where the two agree, so a lossy read cannot corrupt
        // a name this module then matches on — and a name it *cannot* read is
        // one that was never going to match anyway.
        names.push(String::from_utf8_lossy(&dir[name_at..name_end]).into_owned());
        at = name_end + extra_len + comment_len;
    }
    Ok(names)
}

/// Which bundle a manifest describes.
///
/// **Decided on the entries only one of them has.** Both bundles carry
/// `proxy.yaml` *and* `config.txt` (contract §13), so neither of those can
/// discriminate, and a check written against them would call every logs zip a
/// settings zip — which is the exact failure this module exists to prevent.
///
/// Logs is tested **first**. A zip holding both markers is not something
/// AdGuard writes, so it is somebody's hand-assembled archive; calling it Logs
/// refuses the import, and refusing an ambiguous bundle is the safe direction.
pub fn classify(entries: &[String]) -> Bundle {
    let has = |name: &str| entries.iter().any(|entry| entry == name);
    if has("app.log") || has("proxy.log") {
        Bundle::Logs
    } else if has("filters.yaml") || has("agflm_standard.db") {
        Bundle::Settings
    } else {
        Bundle::Neither
    }
}

fn u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures are written by **python's `zipfile`**, not by anything in
    /// this repository. A reader tested against its own writer proves the two
    /// agree with each other and nothing about the format.
    fn fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("adguard-zip-test-{name}"));
        std::fs::write(&path, bytes).expect("fixture");
        path
    }

    const SETTINGS: &[u8] = include_bytes!("../tests/fixtures/export-settings.zip");
    const LOGS: &[u8] = include_bytes!("../tests/fixtures/export-logs.zip");
    const COMMENTED: &[u8] = include_bytes!("../tests/fixtures/commented.zip");

    /// The manifest contract §13 measured, read back name for name — including
    /// the two under `userscripts/`, because a path separator in a name is
    /// where a fixed-offset parser goes wrong.
    #[test]
    fn a_settings_bundle_lists_all_nine_entries() {
        let names = entries(&fixture("settings", SETTINGS)).expect("readable");
        assert_eq!(names.len(), 9);
        for expected in [
            "proxy.yaml",
            "https_exclusions.txt",
            "user.txt",
            "browsers.yaml",
            "userscripts/adguard-extra.meta.json",
            "userscripts/adguard-extra.user.js",
            "agflm_standard.db",
            "filters.yaml",
            "config.txt",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected} missing");
        }
    }

    #[test]
    fn a_logs_bundle_lists_all_six_entries() {
        let names = entries(&fixture("logs", LOGS)).expect("readable");
        assert_eq!(names.len(), 6);
        assert!(names.iter().any(|n| n == "app.log"));
        assert!(names.iter().any(|n| n == "proxy.log.1"));
    }

    /// The whole point of the module.
    #[test]
    fn the_two_bundles_are_told_apart() {
        let settings = entries(&fixture("settings2", SETTINGS)).expect("readable");
        let logs = entries(&fixture("logs2", LOGS)).expect("readable");
        assert_eq!(classify(&settings), Bundle::Settings);
        assert_eq!(classify(&logs), Bundle::Logs);
    }

    /// Both bundles carry these, so a check written against either would call
    /// a logs zip a settings zip — the exact failure this module prevents.
    /// Asserted rather than trusted, because it is a fact about AdGuard that a
    /// future version could change.
    #[test]
    fn the_shared_entries_really_are_shared() {
        let settings = entries(&fixture("settings3", SETTINGS)).expect("readable");
        let logs = entries(&fixture("logs3", LOGS)).expect("readable");
        for shared in ["proxy.yaml", "config.txt"] {
            assert!(settings.iter().any(|n| n == shared), "settings lost {shared}");
            assert!(logs.iter().any(|n| n == shared), "logs lost {shared}");
        }
    }

    /// An archive comment pushes the EOCD off the end of the file. A parser
    /// that seeks to `len - 22` reads garbage here and reports "not a zip".
    #[test]
    fn a_trailing_archive_comment_does_not_hide_the_directory() {
        let names = entries(&fixture("commented", COMMENTED)).expect("readable");
        assert_eq!(names, vec!["filters.yaml".to_owned()]);
    }

    #[test]
    fn something_that_is_not_a_zip_is_refused_rather_than_parsed() {
        let path = fixture("plain", b"this is a text file, not an archive at all");
        assert_eq!(entries(&path), Err(Error::NotAZip));
    }

    /// Shorter than an EOCD. The length check exists so the backward scan
    /// cannot underflow.
    #[test]
    fn a_file_too_short_to_hold_a_record_is_refused() {
        assert_eq!(entries(&fixture("tiny", b"PK")), Err(Error::NotAZip));
    }

    #[test]
    fn a_missing_file_says_so_rather_than_claiming_it_is_not_a_zip() {
        let missing = std::env::temp_dir().join("adguard-zip-test-does-not-exist");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(entries(&missing), Err(Error::Unreadable(_))));
    }

    /// A truncated archive: the EOCD points at a central directory that is not
    /// there. It must not be read as an empty or partial manifest, because a
    /// manifest with no `app.log` in it classifies as a *settings* bundle.
    #[test]
    fn a_truncated_directory_is_damaged_rather_than_an_empty_manifest() {
        let mut bytes = SETTINGS.to_vec();
        let eocd = bytes.len() - EOCD_MIN;
        // Point the directory offset at the last byte, which holds no header.
        let bad = ((bytes.len() - 1) as u32).to_le_bytes();
        bytes[eocd + 16..eocd + 20].copy_from_slice(&bad);
        let names = entries(&fixture("truncated", &bytes));
        assert!(matches!(names, Err(Error::Damaged(_))), "got {names:?}");
    }

    /// A zip that is neither of AdGuard's is not silently treated as one.
    #[test]
    fn an_unrelated_zip_is_neither() {
        assert_eq!(classify(&["notes.txt".to_owned()]), Bundle::Neither);
        assert_eq!(classify(&[]), Bundle::Neither);
    }

    /// An archive carrying both markers is not something AdGuard writes, so it
    /// is hand-assembled. Refusing the import is the safe direction.
    #[test]
    fn an_ambiguous_archive_is_treated_as_logs() {
        let both = ["filters.yaml".to_owned(), "app.log".to_owned()];
        assert_eq!(classify(&both), Bundle::Logs);
    }
}
