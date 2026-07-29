//! Read-only access to AdGuard's filter catalogue.
//!
//! `agflm_standard.db` (HTTP) and `agflm_dns.db` (DNS) are plain SQLite 3
//! databases in AdGuard's data dir. Their `filter` table carries id, group,
//! title, description, homepage and the `is_enabled` / `is_installed` /
//! `is_trusted` flags — strictly better data than `adguard-cli filters list`
//! prints, whose fixed-width title column overflows and collides with the
//! status field for long names (see `docs/cli-contract.md` section 6).
//!
//! These are the running daemon's live databases. Always open read-only;
//! every mutation goes through `adguard-cli filters ...`.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::model::{Filter, FilterGroup};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("filter database not found — has AdGuard CLI been run yet?")]
    NotFound,

    #[error("filter database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

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

    /// Filter categories, in the display order AdGuard itself uses.
    pub fn groups(&self) -> Result<Vec<FilterGroup>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT group_id, name, display_number FROM filter_group ORDER BY display_number",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FilterGroup {
                id: row.get(0)?,
                name: row.get(1)?,
                display_number: row.get(2)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The subscribable catalogue, installed or not.
    ///
    /// Excludes the user-rules pseudo-filter (see [`Filter::USER_RULES_ID`]),
    /// which is not a list and would otherwise appear as a filter belonging to
    /// a nonexistent group.
    pub fn filters(&self) -> Result<Vec<Filter>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT filter_id, group_id, title, description, homepage,
                    is_enabled, is_installed, is_trusted
             FROM filter
             WHERE filter_id != ?1
             ORDER BY group_id, display_number",
        )?;
        let rows = stmt.query_map([Filter::USER_RULES_ID], map_filter)?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The user-rules pseudo-filter, if present — the on/off state of the
    /// user's own `user.txt` / `dns_user.txt` rules.
    pub fn user_rules(&self) -> Result<Option<Filter>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT filter_id, group_id, title, description, homepage,
                    is_enabled, is_installed, is_trusted
             FROM filter
             WHERE filter_id = ?1",
        )?;
        let mut rows = stmt.query([Filter::USER_RULES_ID])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_filter(row)?)),
            None => Ok(None),
        }
    }

    /// Localised name for a filter, falling back to the English `title`.
    ///
    /// `filter_localisation` holds thousands of rows keyed by `lang`, so the
    /// UI should prefer these over `filter.title`.
    pub fn localised_name(&self, filter_id: i64, lang: &str) -> Result<Option<String>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM filter_localisation WHERE filter_id = ?1 AND lang = ?2")?;
        let mut rows = stmt.query(rusqlite::params![filter_id, lang])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }
}

/// Shared row mapping for the `filter` table. Column order must match the
/// SELECT lists above.
fn map_filter(row: &rusqlite::Row<'_>) -> rusqlite::Result<Filter> {
    Ok(Filter {
        id: row.get(0)?,
        group_id: row.get(1)?,
        title: row.get(2)?,
        description: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        homepage: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        installed: row.get::<_, i64>(6)? != 0,
        trusted: row.get::<_, i64>(7)? != 0,
    })
}
