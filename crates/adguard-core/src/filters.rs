//! Read-only access to AdGuard's filter catalogue.
//!
//! `agflm_standard.db` (HTTP) and `agflm_dns.db` (DNS) are plain SQLite 3
//! databases in AdGuard's data dir. Their `filter` table carries id, group,
//! title, description, homepage and the `is_enabled` / `is_installed` /
//! `is_trusted` flags — strictly better data than `adguard-cli filters list`
//! prints, whose fixed-width title column overflows and collides with the
//! status field for long names (see `docs/cli-contract.md` section 6).
//!
//! Names come from `filter_localisation` and `filter_group_localisation`
//! rather than the English `title`/`name` columns, matched against the system
//! locale — see [`crate::locale`] for the tag form those tables use.
//!
//! These are the running daemon's live databases. Always open read-only;
//! every mutation goes through `adguard-cli filters ...`.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::locale::Locale;
use crate::model::{Filter, FilterCatalogue, FilterGroup, FilterSet, FilterState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("filter database not found — has AdGuard CLI been run yet?")]
    NotFound,

    #[error("filter database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Projection of the `filter` table, localised in SQL.
///
/// Two `LEFT JOIN`s against the same table give the two candidates a
/// [`Locale`] resolves to — `?1` the full tag (`pt_BR`), `?2` the bare
/// language (`pt`) — and `COALESCE` walks them down to the English `title`.
/// `NULLIF` is there because a present-but-empty translation should also fall
/// through rather than render as a blank row.
const FILTER_SELECT: &str = "
    SELECT f.filter_id,
           f.group_id,
           COALESCE(NULLIF(lp.name, ''), NULLIF(lb.name, ''), f.title) AS name,
           f.title,
           COALESCE(NULLIF(lp.description, ''), NULLIF(lb.description, ''), f.description, '') AS description,
           f.homepage,
           f.is_enabled,
           f.is_installed,
           f.is_trusted
    FROM filter f
    LEFT JOIN filter_localisation lp ON lp.filter_id = f.filter_id AND lp.lang = ?1
    LEFT JOIN filter_localisation lb ON lb.filter_id = f.filter_id AND lb.lang = ?2
";

/// A read-only handle to one filter catalogue.
pub struct Catalogue {
    conn: Connection,
}

impl Catalogue {
    /// Open a filter database read-only.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::NotFound);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        Ok(Self { conn })
    }

    /// Open whichever catalogue backs `set`, resolving its path.
    pub fn open_set(set: FilterSet) -> Result<Self, Error> {
        let path = set.db_path().ok_or(Error::NotFound)?;
        Self::open(&path)
    }

    /// Everything the Filters page needs, from one point in time.
    pub fn read(&self, locale: &Locale) -> Result<FilterCatalogue, Error> {
        Ok(FilterCatalogue {
            groups: self.groups(locale)?,
            filters: self.filters(locale)?,
            user_rules: self.user_rules(locale)?,
        })
    }

    /// Filter categories, localised, in the display order AdGuard itself uses.
    pub fn groups(&self, locale: &Locale) -> Result<Vec<FilterGroup>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT g.group_id,
                    COALESCE(NULLIF(lp.name, ''), NULLIF(lb.name, ''), g.name) AS name,
                    g.display_number
             FROM filter_group g
             LEFT JOIN filter_group_localisation lp
                    ON lp.group_id = g.group_id AND lp.lang = ?1
             LEFT JOIN filter_group_localisation lb
                    ON lb.group_id = g.group_id AND lb.lang = ?2
             ORDER BY g.display_number",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![locale.primary(), locale.fallback()],
            |row| {
                Ok(FilterGroup {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    display_number: row.get(2)?,
                })
            },
        )?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The subscribable catalogue, installed or not.
    ///
    /// Excludes the user-rules pseudo-filter (see [`Filter::USER_RULES_ID`]),
    /// which is not a list and would otherwise appear as a filter belonging to
    /// a nonexistent group.
    pub fn filters(&self, locale: &Locale) -> Result<Vec<Filter>, Error> {
        let sql =
            format!("{FILTER_SELECT} WHERE f.filter_id != ?3 ORDER BY f.group_id, f.display_number");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params![locale.primary(), locale.fallback(), Filter::USER_RULES_ID],
            map_filter,
        )?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The user-rules pseudo-filter, if present — the on/off state of the
    /// user's own `user.txt` / `dns_user.txt` rules.
    pub fn user_rules(&self, locale: &Locale) -> Result<Option<Filter>, Error> {
        let sql = format!("{FILTER_SELECT} WHERE f.filter_id = ?3");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![
            locale.primary(),
            locale.fallback(),
            Filter::USER_RULES_ID
        ])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_filter(row)?)),
            None => Ok(None),
        }
    }

    /// Re-read one filter's flags.
    ///
    /// This is the verification half of act -> re-read -> reconcile: a CLI
    /// mutation reports success at exit 0 even when it changed nothing, so the
    /// UI confirms against the database instead — without paying for a full
    /// catalogue read per toggle.
    pub fn state(&self, filter_id: i64) -> Result<Option<FilterState>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT is_enabled, is_installed FROM filter WHERE filter_id = ?1")?;
        let mut rows = stmt.query([filter_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(FilterState {
                enabled: row.get::<_, i64>(0)? != 0,
                installed: row.get::<_, i64>(1)? != 0,
            })),
            None => Ok(None),
        }
    }
}

/// How many rules the user has written in `user.txt` / `dns_user.txt`.
///
/// The database says only whether user rules are *on*, not whether there are
/// any — so the "Your rules" row counts them itself.
///
/// Comment syntax differs between the two files and overlaps with real rules:
/// `!` starts a comment in both, `#` starts one in the hosts-style DNS file,
/// but in adblock syntax `##.banner`, `#?#`, `#$#` and `#%#` are cosmetic
/// *rules*. So a `#` line counts as a comment only when what follows is not
/// one of those markers.
///
/// `None` means the file could not be read, which is normal — it does not
/// exist until something writes to it.
pub fn user_rule_count(path: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.lines().filter(|line| is_rule(line.trim())).count())
}

fn is_rule(line: &str) -> bool {
    if line.is_empty() || line.starts_with('!') {
        return false;
    }
    match line.strip_prefix('#') {
        // `#` alone, or `# text` — a hosts-file comment.
        Some(rest) => rest.starts_with(['#', '?', '$', '%', '@']),
        None => true,
    }
}

/// Shared row mapping for [`FILTER_SELECT`]. Column order must match it.
fn map_filter(row: &rusqlite::Row<'_>) -> rusqlite::Result<Filter> {
    Ok(Filter {
        id: row.get(0)?,
        group_id: row.get(1)?,
        name: row.get(2)?,
        title: row.get(3)?,
        description: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        homepage: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        installed: row.get::<_, i64>(7)? != 0,
        trusted: row.get::<_, i64>(8)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::is_rule;

    #[test]
    fn blank_lines_and_bang_comments_are_not_rules() {
        for line in ["", "   ", "! comment", "!------"] {
            assert!(!is_rule(line.trim()), "{line:?} counted as a rule");
        }
    }

    /// `dns_user.txt` is hosts-style, where `#` introduces a comment.
    #[test]
    fn hash_comments_are_not_rules() {
        for line in ["#", "# my notes", "#no space"] {
            assert!(!is_rule(line), "{line:?} counted as a rule");
        }
    }

    /// ...but in adblock syntax a leading `#` is cosmetic-rule syntax, so
    /// dropping every `#` line would undercount `user.txt`.
    #[test]
    fn cosmetic_rules_are_rules() {
        for line in ["##.banner", "#?#div:has(> .ad)", "#$#body { margin: 0 }", "#%#//scriptlet('abort')"] {
            assert!(is_rule(line), "{line:?} not counted as a rule");
        }
    }

    #[test]
    fn ordinary_rules_are_rules() {
        for line in ["||example.org^", "@@||ads.example.org^", "example.com##.ad", "0.0.0.0 tracker.example"] {
            assert!(is_rule(line), "{line:?} not counted as a rule");
        }
    }
}
