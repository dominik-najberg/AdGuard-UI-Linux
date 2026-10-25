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
           COALESCE(f.download_url, '') AS download_url,
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

/// One installed list in [`Catalogue::installed_rules_stamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesStamp {
    pub filter: i64,
    pub enabled: bool,
    /// `rules_list.text_hash`, which AdGuard keeps for every list it
    /// downloaded; `None` for the user-rules row.
    pub hash: Option<String>,
    pub count: i64,
    /// The rules switched off in the list, in full: they are few, and a
    /// length would miss one swapped for another as long.
    pub disabled: String,
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
    /// `display_number` does not order a group on its own — 44 filters share
    /// `(group 7, display_number 2)` in `agflm_standard.db`, and every custom
    /// filter has `display_number = 0` — so `filter_id` breaks the tie. Ascending
    /// is deliberate: it is the order SQLite already returned for the catalogue
    /// (`filter_id` is the rowid), so the shipped page does not move, and for the
    /// negatively-numbered custom filters it puts the most recently installed
    /// list at the top of its group.
    pub fn filters(&self, locale: &Locale) -> Result<Vec<Filter>, Error> {
        let sql = format!(
            "{FILTER_SELECT} WHERE f.filter_id != ?3 \
             ORDER BY f.group_id, f.display_number, f.filter_id"
        );
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

    /// When new filter data last actually arrived, as a Unix timestamp.
    ///
    /// The newest `last_download_time` across the lists that are switched on and
    /// installed — the moment AdGuard last brought something down, not the
    /// moment it last looked.
    ///
    /// # It is "changed", not "checked", and the difference is the whole point
    ///
    /// Measured (contract §14): the column **does not move on an up-to-date
    /// run**. Two consecutive `check-update`s that both answered `Up to date`
    /// left it identical to the second; the run before them, which answered
    /// `1 filter(s) updated`, moved it. So a long gap here means *nothing has
    /// changed upstream*, and emphatically not *nothing has checked* — AdGuard's
    /// daemon refreshes on its own every few hours, measured across days when
    /// nothing invoked the CLI at all.
    ///
    /// Anything rendering this has to say "changed" and not "checked", or it
    /// invites a user to press a button because a list nobody has revised in a
    /// week has not been revised in a week.
    ///
    /// **The newest and not the oldest.** They differ by a lot — on this machine
    /// the freshest enabled list was minutes old while the stalest was eight
    /// days — because lists are revised on their own schedules. The oldest would
    /// therefore report the least active list rather than the state of the
    /// catalogue, which is a fact about that list's author.
    ///
    /// `None` when no installed list carries one, which includes an empty
    /// catalogue. `last_update_time` is deliberately not offered beside it: it is
    /// always the earlier of the two on every row measured here, which is
    /// consistent with it being the list's own publication time rather than
    /// anything about this machine — an inference, and not one anything renders.
    pub fn last_downloaded(&self) -> Result<Option<i64>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT MAX(last_download_time) FROM filter
             WHERE is_enabled = 1 AND is_installed = 1 AND last_download_time > 0",
        )?;
        // `MAX` over no rows is one row holding NULL, so the outer Option is the
        // query's and the inner one is the value's.
        let newest: Option<i64> = stmt.query_row([], |row| row.get(0))?;
        Ok(newest)
    }

    /// The rules of every installed list, by filter id, whether it is switched
    /// on, and its text as AdGuard stores it — for [`crate::rules`] to match an
    /// address against the lists that are on, and to find which list a rule
    /// from the log is in, on or not.
    ///
    /// Rules the user disabled in a list (`disabled_rules_text`, one per line)
    /// are taken out. The user-rules pseudo-filter's row holds no text: its
    /// rules are `user.txt`, which the caller reads itself.
    pub fn installed_rules(&self) -> Result<Vec<(i64, bool, String)>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT f.filter_id, f.is_enabled, r.rules_text, r.disabled_rules_text
             FROM filter f JOIN rules_list r ON r.filter_id = f.filter_id
             WHERE f.is_installed = 1 AND f.filter_id != ?1
             ORDER BY f.filter_id",
        )?;
        let rows = stmt.query_map([Filter::USER_RULES_ID], |row| {
            let text: Option<String> = row.get(2)?;
            let disabled: Option<String> = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, text.unwrap_or_default(), disabled.unwrap_or_default()))
        })?;
        let mut lists = Vec::new();
        for row in rows {
            let (id, enabled, text, disabled) = row?;
            let disabled: std::collections::HashSet<&str> =
                disabled.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
            let text = if disabled.is_empty() {
                text
            } else {
                text.lines()
                    .filter(|line| !disabled.contains(line.trim()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            lists.push((id, enabled, text));
        }
        Ok(lists)
    }

    /// What [`Self::installed_rules`] would read, in a few bytes a list: which
    /// lists are installed and which are on, the user-rules pseudo-filter
    /// among them, and each one's `text_hash`, rule count and switched-off
    /// rules. It changes when that would, and reading it costs a millisecond
    /// rather than the tens of megabytes of text.
    ///
    /// The file's modification time would not do: the catalogue is in WAL
    /// mode on 1.5.0 — measured 24 October 2026, `PRAGMA journal_mode` answers
    /// `wal`, with `-wal` and `-shm` beside the file — so a write lands in
    /// `-wal` and reaches the file only at a checkpoint, and every reader, this
    /// one included, touches `-wal`.
    pub fn installed_rules_stamp(&self) -> Result<Vec<RulesStamp>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT f.filter_id, f.is_enabled, r.text_hash, IFNULL(r.rules_count, 0),
                    IFNULL(r.disabled_rules_text, '')
             FROM filter f LEFT JOIN rules_list r ON r.filter_id = f.filter_id
             WHERE f.is_installed = 1
             ORDER BY f.filter_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RulesStamp {
                filter: row.get(0)?,
                enabled: row.get(1)?,
                hash: row.get(2)?,
                count: row.get(3)?,
                disabled: row.get(4)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The lists the user installed by URL.
    ///
    /// A subset of [`filters`], read on its own because it is how an install is
    /// **verified**. `filters install` cannot be trusted to have worked from
    /// what it printed, and unlike every other mutation in this crate there is
    /// no id to re-read afterwards: the new row's id is assigned by AdGuard and
    /// only discoverable by looking. So the caller reads this before and after,
    /// and a row that was not there before is the evidence.
    ///
    /// Matching on the URL instead would be the obvious alternative and is
    /// wrong: a local path is stored back normalised to `file://…`, so the
    /// string the caller passed is not always the string that lands.
    ///
    /// [`filters`]: Self::filters
    pub fn custom_filters(&self, locale: &Locale) -> Result<Vec<Filter>, Error> {
        let sql = format!("{FILTER_SELECT} WHERE f.group_id = ?3 ORDER BY f.filter_id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params![locale.primary(), locale.fallback(), FilterGroup::CUSTOM_ID],
            map_filter,
        )?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// How many filters in this set are switched on.
    ///
    /// For the Status page's at-a-glance count, which wants one number and not
    /// the ~200-row localised catalogue [`read`] builds to get it.
    ///
    /// Counts the same population the Filters page renders switches for, so the
    /// number and the page can be checked against each other: the user-rules
    /// pseudo-filter is excluded, exactly as [`filters`] excludes it, because it
    /// is not a list and the page shows it in a group of its own.
    ///
    /// [`read`]: Self::read
    /// [`filters`]: Self::filters
    pub fn enabled_count(&self) -> Result<usize, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM filter WHERE is_enabled != 0 AND filter_id != ?1")?;
        let mut rows = stmt.query([Filter::USER_RULES_ID])?;
        match rows.next()? {
            Some(row) => Ok(row.get::<_, i64>(0)?.max(0) as usize),
            // `COUNT(*)` always returns a row; this arm exists so a driver that
            // somehow disagreed reads as "no count" rather than panicking.
            None => Ok(0),
        }
    }

    /// Re-read one filter's flags.
    ///
    /// This is the verification half of act -> re-read -> reconcile: a CLI
    /// mutation reports success at exit 0 even when it changed nothing, so the
    /// UI confirms against the database instead — without paying for a full
    /// catalogue read per toggle.
    ///
    /// All three flags every time, whichever control asked. `is_enabled` and
    /// `is_installed` already move together — `add` sets both — but **trust is
    /// on an axis of its own**: `set-trusted` leaves the other two alone, and a
    /// switch flip leaves `is_trusted` alone, measured both ways (contract §6).
    /// So a reconcile that read back only the flag it had just written would be
    /// *assuming* the rest had not moved, and they can: the daemon updates
    /// these filters on its own schedule, and a second window is one `filters`
    /// invocation away.
    pub fn state(&self, filter_id: i64) -> Result<Option<FilterState>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT is_enabled, is_installed, is_trusted FROM filter WHERE filter_id = ?1")?;
        let mut rows = stmt.query([filter_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(FilterState {
                enabled: row.get::<_, i64>(0)? != 0,
                installed: row.get::<_, i64>(1)? != 0,
                trusted: row.get::<_, i64>(2)? != 0,
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
    Some(rule_count(&text))
}

/// How many lines of `text` are rules rather than comments or blanks.
pub fn rule_count(text: &str) -> usize {
    text.lines().filter(|line| is_rule(line.trim())).count()
}

/// Write `text` over the user-rules file at `path` by renaming a sibling into
/// place, so AdGuard never reads half a file. Keeps the file's permissions.
///
/// Shared by the allowlist, which changes one line, and the rules editor,
/// which replaces the whole file with what the user typed.
pub fn write_user_rules(path: &Path, text: &str) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".adguard-ui");
    let temporary = std::path::PathBuf::from(temporary);
    std::fs::write(&temporary, text)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temporary, metadata.permissions())?;
    }
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
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
        download_url: row.get(6)?,
        enabled: row.get::<_, i64>(7)? != 0,
        installed: row.get::<_, i64>(8)? != 0,
        trusted: row.get::<_, i64>(9)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::{is_rule, Catalogue};

    /// The handle cannot write, whatever a later query tries: the open flags,
    /// asserted rather than taken on trust. `filters_live` checks the other
    /// half, that reading a real catalogue leaves its bytes alone.
    #[test]
    fn the_handle_refuses_writes() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-filters-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join("catalogue.db");
        rusqlite::Connection::open(&path)
            .and_then(|conn| conn.execute_batch("CREATE TABLE filter (filter_id INTEGER);"))
            .expect("create a database");

        let catalogue = Catalogue::open(&path).expect("should open read-only");
        assert!(catalogue.conn.is_readonly(rusqlite::MAIN_DB).expect("main is attached"));
        assert!(catalogue.conn.execute("INSERT INTO filter VALUES (1)", []).is_err());

        drop(catalogue);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stamp moves with every change [`Catalogue::installed_rules`] would
    /// see, and not otherwise.
    #[test]
    fn the_rules_stamp_follows_what_would_be_read() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-filters-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join("catalogue.db");
        let writer = rusqlite::Connection::open(&path).expect("create a database");
        writer
            .execute_batch(
                "CREATE TABLE filter (filter_id INTEGER PRIMARY KEY, is_enabled BOOLEAN, is_installed BOOLEAN,
                                      last_download_time INTEGER);
                 CREATE TABLE rules_list (filter_id INTEGER, rules_text TEXT, disabled_rules_text TEXT,
                                          rules_count INTEGER, text_hash TEXT);
                 INSERT INTO filter VALUES (-2147483648, 1, 1, 0), (2, 1, 1, 0), (3, 0, 1, 0), (4, 0, 0, 0);
                 INSERT INTO rules_list VALUES (-2147483648, NULL, NULL, 0, NULL),
                                               (2, '||a.example^', '', 1, 'aa'), (3, '||b.example^', '', 1, 'bb');",
            )
            .expect("fill it");
        let stamp = || Catalogue::open(&path).unwrap().installed_rules_stamp().unwrap();

        let first = stamp();
        assert_eq!(first.iter().map(|row| row.filter).collect::<Vec<_>>(), [-2147483648, 2, 3], "installed");
        writer.execute("UPDATE filter SET last_download_time = 1", []).unwrap();
        assert_eq!(stamp(), first, "a download that changed nothing");
        for change in [
            "UPDATE rules_list SET text_hash = 'ab' WHERE filter_id = 2",
            "UPDATE rules_list SET disabled_rules_text = '||a.example^' WHERE filter_id = 2",
            "UPDATE rules_list SET disabled_rules_text = '||b.example^' WHERE filter_id = 2",
            "UPDATE filter SET is_enabled = 1 WHERE filter_id = 3",
            "UPDATE filter SET is_installed = 1 WHERE filter_id = 4",
            "UPDATE filter SET is_enabled = 0 WHERE filter_id = -2147483648",
        ] {
            let before = stamp();
            writer.execute(change, []).unwrap();
            assert_ne!(stamp(), before, "{change}");
        }

        drop(writer);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
