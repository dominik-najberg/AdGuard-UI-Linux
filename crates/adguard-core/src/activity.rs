//! What the proxy has been doing, counted: the store behind the Activity page.
//!
//! The first item of [issue #21]. AdGuard writes one line per request to
//! `<data>/logs/access.log` and keeps about three and a half days of it across
//! ten ~10 MiB generations (contract §9). A dashboard that reaches back further
//! has to copy something out, and **what it copies is a browsing record**, so
//! the decision about what that is came first and was the owner's.
//!
//! # Counts, never requests
//!
//! Decided 30 September 2026: this keeps **counts and nothing else**. Per hour,
//! how many requests ended in each [`Action`] and how many bytes they moved.
//! Per day, how many went to each host name, were decided by each rule and came
//! from each program. There is no row per request, so there is nothing here
//! that says *when* a site was visited to finer than a day, or in what order.
//!
//! Three columns of the log are never stored at all:
//!
//! - **the URL's path and query.** A host is cut out of field 6 and the rest is
//!   dropped at parse time ([`host`]), so no page title, search term or
//!   identifier reaches the database;
//! - **the referrer**, field 7, which is a second URL;
//! - **the upstream address**, field 13.
//!
//! What survives is still personal, and the page says so. Host names are the
//! top-domains list the issue asks for, a rule's text routinely names the domain
//! it matched (contract §9), and the client column names every program on the
//! machine that used the network. The file is created `0600`, deleted rows are
//! overwritten rather than left in free pages ([`Store::open`]), and
//! [`Store::clear`] empties it.
//!
//! # Reading a log that rotates underneath the reader
//!
//! There is no push mechanism, so this reads the files, and AdGuard renames
//! them under it every ~10 MiB — `access.log` becomes `access.log.1`, and so on
//! down to `.9`. What survives a rename is the **inode**, so the position read
//! up to is kept as *which file* (inode, plus a hash of its first line in case
//! an inode is reused) and *how far into it*. A later read finds that file
//! under whatever name it now has, finishes it, and reads every newer
//! generation whole. Nothing holds a descriptor between reads.
//!
//! The position and the counts it produced are written in one transaction, so
//! a line is counted once or not at all. When the file read up to has rotated
//! out of the window entirely — the application was not running for longer
//! than AdGuard keeps — every generation is read and lines no later than the
//! last one counted are skipped, which is the only way back in that cannot
//! double-count.
//!
//! # Failing to silence
//!
//! The format is undocumented and can move with any `adguard-cli` release. As
//! in [`crate::access`], a line is read only when every column this depends on
//! looks the way it was measured to — sixteen fields before the rule, the `--`
//! marker where it belongs, a quoted client, an action from the known set, a
//! size ending in `b` and a duration in `ms` — and a line that does not is
//! **counted as unread** rather than guessed at. The page shows that count, so
//! a format that drifts empties the dashboard visibly instead of filling it
//! with the wrong column.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

/// How many days of counts are kept until the user chooses otherwise. Older
/// rows are dropped on every ingest.
///
/// Ninety because the counts are small: measured on the reference machine at
/// about 700 host rows and 25 rule rows a day, so a quarter of a year is a few
/// megabytes. The longest range the page offers is thirty days.
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

/// The retentions the page offers, shortest first.
///
/// A short list rather than a number field: each is a promise the page states
/// in words, and "counts older than 1 year are deleted" reads as a decision
/// where "older than 211 days" reads as a typo. Seven is the floor because a
/// shorter one would empty the week range the page opens beside; a year is the
/// ceiling because the file grows by a few megabytes a quarter and nothing on
/// the page reaches back further than thirty days.
pub const RETENTION_CHOICES: [i64; 4] = [7, 30, DEFAULT_RETENTION_DAYS, 365];

/// The database, beside the window state under `$XDG_STATE_HOME/adguard-ui`.
///
/// State rather than data, in the basedir spec's own terms: it lists "actions
/// history" under `$XDG_STATE_HOME`, and nothing here is worth syncing between
/// machines.
const FILE: &str = "activity.sqlite";

/// Bumped when the tables change shape. A database from a newer build is
/// refused rather than written into ([`Error::Newer`]).
///
/// 2 adds `settings`, which holds the retention. A version-1 file is upgraded
/// in place by creating it; nothing already in the file changes.
const SCHEMA: i64 = 2;

/// The `settings` key the retention is stored under, in days.
const RETENTION_KEY: &str = "retention_days";

/// How many rotated generations to look for. Ten are measured (contract §9);
/// the margin costs a failed `open` each.
const GENERATIONS: usize = 32;

/// Fields before the rule text, which is the only column that may itself hold
/// spaces (contract §9).
const FIELDS: usize = 16;

/// The marker that closes the fixed columns, field 16.
const MARKER: &str = "--";

/// The key [`Tally`] files a line under when it could not be read.
const UNREAD: &str = "unread";

/// How long a top list is.
const TOP: usize = 10;

/// How many bytes of a generation's first line identify it.
const HEAD: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("activity database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("could not create {}: {source}", .path.display())]
    Directory { path: PathBuf, source: std::io::Error },

    #[error("the activity database was written by a newer AdGuard UI (schema {0})")]
    Newer(i64),

    #[error("no home directory to keep the activity database in")]
    NoHome,

    #[error("{0} days is not a retention this version offers")]
    Retention(i64),
}

/// What became of a request, from field 10.
///
/// `MODIFIED_CONTENT` and `MODIFIED_META` are one value here. The first is a
/// page body rewritten, the second headers or cookies, and a dashboard that
/// told them apart would be asking the user a question AdGuard's own apps do
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// `NONE`: inspected, and nothing matched.
    Passed,
    /// `BLOCKED`.
    Blocked,
    /// `MODIFIED_CONTENT` or `MODIFIED_META`.
    Modified,
    /// `WHITELISTED`: an exception rule let it through.
    Allowed,
    /// `-`, which half the QUIC lines carry (contract §9). What it means is not
    /// measured, so it is counted apart rather than folded into [`Self::Passed`].
    Uninspected,
}

impl Action {
    pub const ALL: [Self; 5] = [
        Self::Passed,
        Self::Blocked,
        Self::Modified,
        Self::Allowed,
        Self::Uninspected,
    ];

    fn parse(field: &str) -> Option<Self> {
        match field {
            "NONE" => Some(Self::Passed),
            "BLOCKED" => Some(Self::Blocked),
            "MODIFIED_CONTENT" | "MODIFIED_META" => Some(Self::Modified),
            "WHITELISTED" => Some(Self::Allowed),
            "-" => Some(Self::Uninspected),
            // A value AdGuard added after 1.4.13. Not a request of any known
            // kind, so the whole line goes unread — see the module header.
            _ => None,
        }
    }

    /// The spelling in the database. Ours, not AdGuard's, so a rename upstream
    /// does not strand the rows already written.
    fn key(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Blocked => "blocked",
            Self::Modified => "modified",
            Self::Allowed => "allowed",
            Self::Uninspected => "uninspected",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.key() == key)
    }
}

/// Requests by [`Action`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub passed: u64,
    pub blocked: u64,
    pub modified: u64,
    pub allowed: u64,
    pub uninspected: u64,
}

impl Counts {
    pub fn total(&self) -> u64 {
        self.passed + self.blocked + self.modified + self.allowed + self.uninspected
    }

    pub fn get(&self, action: Action) -> u64 {
        match action {
            Action::Passed => self.passed,
            Action::Blocked => self.blocked,
            Action::Modified => self.modified,
            Action::Allowed => self.allowed,
            Action::Uninspected => self.uninspected,
        }
    }

    fn add(&mut self, action: Action, requests: u64) {
        let slot = match action {
            Action::Passed => &mut self.passed,
            Action::Blocked => &mut self.blocked,
            Action::Modified => &mut self.modified,
            Action::Allowed => &mut self.allowed,
            Action::Uninspected => &mut self.uninspected,
        };
        *slot += requests;
    }
}

/// The ranges the page offers, in whole local days ending today.
///
/// Days rather than a sliding 24 hours because the per-host, per-rule and
/// per-program counts are kept per day. A "last 24 hours" top list would have
/// to take in all of yesterday and call it part of the last day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Today,
    Week,
    Month,
}

impl Span {
    pub fn days(self) -> i64 {
        match self {
            Self::Today => 1,
            Self::Week => 7,
            Self::Month => 30,
        }
    }
}

/// One bar on the chart: an hour for [`Span::Today`], a day otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// Epoch seconds, local time's hour or midnight.
    pub start: i64,
    pub counts: Counts,
}

/// A name and how many requests it accounts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub name: String,
    pub requests: u64,
}

/// A program and what became of its requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// As AdGuard names it, without the quotes: `chrome`, `slack`, and
    /// `internal_proxy_client` for AdGuard's own.
    pub name: String,
    pub requests: u64,
    pub blocked: u64,
}

/// A rule that decided requests, and the list it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The filter list's id in `agflm_standard.db`, from `ID=<n>`.
    pub filter: i64,
    pub text: String,
    pub action: Action,
    pub requests: u64,
}

/// Everything the page shows for one [`Span`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub span: Span,
    /// Local midnight at the start of the span, epoch seconds.
    pub since: i64,
    pub totals: Counts,
    /// Bytes the requests in [`Self::totals`] moved, from field 14.
    pub bytes: u64,
    /// Lines in the span that were not understood — see the module header.
    pub unread: u64,
    pub buckets: Vec<Bucket>,
    pub blocked_hosts: Vec<Ranked>,
    pub hosts: Vec<Ranked>,
    pub rules: Vec<Rule>,
    pub clients: Vec<Client>,
    /// The earliest hour anything was counted in, kept or not pruned yet.
    /// `None` when nothing has been.
    pub recorded_since: Option<i64>,
    /// How many days of counts are kept — [`Store::retention`].
    pub retention_days: i64,
}

/// What one [`Store::ingest`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ingested {
    /// Requests counted.
    pub lines: u64,
    /// Lines that were not understood.
    pub unread: u64,
    /// The file last read up to was no longer in the window, so everything
    /// AdGuard still keeps was read and what had been counted already skipped.
    /// Anything that rotated out in between is gone.
    pub resumed: bool,
}

/// The activity database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Where the database lives on this machine, or `None` without a home.
    pub fn path() -> Option<PathBuf> {
        Some(crate::window_state::state_home()?.join(crate::window_state::SUBDIR).join(FILE))
    }

    /// Open the database at [`Self::path`], creating it if needed.
    pub fn locate() -> Result<Self, Error> {
        Self::open(&Self::path().ok_or(Error::NoHome)?)
    }

    /// Open, or create, the database at `path`.
    ///
    /// A new directory is made `0700` and the file `0600`: what is in here is a
    /// summary of the user's browsing, and nobody else on the machine needs it.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(dir) = path.parent() {
            if !dir.is_dir() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|source| Error::Directory {
                        path: dir.to_path_buf(),
                        source,
                    })?;
            }
        }
        let conn = Connection::open(path)?;
        // Best effort: a filesystem that refuses the mode still gets a working
        // database, and the directory above it is already private.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        // Two sessions of this application — the window and a stray second
        // launch — may ingest at once. The second waits for the first rather
        // than failing.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        // A cleared or pruned row is overwritten on disk, not left readable in
        // a free page until SQLite happens to reuse it.
        conn.pragma_update(None, "secure_delete", true)?;

        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA {
            return Err(Error::Newer(version));
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS hourly (
                 hour INTEGER NOT NULL, action TEXT NOT NULL,
                 requests INTEGER NOT NULL, bytes INTEGER NOT NULL,
                 PRIMARY KEY (hour, action)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS hosts (
                 day INTEGER NOT NULL, host TEXT NOT NULL, action TEXT NOT NULL,
                 requests INTEGER NOT NULL,
                 PRIMARY KEY (day, host, action)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS rules (
                 day INTEGER NOT NULL, filter INTEGER NOT NULL, rule TEXT NOT NULL,
                 action TEXT NOT NULL, requests INTEGER NOT NULL,
                 PRIMARY KEY (day, filter, rule, action)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS clients (
                 day INTEGER NOT NULL, client TEXT NOT NULL, action TEXT NOT NULL,
                 requests INTEGER NOT NULL,
                 PRIMARY KEY (day, client, action)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS cursor (
                 id INTEGER PRIMARY KEY CHECK (id = 0),
                 inode INTEGER NOT NULL, head INTEGER NOT NULL,
                 offset INTEGER NOT NULL, last INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA)?;
        Ok(Self { conn })
    }

    /// Count whatever AdGuard has logged since the last call.
    ///
    /// `live` is `access.log` itself; its rotated generations are found beside
    /// it. A log that is not there is nothing to count, not an error.
    pub fn ingest(&mut self, live: &Path) -> Result<Ingested, Error> {
        self.ingest_at(live, now())
    }

    fn ingest_at(&mut self, live: &Path, now: i64) -> Result<Ingested, Error> {
        // Immediate, so a second ingest waits here and then reads the position
        // this one leaves, rather than both reading from the same place.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor = Cursor::load(&tx)?;
        let generations = generations(live);

        let found = cursor.and_then(|cursor| {
            generations.iter().position(|generation| {
                generation.inode == cursor.inode && generation.head == Some(cursor.head)
            })
        });
        // The resume path: nothing to seek to, so skip what was counted before.
        let skip_through = match found {
            Some(_) => None,
            None => cursor.map(|cursor| cursor.last),
        };

        let mut tally = Tally::new(now, skip_through);
        let mut next = cursor;
        for (index, generation) in generations.iter().enumerate().skip(found.unwrap_or(0)) {
            let from = match (found, cursor) {
                // Shorter than the position means rewritten in place, and a
                // rewritten file is read from the start.
                (Some(at), Some(cursor)) if at == index && cursor.offset <= generation.len => {
                    cursor.offset
                }
                _ => 0,
            };
            let Some(bytes) = read_from(&generation.file, from) else {
                // Stop at the first file that cannot be read, so the position
                // stays in front of it rather than skipping past.
                break;
            };
            // Only the live file can still be growing, so only its trailing
            // fragment waits for the rest of its line. A rotated generation is
            // finished, and a fragment at its end is all there will ever be.
            let newest = index + 1 == generations.len();
            let consumed = if newest {
                bytes.iter().rposition(|&byte| byte == b'\n').map_or(0, |at| at + 1)
            } else {
                bytes.len()
            };
            if consumed == 0 {
                continue;
            }
            let bytes = &bytes[..consumed];
            for line in bytes.split(|&byte| byte == b'\n') {
                tally.line(line);
            }

            let head = if from == 0 {
                fingerprint(bytes)
            } else {
                cursor.map(|cursor| cursor.head)
            };
            if let Some(head) = head {
                next = Some(Cursor {
                    inode: generation.inode,
                    head,
                    offset: from + consumed as u64,
                    last: tally.last.or(next.map(|cursor| cursor.last)).unwrap_or(0),
                });
            }
        }

        tally.write(&tx)?;
        prune(&tx, now, retention(&tx)?)?;
        if let Some(next) = next {
            next.save(&tx)?;
        }
        tx.commit()?;
        Ok(Ingested {
            lines: tally.lines,
            unread: tally.unread,
            resumed: cursor.is_some() && found.is_none() && !generations.is_empty(),
        })
    }

    /// Delete every count. The position in the log is kept, so what was cleared
    /// is not read straight back in from the generations AdGuard still holds.
    pub fn clear(&mut self) -> Result<(), Error> {
        self.conn.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM hourly; DELETE FROM hosts; DELETE FROM rules; DELETE FROM clients;
             COMMIT;",
        )?;
        // `secure_delete` has already overwritten the rows; this gives the
        // pages back, so the file shrinks to what is left.
        self.conn.execute_batch("VACUUM;")?;
        Ok(())
    }

    /// How many whole local days of counts are kept: the user's choice, or
    /// [`DEFAULT_RETENTION_DAYS`] until they make one.
    pub fn retention(&self) -> Result<i64, Error> {
        retention(&self.conn)
    }

    /// Keep `days` of counts from now on, and drop what is already older.
    ///
    /// Only [`RETENTION_CHOICES`] are accepted, so a value typed into the
    /// database by hand or written by a later version cannot make this one
    /// keep nothing, or keep forever. The prune happens here rather than at the
    /// next ingest because the page has just told the user it has: a shortened
    /// retention that left the old counts until the next ten-minute read would
    /// be a promise kept late.
    pub fn set_retention(&mut self, days: i64) -> Result<(), Error> {
        self.set_retention_at(days, now())
    }

    fn set_retention_at(&mut self, days: i64, now: i64) -> Result<(), Error> {
        if !RETENTION_CHOICES.contains(&days) {
            return Err(Error::Retention(days));
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![RETENTION_KEY, days],
        )?;
        prune(&tx, now, days)?;
        tx.commit()?;
        Ok(())
    }

    /// The counts for `span`, ending today.
    pub fn summary(&self, span: Span) -> Result<Summary, Error> {
        self.summary_at(span, now())
    }

    fn summary_at(&self, span: Span, now: i64) -> Result<Summary, Error> {
        let since = midnight(now, span.days() - 1).unwrap_or(now - span.days() * 86_400);
        let starts: Vec<i64> = match span {
            Span::Today => {
                let end = midnight(now, -1).unwrap_or(since + 86_400);
                (since..end).step_by(3_600).collect()
            }
            _ => (0..span.days())
                .rev()
                .filter_map(|back| midnight(now, back))
                .collect(),
        };
        let mut buckets: Vec<Bucket> = starts
            .iter()
            .map(|&start| Bucket {
                start,
                counts: Counts::default(),
            })
            .collect();

        let mut totals = Counts::default();
        let mut bytes = 0;
        let mut unread = 0;
        let mut stmt = self
            .conn
            .prepare("SELECT hour, action, requests, bytes FROM hourly WHERE hour >= ?1")?;
        let mut rows = stmt.query(params![since])?;
        while let Some(row) = rows.next()? {
            let hour: i64 = row.get(0)?;
            let key: String = row.get(1)?;
            let requests = row.get::<_, i64>(2)?.max(0) as u64;
            let Some(action) = Action::from_key(&key) else {
                if key == UNREAD {
                    unread += requests;
                }
                continue;
            };
            totals.add(action, requests);
            bytes += row.get::<_, i64>(3)?.max(0) as u64;
            // The last bar that began at or before this hour.
            let slot = buckets.partition_point(|bucket| bucket.start <= hour);
            if let Some(bucket) = slot.checked_sub(1).and_then(|slot| buckets.get_mut(slot)) {
                bucket.counts.add(action, requests);
            }
        }

        let ranked = |sql: &str| -> Result<Vec<Ranked>, Error> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map(params![since, TOP as i64], |row| {
                Ok(Ranked {
                    name: row.get(0)?,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                })
            })?;
            rows.collect::<Result<_, _>>().map_err(Into::into)
        };
        let blocked_hosts = ranked(
            "SELECT host, SUM(requests) AS n FROM hosts
             WHERE day >= ?1 AND action = 'blocked'
             GROUP BY host ORDER BY n DESC, host LIMIT ?2",
        )?;
        let hosts = ranked(
            "SELECT host, SUM(requests) AS n FROM hosts
             WHERE day >= ?1
             GROUP BY host ORDER BY n DESC, host LIMIT ?2",
        )?;

        let mut stmt = self.conn.prepare(
            "SELECT filter, rule, action, SUM(requests) AS n FROM rules
             WHERE day >= ?1
             GROUP BY filter, rule, action ORDER BY n DESC, rule LIMIT ?2",
        )?;
        let rules = stmt
            .query_map(params![since, TOP as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .filter_map(|row| match row {
                Ok((filter, text, key, requests)) => Some(Ok(Rule {
                    filter,
                    text,
                    action: Action::from_key(&key)?,
                    requests: requests.max(0) as u64,
                })),
                Err(err) => Some(Err(err)),
            })
            .collect::<Result<_, _>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT client, SUM(requests) AS n,
                    SUM(CASE WHEN action = 'blocked' THEN requests ELSE 0 END)
             FROM clients WHERE day >= ?1
             GROUP BY client ORDER BY n DESC, client LIMIT ?2",
        )?;
        let clients = stmt
            .query_map(params![since, TOP as i64], |row| {
                Ok(Client {
                    name: row.get(0)?,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                    blocked: row.get::<_, i64>(2)?.max(0) as u64,
                })
            })?
            .collect::<Result<_, _>>()?;

        let recorded_since = self
            .conn
            .query_row("SELECT MIN(hour) FROM hourly", [], |row| row.get::<_, Option<i64>>(0))
            .optional()?
            .flatten();

        Ok(Summary {
            span,
            since,
            totals,
            bytes,
            unread,
            buckets,
            blocked_hosts,
            hosts,
            rules,
            clients,
            recorded_since,
            retention_days: self.retention()?,
        })
    }
}

/// How far the log has been read: which file, by inode and first line, and
/// the byte after the last line counted in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    inode: u64,
    head: i64,
    offset: u64,
    /// The last counted line's timestamp, in microseconds — what the resume
    /// path skips through.
    last: i64,
}

impl Cursor {
    fn load(tx: &Transaction) -> Result<Option<Self>, Error> {
        tx.query_row(
            "SELECT inode, head, offset, last FROM cursor WHERE id = 0",
            [],
            |row| {
                Ok(Self {
                    inode: row.get::<_, i64>(0)? as u64,
                    head: row.get(1)?,
                    offset: row.get::<_, i64>(2)?.max(0) as u64,
                    last: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    fn save(&self, tx: &Transaction) -> Result<(), Error> {
        tx.execute(
            "INSERT OR REPLACE INTO cursor (id, inode, head, offset, last)
             VALUES (0, ?1, ?2, ?3, ?4)",
            params![self.inode as i64, self.head, self.offset as i64, self.last],
        )?;
        Ok(())
    }
}

/// One log file, opened.
struct Generation {
    file: File,
    inode: u64,
    len: u64,
    /// [`fingerprint`] of its first line, or `None` while it has none.
    head: Option<i64>,
}

/// Every generation that exists, oldest first.
///
/// Opened **newest first**, which is what makes a rotation during the walk
/// harmless: a file renamed between two opens is met twice under two names and
/// kept once by inode, where opening oldest first could step past it.
fn generations(live: &Path) -> Vec<Generation> {
    let mut found: Vec<Generation> = Vec::new();
    for index in 0..GENERATIONS {
        let path = if index == 0 {
            live.to_path_buf()
        } else {
            let mut name = live.as_os_str().to_owned();
            name.push(format!(".{index}"));
            PathBuf::from(name)
        };
        let Ok(file) = File::open(&path) else { continue };
        let Ok(meta) = file.metadata() else { continue };
        // A fifo or a device would hang the read.
        if !meta.is_file() || found.iter().any(|generation| generation.inode == meta.ino()) {
            continue;
        }
        let mut head = vec![0; HEAD];
        let read = file.read_at(&mut head, 0).unwrap_or(0);
        head.truncate(read);
        found.push(Generation {
            head: fingerprint(&head),
            file,
            inode: meta.ino(),
            len: meta.len(),
        });
    }
    found.reverse();
    found
}

/// A file's contents from `from` onwards, or `None` if it cannot be read.
fn read_from(file: &File, from: u64) -> Option<Vec<u8>> {
    let mut file = file;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// FNV-1a over the first complete line — `None` when there is no complete line
/// yet. Hand-rolled because `std`'s hasher is not promised to be stable across
/// releases, and this value is stored.
fn fingerprint(bytes: &[u8]) -> Option<i64> {
    let end = bytes.iter().position(|&byte| byte == b'\n')?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in &bytes[..end] {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(hash as i64)
}

/// One request, as far as it is kept.
#[derive(Debug, PartialEq, Eq)]
struct Request<'a> {
    /// Microseconds since the epoch.
    at: i64,
    /// Local hour and local midnight it fell in, epoch seconds.
    hour: i64,
    day: i64,
    client: &'a str,
    host: Option<String>,
    action: Action,
    filter: Option<i64>,
    rule: Option<&'a str>,
    bytes: u64,
}

/// A line as a [`Request`], or `None` for anything not positively recognised.
///
/// The columns, in contract §9's numbering: 1 date, 2 time, 3 quoted client,
/// 4 protocol, 5 method, 6 URL or host, 7 referrer, 8 status, 9 request type,
/// 10 action, 11 a small count, 12 `ID=<n>` or `-`, 13 upstream, 14 size,
/// 15 duration, 16 `--`, then the rule when 12 names one.
fn parse<'a>(line: &'a str, clock: &mut Clock) -> Option<Request<'a>> {
    let mut fields = [""; FIELDS];
    let mut rest = line;
    for field in &mut fields {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let end = rest.find(|c: char| c.is_ascii_whitespace()).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let rule = rest.trim();

    if fields[15] != MARKER {
        return None;
    }
    let client = fields[2].strip_prefix('"')?.strip_suffix('"')?;
    let action = Action::parse(fields[9])?;
    // A size and a duration, each in its own unit. Neither is needed for
    // anything but the traffic figure; both are here because a column that has
    // shifted fails them.
    let bytes: u64 = fields[13].strip_suffix('b')?.parse().ok()?;
    fields[14].strip_suffix("ms")?.parse::<u64>().ok()?;
    // A rule exactly when there is an id, measured without exception across
    // 235,858 lines (contract §9).
    let (filter, rule) = match (fields[11], rule.is_empty()) {
        ("-", true) => (None, None),
        (id, false) => (Some(id.strip_prefix("ID=")?.parse().ok()?), Some(rule)),
        _ => return None,
    };
    let (at, hour, day) = clock.stamp(fields[0], fields[1])?;

    Some(Request {
        at,
        hour,
        day,
        client,
        host: host(fields[5]),
        action,
        filter,
        rule,
        bytes,
    })
}

/// The host name in field 6, and nothing else from it.
///
/// Field 6 is a full URL on HTTP lines and a bare name on TLS, QUIC and TCP
/// lines (contract §9). **This is where the path and query are dropped**, which
/// is the privacy decision in the module header: only what is returned here is
/// ever stored.
fn host(field: &str) -> Option<String> {
    if field == "-" {
        return None;
    }
    let host = match field.split_once("://") {
        Some((_, rest)) => {
            let authority = rest.split(['/', '?', '#']).next()?;
            let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            match authority.strip_prefix('[') {
                // An IPv6 literal, whose colons are not a port.
                Some(literal) => literal.split_once(']')?.0,
                None => match authority.rsplit_once(':') {
                    Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
                    _ => authority,
                },
            }
        }
        None => field,
    };
    let host = host.trim_end_matches('.');
    let valid = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':'));
    valid.then(|| host.to_ascii_lowercase())
}

/// Log timestamps as epoch values, one `mktime` per hour of log rather than
/// per line.
///
/// The log writes local wall clock with no offset. Converting through the
/// hour's own start keeps the C library's summer-time rules in charge, which
/// [`crate::access::epoch`] documents; the minutes and seconds inside an hour
/// are then plain arithmetic.
#[derive(Default)]
struct Clock {
    date: String,
    hour_of_day: u32,
    /// `(hour start, midnight)`, for [`Self::date`] and [`Self::hour_of_day`].
    cached: Option<(i64, i64)>,
}

impl Clock {
    /// `(microseconds, hour start, midnight)`.
    fn stamp(&mut self, date: &str, time: &str) -> Option<(i64, i64, i64)> {
        let (clock, fraction) = time.split_once('.')?;
        let mut parts = clock.split(':');
        let hour: u32 = parts.next()?.parse().ok()?;
        let minute: i64 = parts.next()?.parse().ok()?;
        let second: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        let digits = fraction.bytes().all(|byte| byte.is_ascii_digit());
        if fraction.is_empty() || fraction.len() > 6 || !digits {
            return None;
        }
        let micros: i64 = format!("{fraction:0<6}").parse().ok()?;

        let hit = self.cached.is_some() && self.hour_of_day == hour && self.date == date;
        if !hit {
            let start = crate::access::epoch(date, &format!("{hour:02}:00:00"))?;
            let midnight = crate::access::epoch(date, "00:00:00")?;
            self.date = date.to_owned();
            self.hour_of_day = hour;
            self.cached = Some((start, midnight));
        }
        let (start, midnight) = self.cached?;
        Some(((start + minute * 60 + second) * 1_000_000 + micros, start, midnight))
    }
}

/// Counts gathered from one ingest, before they are added to the tables.
struct Tally {
    clock: Clock,
    skip_through: Option<i64>,
    /// Where an unreadable line is filed: the hour of the last line that was
    /// readable, or the hour the ingest ran in if none has been yet.
    hour: i64,
    hourly: HashMap<(i64, &'static str), (u64, u64)>,
    hosts: HashMap<(i64, String, Action), u64>,
    rules: HashMap<(i64, i64, String, Action), u64>,
    clients: HashMap<(i64, String, Action), u64>,
    lines: u64,
    unread: u64,
    last: Option<i64>,
}

impl Tally {
    fn new(now: i64, skip_through: Option<i64>) -> Self {
        Self {
            clock: Clock::default(),
            skip_through,
            hour: now - now.rem_euclid(3_600),
            hourly: HashMap::new(),
            hosts: HashMap::new(),
            rules: HashMap::new(),
            clients: HashMap::new(),
            lines: 0,
            unread: 0,
            last: None,
        }
    }

    fn line(&mut self, bytes: &[u8]) {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let request = std::str::from_utf8(bytes)
            .ok()
            .and_then(|line| parse(line, &mut self.clock));
        let Some(request) = request else {
            self.unread += 1;
            self.hourly.entry((self.hour, UNREAD)).or_default().0 += 1;
            return;
        };
        self.hour = request.hour;
        if self.skip_through.is_some_and(|last| request.at <= last) {
            return;
        }
        self.lines += 1;
        self.last = Some(request.at);

        let slot = self.hourly.entry((request.hour, request.action.key())).or_default();
        slot.0 += 1;
        slot.1 += request.bytes;
        if let Some(host) = request.host {
            *self.hosts.entry((request.day, host, request.action)).or_default() += 1;
        }
        if let (Some(filter), Some(rule)) = (request.filter, request.rule) {
            *self
                .rules
                .entry((request.day, filter, rule.to_owned(), request.action))
                .or_default() += 1;
        }
        *self
            .clients
            .entry((request.day, request.client.to_owned(), request.action))
            .or_default() += 1;
    }

    fn write(&self, tx: &Transaction) -> Result<(), Error> {
        let mut stmt = tx.prepare(
            "INSERT INTO hourly (hour, action, requests, bytes) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (hour, action) DO UPDATE SET
                 requests = requests + excluded.requests, bytes = bytes + excluded.bytes",
        )?;
        for ((hour, action), (requests, bytes)) in &self.hourly {
            stmt.execute(params![hour, action, *requests as i64, *bytes as i64])?;
        }
        let mut stmt = tx.prepare(
            "INSERT INTO hosts (day, host, action, requests) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (day, host, action) DO UPDATE SET
                 requests = requests + excluded.requests",
        )?;
        for ((day, host, action), requests) in &self.hosts {
            stmt.execute(params![day, host, action.key(), *requests as i64])?;
        }
        let mut stmt = tx.prepare(
            "INSERT INTO rules (day, filter, rule, action, requests) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (day, filter, rule, action) DO UPDATE SET
                 requests = requests + excluded.requests",
        )?;
        for ((day, filter, rule, action), requests) in &self.rules {
            stmt.execute(params![day, filter, rule, action.key(), *requests as i64])?;
        }
        let mut stmt = tx.prepare(
            "INSERT INTO clients (day, client, action, requests) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (day, client, action) DO UPDATE SET
                 requests = requests + excluded.requests",
        )?;
        for ((day, client, action), requests) in &self.clients {
            stmt.execute(params![day, client, action.key(), *requests as i64])?;
        }
        Ok(())
    }
}

/// The stored retention, or the default. A stored value that is not one of
/// [`RETENTION_CHOICES`] reads as the default rather than being obeyed.
fn retention(conn: &Connection) -> Result<i64, Error> {
    let stored: Option<i64> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![RETENTION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored
        .filter(|days| RETENTION_CHOICES.contains(days))
        .unwrap_or(DEFAULT_RETENTION_DAYS))
}

/// Drop everything older than `days` whole local days.
fn prune(tx: &Transaction, now: i64, days: i64) -> Result<(), Error> {
    let Some(cutoff) = midnight(now, days - 1) else {
        return Ok(());
    };
    tx.execute("DELETE FROM hourly WHERE hour < ?1", params![cutoff])?;
    for table in ["hosts", "rules", "clients"] {
        tx.execute(&format!("DELETE FROM {table} WHERE day < ?1"), params![cutoff])?;
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// Local midnight `back` days before the day `now` falls in — a negative
/// `back` is days ahead. `mktime` does the calendar and the summer time.
fn midnight(now: i64, back: i64) -> Option<i64> {
    let time = now as libc::time_t;
    // SAFETY: a zeroed `tm` is valid, and `localtime_r` writes only into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_mday -= i32::try_from(back).ok()?;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a live, initialised local that `mktime` normalises.
    let seconds = unsafe { libc::mktime(&mut tm) };
    (seconds != -1).then_some(seconds as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with no rule, in the shape every 16-field line has. The hosts
    /// here and below are reserved example names, never ones from a real log:
    /// the real one is a browsing record.
    fn passed(clock: &str, client: &str, url: &str) -> String {
        format!(
            "25.08.2026 {clock}.123456 \"{client}\" HTTP2 GET {url} https://example.com/referring/page 200 xhr NONE 0 - 192.0.2.1:443 2649b 162ms --"
        )
    }

    /// A request a rule decided, which carries its id and its text.
    fn decided(clock: &str, url: &str, action: &str, id: i64, rule: &str) -> String {
        format!(
            "25.08.2026 {clock}.000001 \"firefox\" HTTP2 GET {url} - 0 script {action} 1 ID={id} 192.0.2.1:443 0b 0ms -- {rule}"
        )
    }

    /// The QUIC shape: a bare host and `-` for the action.
    fn quic(clock: &str) -> String {
        format!(
            "25.08.2026 {clock}.500000 \"firefox\" IQUIC - video.example.net - - any - 0 - 192.0.2.9:443 900b 3ms --"
        )
    }

    fn at(clock: &str) -> i64 {
        crate::access::epoch("25.08.2026", clock).expect("a valid time")
    }

    /// A scratch directory per test, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("adguard-ui-activity-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("logs")).expect("scratch dir");
            Self(dir)
        }

        fn live(&self) -> PathBuf {
            self.0.join("logs/access.log")
        }

        fn store(&self) -> Store {
            Store::open(&self.0.join("state/activity.sqlite")).expect("open the store")
        }

        fn write(&self, name: &str, lines: &[String]) {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            fs::write(self.0.join("logs").join(name), text).expect("write a log");
        }

        fn append(&self, text: &str) {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(self.live())
                .expect("open the live log");
            file.write_all(text.as_bytes()).expect("append");
        }

        /// What AdGuard does every ~10 MiB: shift every generation up one and
        /// start a new live file.
        fn rotate(&self) {
            let logs = self.0.join("logs");
            for index in (1..GENERATIONS).rev() {
                let from = logs.join(format!("access.log.{index}"));
                if from.exists() {
                    fs::rename(&from, logs.join(format!("access.log.{}", index + 1))).unwrap();
                }
            }
            fs::rename(logs.join("access.log"), logs.join("access.log.1")).unwrap();
            fs::write(logs.join("access.log"), "").unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Noon on the fixtures' day, so today's span covers them and retention
    /// does not touch them.
    fn noon() -> i64 {
        at("12:00:00")
    }

    fn total(store: &Store) -> u64 {
        store.summary_at(Span::Today, noon()).unwrap().totals.total()
    }

    #[test]
    fn reads_a_request_with_no_rule() {
        let line = passed("10:15:30", "chrome", "https://www.example.com:8443/a/b?q=secret");
        let request = parse(&line, &mut Clock::default()).expect("parses");
        assert_eq!(request.client, "chrome");
        assert_eq!(request.host.as_deref(), Some("www.example.com"));
        assert_eq!(request.action, Action::Passed);
        assert_eq!((request.filter, request.rule), (None, None));
        assert_eq!(request.bytes, 2649);
        assert_eq!(request.hour, at("10:00:00"));
        assert_eq!(request.day, at("00:00:00"));
        assert_eq!(request.at, at("10:15:30") * 1_000_000 + 123_456);
    }

    #[test]
    fn reads_a_rule_and_its_list() {
        let line = decided("10:00:00", "https://ads.example.net/x.js", "BLOCKED", 2, "||ads.example.net^");
        let request = parse(&line, &mut Clock::default()).expect("parses");
        assert_eq!(request.action, Action::Blocked);
        assert_eq!(request.filter, Some(2));
        assert_eq!(request.rule, Some("||ads.example.net^"));
    }

    /// The rule is the one column that may hold spaces, and it is the last.
    #[test]
    fn a_rule_with_spaces_is_read_whole() {
        let rule = "example.org#$#body { overflow: auto !important; }";
        let line = decided("10:00:00", "https://example.org/", "MODIFIED_CONTENT", 11, rule);
        let request = parse(&line, &mut Clock::default()).expect("parses");
        assert_eq!(request.rule, Some(rule));
        assert_eq!(request.action, Action::Modified);
    }

    #[test]
    fn reads_the_quic_shape() {
        let line = quic("10:00:00");
        let request = parse(&line, &mut Clock::default()).expect("parses");
        assert_eq!(request.action, Action::Uninspected);
        assert_eq!(request.host.as_deref(), Some("video.example.net"));
    }

    /// **The privacy decision, as a test.** Whatever the URL carries, the host
    /// is all that comes out.
    #[test]
    fn only_the_host_survives_a_url() {
        for (field, expected) in [
            ("https://user:pw@Example.COM/path?q=1#f", Some("example.com")),
            ("http://example.com:80", Some("example.com")),
            ("https://[2001:db8::1]:443/x", Some("2001:db8::1")),
            ("https://example.com./", Some("example.com")),
            ("example.net", Some("example.net")),
            ("-", None),
            ("https:///nothing", None),
            ("not a host", None),
        ] {
            assert_eq!(host(field).as_deref(), expected, "{field}");
        }
    }

    /// Every guard in [`parse`], each offered the line it exists to refuse.
    /// All of them must go unread, not be read as something else.
    #[test]
    fn a_line_that_has_drifted_is_not_read() {
        let good = passed("10:00:00", "chrome", "https://example.com/");
        assert!(parse(&good, &mut Clock::default()).is_some());
        let rule = decided("10:00:00", "https://example.com/", "BLOCKED", 2, "||example.com^");
        for line in [
            // A column lost, which moves the marker.
            good.replacen(" HTTP2", "", 1),
            // A column gained before the marker.
            good.replacen(" HTTP2", " HTTP2 extra", 1),
            // The client unquoted.
            good.replace("\"chrome\"", "chrome"),
            // An action this was not measured against.
            good.replace(" NONE ", " REDIRECTED "),
            // A size in another unit.
            good.replace(" 2649b ", " 2649 "),
            // A duration in another unit.
            good.replace(" 162ms ", " 0.162s "),
            // The date reordered.
            good.replace("25.08.2026", "2026-08-25"),
            // An id with no rule after it.
            rule.replace(" ||example.com^", ""),
            // A rule with no id.
            good.clone() + " ||example.com^",
            // An id that is not a number.
            rule.replace("ID=2", "ID=two"),
            String::new(),
        ] {
            assert!(parse(&line, &mut Clock::default()).is_none(), "{line:?}");
        }
    }

    #[test]
    fn the_first_ingest_reads_every_generation() {
        let scratch = Scratch::new("first");
        scratch.write("access.log.2", &[passed("08:00:00", "chrome", "https://a.example.com/")]);
        scratch.write("access.log.1", &[passed("09:00:00", "chrome", "https://b.example.com/")]);
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://c.example.com/")]);
        let mut store = scratch.store();
        let ingested = store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!(ingested, Ingested { lines: 3, unread: 0, resumed: false });
        assert_eq!(total(&store), 3);
    }

    #[test]
    fn a_second_ingest_reads_only_what_was_appended() {
        let scratch = Scratch::new("append");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!(store.ingest_at(&scratch.live(), noon()).unwrap().lines, 0);

        scratch.append(&format!("{}\n", passed("10:01:00", "chrome", "https://example.com/")));
        assert_eq!(store.ingest_at(&scratch.live(), noon()).unwrap().lines, 1);
        assert_eq!(total(&store), 2);
    }

    /// **The rotation, which is the reason for the cursor.** Lines on both
    /// sides of a roll are each counted exactly once.
    #[test]
    fn a_rotation_loses_nothing_and_counts_nothing_twice() {
        let scratch = Scratch::new("rotate");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();

        // More is written, the file rolls, and more is written to the new one,
        // all before the next read.
        scratch.append(&format!("{}\n", passed("10:01:00", "chrome", "https://example.com/")));
        scratch.rotate();
        scratch.append(&format!("{}\n", passed("10:02:00", "chrome", "https://example.com/")));

        let ingested = store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!((ingested.lines, ingested.resumed), (2, false));
        assert_eq!(total(&store), 3);
    }

    /// A line still being written is left for the next read, not counted as
    /// unread now and again as a request later.
    #[test]
    fn a_half_written_line_waits() {
        let scratch = Scratch::new("partial");
        scratch.write("access.log", &[]);
        let line = passed("10:00:00", "chrome", "https://example.com/");
        let (first, second) = line.split_at(40);
        scratch.append(first);
        let mut store = scratch.store();
        assert_eq!(store.ingest_at(&scratch.live(), noon()).unwrap(), Ingested::default());
        scratch.append(&format!("{second}\n"));
        assert_eq!(store.ingest_at(&scratch.live(), noon()).unwrap().lines, 1);
        assert_eq!(total(&store), 1);
    }

    /// The application was off for longer than AdGuard keeps. The file it had
    /// read up to is gone, so everything is read and what was counted skipped.
    #[test]
    fn resuming_after_the_window_moved_on_skips_what_was_counted() {
        let scratch = Scratch::new("resume");
        scratch.write("access.log", &[
            passed("09:00:00", "chrome", "https://example.com/"),
            passed("10:00:00", "chrome", "https://example.com/"),
        ]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();

        // Replaced wholesale: a new inode and a new first line, holding one line
        // already counted and one that is new.
        fs::remove_file(scratch.live()).unwrap();
        scratch.write("access.log", &[
            passed("10:00:00", "chrome", "https://example.com/"),
            passed("11:00:00", "chrome", "https://example.com/"),
        ]);
        let ingested = store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!((ingested.lines, ingested.resumed), (1, true));
        assert_eq!(total(&store), 3);
    }

    #[test]
    fn an_unreadable_line_is_counted_as_unread_and_nothing_else() {
        let scratch = Scratch::new("unread");
        scratch.write("access.log", &[
            passed("10:00:00", "chrome", "https://example.com/"),
            "something this parser has never seen".to_owned(),
        ]);
        let mut store = scratch.store();
        let ingested = store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!((ingested.lines, ingested.unread), (1, 1));
        let summary = store.summary_at(Span::Today, noon()).unwrap();
        assert_eq!((summary.totals.total(), summary.unread), (1, 1));
    }

    #[test]
    fn the_summary_adds_up() {
        let scratch = Scratch::new("summary");
        scratch.write("access.log", &[
            passed("09:10:00", "chrome", "https://example.com/a"),
            passed("09:20:00", "chrome", "https://example.com/b"),
            passed("10:00:00", "curl", "https://example.org/"),
            decided("10:05:00", "https://ads.example.net/1.js", "BLOCKED", 2, "||ads.example.net^"),
            decided("10:06:00", "https://ads.example.net/2.js", "BLOCKED", 2, "||ads.example.net^"),
            decided("10:07:00", "https://cdn.example.com/f", "WHITELISTED", 253, "@@||cdn.example.com^"),
            quic("11:00:00"),
        ]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        let summary = store.summary_at(Span::Today, noon()).unwrap();

        assert_eq!(summary.since, at("00:00:00"));
        assert_eq!(
            summary.totals,
            Counts { passed: 3, blocked: 2, modified: 0, allowed: 1, uninspected: 1 }
        );
        assert_eq!(summary.bytes, 3 * 2649 + 900);
        assert_eq!(summary.buckets.len(), 24);
        assert_eq!(summary.buckets[9].counts.total(), 2);
        assert_eq!(summary.buckets[10].counts.blocked, 2);
        assert_eq!(summary.buckets[11].counts.uninspected, 1);
        assert_eq!(
            summary.blocked_hosts,
            vec![Ranked { name: "ads.example.net".into(), requests: 2 }]
        );
        // A tie is broken by name, so the order is stable between readings.
        assert_eq!(summary.hosts[0], Ranked { name: "ads.example.net".into(), requests: 2 });
        assert_eq!(summary.hosts[1], Ranked { name: "example.com".into(), requests: 2 });
        assert_eq!(summary.rules[0].text, "||ads.example.net^");
        assert_eq!((summary.rules[0].filter, summary.rules[0].requests), (2, 2));
        assert_eq!(summary.clients[0].name, "firefox");
        assert_eq!((summary.clients[0].requests, summary.clients[0].blocked), (4, 2));
        assert_eq!(summary.recorded_since, Some(at("09:00:00")));

        // A week of days, today last, and today's counts in it.
        let week = store.summary_at(Span::Week, noon()).unwrap();
        assert_eq!(week.buckets.len(), 7);
        assert_eq!(week.buckets[6].start, at("00:00:00"));
        assert_eq!(week.buckets[6].counts.total(), 7);
    }

    /// **The other half of the privacy decision.** Neither the path, the query
    /// nor the referrer is anywhere in the file.
    #[test]
    fn no_path_query_or_referrer_reaches_the_database() {
        let scratch = Scratch::new("private");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/secret/page?token=hunter2")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        drop(store);

        let bytes = fs::read(scratch.0.join("state/activity.sqlite")).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for leaked in ["secret", "hunter2", "referring", "192.0.2.1"] {
            assert!(!text.contains(leaked), "{leaked} is in the database");
        }
        assert!(text.contains("example.com"));
    }

    #[test]
    fn the_database_is_private_to_its_owner() {
        let scratch = Scratch::new("mode");
        let store = scratch.store();
        drop(store);
        let file = fs::metadata(scratch.0.join("state/activity.sqlite")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
        let dir = fs::metadata(scratch.0.join("state")).unwrap();
        assert_eq!(dir.permissions().mode() & 0o777, 0o700);
    }

    /// Clearing empties the counts and does not read the same lines back in.
    #[test]
    fn clearing_forgets_and_stays_forgotten() {
        let scratch = Scratch::new("clear");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        store.clear().unwrap();
        assert_eq!(total(&store), 0);
        store.ingest_at(&scratch.live(), noon()).unwrap();
        assert_eq!(total(&store), 0);

        let bytes = fs::read(scratch.0.join("state/activity.sqlite")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("example.com"));
    }

    #[test]
    fn counts_older_than_the_retention_are_dropped() {
        let scratch = Scratch::new("retention");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        let later = noon() + (DEFAULT_RETENTION_DAYS + 1) * 86_400;
        store.ingest_at(&scratch.live(), later).unwrap();
        assert_eq!(total(&store), 0);
        assert_eq!(store.summary_at(Span::Today, later).unwrap().recorded_since, None);
    }

    #[test]
    fn the_retention_defaults_and_can_be_chosen() {
        let scratch = Scratch::new("retention-choice");
        let mut store = scratch.store();
        assert_eq!(store.retention().unwrap(), DEFAULT_RETENTION_DAYS);
        store.set_retention_at(365, noon()).unwrap();
        assert_eq!(store.retention().unwrap(), 365);
        // And it outlives the connection.
        drop(store);
        assert_eq!(scratch.store().retention().unwrap(), 365);
    }

    #[test]
    fn a_retention_not_on_offer_is_refused_and_not_obeyed() {
        let scratch = Scratch::new("retention-refused");
        let mut store = scratch.store();
        for days in [0, -1, 1, 8, 36_500] {
            assert!(matches!(store.set_retention_at(days, noon()), Err(Error::Retention(_))));
        }
        assert_eq!(store.retention().unwrap(), DEFAULT_RETENTION_DAYS);
        // One written behind its back reads as the default.
        store
            .conn
            .execute("INSERT OR REPLACE INTO settings VALUES ('retention_days', 0)", [])
            .unwrap();
        assert_eq!(store.retention().unwrap(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn shortening_the_retention_drops_older_counts_at_once() {
        let scratch = Scratch::new("retention-short");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        // Ten days on, the default keeps it...
        let later = noon() + 10 * 86_400;
        assert_eq!(total(&store), 1);
        // ...and a week does not, without waiting for the next ingest.
        store.set_retention_at(7, later).unwrap();
        assert_eq!(total(&store), 0);
    }

    #[test]
    fn a_longer_retention_keeps_what_the_default_would_drop() {
        let scratch = Scratch::new("retention-long");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = scratch.store();
        store.set_retention_at(365, noon()).unwrap();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        let later = noon() + (DEFAULT_RETENTION_DAYS + 1) * 86_400;
        store.ingest_at(&scratch.live(), later).unwrap();
        assert_eq!(total(&store), 1);
    }

    #[test]
    fn a_version_one_file_is_upgraded_in_place() {
        let scratch = Scratch::new("schema-one");
        let path = scratch.0.join("state/activity.sqlite");
        scratch.write("access.log", &[passed("10:00:00", "chrome", "https://example.com/")]);
        let mut store = Store::open(&path).unwrap();
        store.ingest_at(&scratch.live(), noon()).unwrap();
        // Back to what 1.7.0's first build wrote: no settings table, version 1.
        store.conn.execute_batch("DROP TABLE settings; PRAGMA user_version = 1;").unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(total(&store), 1);
        assert_eq!(store.retention().unwrap(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let scratch = Scratch::new("schema");
        let path = scratch.0.join("state/activity.sqlite");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA + 1)
            .unwrap();
        assert!(matches!(Store::open(&path), Err(Error::Newer(_))));
    }

    #[test]
    fn no_log_is_nothing_to_count() {
        let scratch = Scratch::new("absent");
        let mut store = scratch.store();
        assert_eq!(store.ingest_at(&scratch.live(), noon()).unwrap(), Ingested::default());
    }
}
