//! Whether the certificate stores browsers keep for themselves have AdGuard's
//! CA, and trust it.
//!
//! [`crate::trust`] answers for the system store, and says in as many words
//! that a `true` there is never a statement about browsers: Firefox and the
//! Chromium family keep NSS databases of their own and read the system store
//! for nothing. A machine can therefore trust the CA and still fail every
//! filtered HTTPS page in the browser the user actually opens — and until this
//! module, the application had no way to tell that machine from a working one.
//! The third item of [issue #21].
//!
//! # Where it looks: where AdGuard's installer writes
//!
//! The locations are `install_cert.sh`'s, read out of the script beside the
//! CLI, for the reason [`crate::trust`]'s anchor list is: a check that looked
//! somewhere of its own would be answering a different question under the
//! same name, and the command it would offer could not fix what it reported.
//!
//! - **Firefox** — every profile listed in `profiles.ini` under
//!   `~/.mozilla/firefox` and under each `~/snap/firefox*/common/.mozilla/firefox`.
//!   The script itself only writes to the profile marked `Default=1`, and
//!   reaches any other only when it is named with `-f`; [`Store::profile_flag`]
//!   carries that distinction, because it changes the command.
//! - **Chromium and everything built on it** — `~/.pki/nssdb`, which Chrome,
//!   Chromium, Edge, Brave and Vivaldi on Linux all share, and the snap
//!   Chromium's own `~/snap/chromium/current/.pki/nssdb`.
//!
//! A store with no `cert9.db` is left out, as the script leaves it out: a
//! profile that has never been opened has nothing to report yet.
//!
//! Not covered, and said so rather than guessed at: Flatpak browsers, which
//! the script does not know about either, and distributions that make NSS read
//! the system store through p11-kit, where a store reported here as missing
//! may not matter.
//!
//! # What is read, measured
//!
//! `cert9.db` is SQLite, one table, `nssPublic`, one row per PKCS #11 object
//! and one column per attribute, named `a` and the attribute type in hex. On
//! the reference machine's two real stores and on a scratch one built with the
//! `certutil` AdGuard ships:
//!
//! - a certificate is a row whose `a0` (`CKA_CLASS`) is `00000001`, with the
//!   whole DER certificate in `a11` (`CKA_VALUE`);
//! - its trust is a **separate row**, class `CE534353` (`CKO_NSS_TRUST`),
//!   joined to the certificate by `a81` and `a82` — issuer and serial number;
//! - whether it may identify websites is that row's `ace536358`
//!   (`CKA_TRUST_SERVER_AUTH`), and only `CE534352`
//!   (`CKT_NSS_TRUSTED_DELEGATOR`) means yes. The installer's `-t "TC,C,T"`
//!   writes exactly that.
//!
//! **Presence is not trust, and the scratch store proves it.** Added with
//! `-t ",,"` the certificate is there and no trust row exists at all; marked
//! `p` in the SSL column the row says `CE53435A`, and `c` says `CE53435B`. All
//! three would pass a check that stopped at finding the certificate, and every
//! one of them fails a filtered page. Re-running the installer's `certutil -A`
//! over any of them writes `CE534352` back, so all three share one remedy.
//!
//! **And the name is worth nothing.** Both real stores on the reference
//! machine hold *two* certificates labelled `AdGuard CLI CA`, the current one
//! and one from before the CA was regenerated, each with full trust. A check
//! by nickname would call either store fine whichever one it found first. The
//! comparison is on the DER bytes, as [`crate::trust`]'s is on the PEM body.
//!
//! Nothing here writes, spawns or escalates. The databases are opened
//! read-only, and a browser that holds one open is not disturbed by it.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// The database file inside each store directory. The older `cert8.db` is
/// not read: Firefox left it in version 58, and the installer writes `sql:`.
const DATABASE: &str = "cert9.db";

/// `CKA_CLASS` of a certificate, `CKO_CERTIFICATE`, as NSS stores a `CK_ULONG`:
/// four bytes, big-endian.
const CERTIFICATE: [u8; 4] = 0x0000_0001_u32.to_be_bytes();

/// `CKA_CLASS` of a trust object, `CKO_NSS_TRUST`.
const TRUST: [u8; 4] = 0xCE53_4353_u32.to_be_bytes();

/// `CKT_NSS_TRUSTED_DELEGATOR` — a CA trusted to issue for websites.
const TRUSTED_DELEGATOR: [u8; 4] = 0xCE53_4352_u32.to_be_bytes();

/// The certificate by its bytes, and its server-auth trust when a trust row
/// is there. One row per copy of the certificate; none when it is absent.
const SELECT: &str = "
    SELECT t.ace536358
    FROM nssPublic c
    LEFT JOIN nssPublic t ON t.a0 = ?3 AND t.a81 = c.a81 AND t.a82 = c.a82
    WHERE c.a0 = ?2 AND c.a11 = ?1
";

/// The Chromium stores, in the installer's order: display name, and the store
/// directory relative to `$HOME`.
const CHROMIUM: [(&str, &str); 2] = [
    ("Chromium-based browsers", ".pki/nssdb"),
    ("Chromium (snap)", "snap/chromium/current/.pki/nssdb"),
];

/// Firefox's profile root outside a snap, relative to `$HOME`. The snap roots
/// are found by [`firefox_roots`], as the installer's glob finds them.
const FIREFOX: &str = ".mozilla/firefox";

/// What one store says about the CA.
///
/// Not a bool, for the reason [`crate::CaTrust`] is not one: "absent" and
/// "present but not trusted" read the same from a failing page, and only one
/// of them is explained by the installer never having run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreState {
    /// The certificate is there, trusted to identify websites.
    Trusted,
    /// The certificate is not in this store at all.
    Missing,
    /// It is there, but not trusted to identify websites: no trust row, or one
    /// that says anything but `CKT_NSS_TRUSTED_DELEGATOR`. See the module docs
    /// for the three shapes this has been measured to take.
    Untrusted,
    /// The database is there and could not be read. Never rendered as the
    /// reassuring answer, and never as a failure either.
    Unreadable(String),
}

/// A Firefox profile, as `profiles.ini` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The name the user sees in Firefox's profile manager.
    pub name: String,
    /// Its position in `profiles.ini`, from one — how a report that must not
    /// carry the name can still tell two profiles apart.
    pub number: usize,
    /// Marked `Default=1`, which is the one profile the installer reaches
    /// without being told.
    pub default: bool,
    /// The profile directory, resolved.
    pub dir: PathBuf,
}

/// One store found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// `Firefox`, `Firefox (snap)`, `Chromium-based browsers` or
    /// `Chromium (snap)`.
    pub browser: &'static str,
    /// The profile, for a Firefox store. The Chromium stores are one per
    /// user, whatever profiles the browser itself has.
    pub profile: Option<Profile>,
    /// The `cert9.db` that was read, so a row can name it.
    pub database: PathBuf,
    pub state: StoreState,
}

impl Store {
    /// The store as a sentence names it: `Firefox profile “default”`. No
    /// comma, because the names are joined into lists with them.
    pub fn name(&self) -> String {
        match &self.profile {
            Some(profile) => format!("{} profile “{}”", self.browser, profile.name),
            None => self.browser.to_string(),
        }
    }

    /// The store as a public report may name it: `Firefox profile 1 (default)`.
    ///
    /// A profile's name is whatever its user typed, often their own, and the
    /// diagnostics report promises to carry nothing personal by construction
    /// (`diagnostics.rs`). Its position is enough to tell two apart.
    pub fn anonymous_name(&self) -> String {
        match &self.profile {
            Some(profile) if profile.default => {
                format!("{} profile {} (default)", self.browser, profile.number)
            }
            Some(profile) => format!("{} profile {}", self.browser, profile.number),
            None => self.browser.to_string(),
        }
    }

    /// The directory to hand the installer's `-f`, for a profile its ordinary
    /// run passes over. `None` for every store that run already reaches.
    pub fn profile_flag(&self) -> Option<&Path> {
        self.profile
            .as_ref()
            .filter(|profile| !profile.default)
            .map(|profile| profile.dir.as_path())
    }
}

/// The check, across every store on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserStores {
    /// Firefox profiles first, then the Chromium stores — the installer's
    /// order. Empty on a machine with neither.
    pub stores: Vec<Store>,
}

impl BrowserStores {
    /// Read this machine's stores for the certificate at `certificate`.
    ///
    /// `None` when there is nothing to compare against — no certificate at
    /// that path, which [`crate::CaTrust`] is already reporting — or no
    /// `$HOME` to look under. `$ADGUARD_BROWSER_HOME` overrides `$HOME`, as it
    /// does for [`crate::BrowserIntegration`], and for the same reason.
    ///
    /// The certificate is read here a second time rather than handed over by
    /// [`crate::CaTrust`], which keeps PEM text and not bytes. A regeneration
    /// landing between the two reads would show for one repaint, and the next
    /// focus re-reads both.
    pub fn detect(certificate: &Path) -> Option<Self> {
        let der = crate::trust::der(certificate)?;
        Some(Self::inspect(&crate::browser::home()?, &der))
    }

    /// The same check against an explicit `$HOME` and certificate.
    ///
    /// A parameter for the reason every check in this crate takes one: on the
    /// reference machine every store is trusted, so the states worth rendering
    /// are only reachable against stores built for the purpose — which a test
    /// may do in a temporary directory, and must never do to a real browser.
    pub fn inspect(home: &Path, der: &[u8]) -> Self {
        let mut stores = Vec::new();

        for (browser, root) in firefox_roots(home) {
            let Ok(ini) = fs::read_to_string(root.join("profiles.ini")) else {
                continue;
            };
            for (index, listed) in profiles(&ini).into_iter().enumerate() {
                let dir = if listed.relative {
                    root.join(&listed.path)
                } else {
                    PathBuf::from(&listed.path)
                };
                let database = dir.join(DATABASE);
                if !database.is_file() {
                    continue;
                }
                stores.push(Store {
                    browser,
                    profile: Some(Profile {
                        name: listed.name,
                        number: index + 1,
                        default: listed.default,
                        dir,
                    }),
                    state: read_state(&database, der),
                    database,
                });
            }
        }

        for (browser, dir) in CHROMIUM {
            let database = home.join(dir).join(DATABASE);
            if database.is_file() {
                stores.push(Store {
                    browser,
                    profile: None,
                    state: read_state(&database, der),
                    database,
                });
            }
        }

        Self { stores }
    }

    /// Every store that has been read and does not trust the CA — the ones
    /// AdGuard's installer would change. An unreadable store is not among
    /// them: only positive evidence counts, as everywhere in this crate.
    pub fn unmet(&self) -> Vec<&Store> {
        self.stores
            .iter()
            .filter(|store| matches!(store.state, StoreState::Missing | StoreState::Untrusted))
            .collect()
    }
}

/// Firefox's profile roots under `home`, with the name each is shown under:
/// the ordinary one, then every snap whose name starts `firefox`, sorted —
/// which is what the installer's `"$HOME"/snap/firefox*/common/.mozilla/firefox`
/// expands to.
fn firefox_roots(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut roots = vec![("Firefox", home.join(FIREFOX))];

    let mut snaps: Vec<PathBuf> = fs::read_dir(home.join("snap"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("firefox"))
        .map(|entry| entry.path().join("common/.mozilla/firefox"))
        .collect();
    snaps.sort();
    roots.extend(snaps.into_iter().map(|root| ("Firefox (snap)", root)));

    roots.retain(|(_, root)| root.is_dir());
    roots
}

/// One `[ProfileN]` section of `profiles.ini`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Listed {
    name: String,
    path: String,
    relative: bool,
    default: bool,
}

/// The profiles in `profiles.ini`, in file order.
///
/// Read with the installer's rules, because which profile is "the default" is
/// the installer's decision and it is the one that matters for the command: a
/// header is `[Profile` and digits and nothing else, and inside one only
/// `Name=`, `Path=`, the exact line `IsRelative=0` and the exact line
/// `Default=1` count. Modern Firefox also writes `[Install…]` sections with a
/// `Default=` of their own; the installer ignores them, and so does this.
///
/// A section with no `Path=` is dropped, as the installer drops it.
fn profiles(ini: &str) -> Vec<Listed> {
    let mut found = Vec::new();
    let mut current: Option<Listed> = None;
    let mut inside = false;

    for line in ini.lines() {
        if is_profile_header(line) {
            found.extend(current.take());
            current = Some(Listed {
                relative: true,
                ..Listed::default()
            });
            inside = true;
        } else if line.starts_with('[') && line.ends_with(']') {
            inside = false;
        } else if let (true, Some(listed)) = (inside, current.as_mut()) {
            if let Some(name) = line.strip_prefix("Name=") {
                listed.name = name.to_string();
            } else if let Some(path) = line.strip_prefix("Path=") {
                listed.path = path.to_string();
            } else if line == "IsRelative=0" {
                listed.relative = false;
            } else if line == "Default=1" {
                listed.default = true;
            }
        }
    }
    found.extend(current);
    found.retain(|listed| !listed.path.is_empty());
    found
}

/// `[Profile0]`, `[Profile12]` — and not `[ProfileX]` or `[Install4F96…]`.
fn is_profile_header(line: &str) -> bool {
    line.strip_prefix("[Profile")
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Read one store.
fn read_state(database: &Path, der: &[u8]) -> StoreState {
    match trust_values(database, der) {
        Ok(values) if values.is_empty() => StoreState::Missing,
        // Any copy with full trust is enough: that is the one NSS will use.
        Ok(values) if values.iter().any(|value| value.as_deref() == Some(&TRUSTED_DELEGATOR[..])) => {
            StoreState::Trusted
        }
        Ok(_) => StoreState::Untrusted,
        Err(err) => StoreState::Unreadable(err.to_string()),
    }
}

/// The server-auth trust of every copy of the certificate in `database`.
///
/// Read-only, and with a short busy timeout: NSS writes to these files while
/// the browser runs — a certificate added in its settings — and a read that
/// arrives mid-write should wait the moment it takes rather than report the
/// store as unreadable. Not `immutable=1`, which would skip the locking and
/// could read that write half-done.
fn trust_values(database: &Path, der: &[u8]) -> rusqlite::Result<Vec<Option<Vec<u8>>>> {
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(200))?;
    let mut select = conn.prepare(SELECT)?;
    let rows = select.query_map(
        rusqlite::params![der, &CERTIFICATE[..], &TRUST[..]],
        |row| row.get::<_, Option<Vec<u8>>>(0),
    )?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two certificates that differ, as DER. The bytes need not parse as
    /// X.509: the store is only ever compared, never decoded.
    const OURS: &[u8] = b"\x30\x03ours";
    const OLD: &[u8] = b"\x30\x03old!";

    /// A throwaway `$HOME`, as `browser.rs` builds its own.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("adguard-ui-nss-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// How a certificate sits in a store: added with full trust, added with
    /// none, or added and then given some trust other than full.
    enum Entry<'a> {
        Trusted(&'a [u8]),
        Bare(&'a [u8]),
        TrustedAs(&'a [u8], u32),
    }

    /// A `cert9.db` with the columns this module reads, in the shape the
    /// reference machine's stores have them: one row per certificate, one per
    /// trust object, joined by issuer and serial. A real store has a hundred
    /// more columns, none of which the query names.
    fn store(dir: &Path, entries: &[Entry]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let database = dir.join(DATABASE);
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "CREATE TABLE nssPublic (id PRIMARY KEY UNIQUE ON CONFLICT ABORT, \
             a0, a3, a11, a81, a82, ace536358)",
        )
        .unwrap();
        for (serial, entry) in entries.iter().enumerate() {
            let (der, trust) = match entry {
                Entry::Trusted(der) => (*der, Some(TRUSTED_DELEGATOR.to_vec())),
                Entry::Bare(der) => (*der, None),
                Entry::TrustedAs(der, value) => (*der, Some(value.to_be_bytes().to_vec())),
            };
            let id = serial as i64 * 2;
            let serial = vec![2, 1, serial as u8];
            let issuer = b"issuer".to_vec();
            conn.execute(
                "INSERT INTO nssPublic (id, a0, a3, a11, a81, a82) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    id,
                    &CERTIFICATE[..],
                    "AdGuard CLI CA",
                    der,
                    issuer,
                    serial
                ],
            )
            .unwrap();
            if let Some(trust) = trust {
                conn.execute(
                    "INSERT INTO nssPublic (id, a0, a81, a82, ace536358) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        id + 1,
                        &TRUST[..],
                        issuer,
                        serial,
                        trust
                    ],
                )
                .unwrap();
            }
        }
        database
    }

    fn firefox(home: &Path, ini: &str) -> PathBuf {
        let root = home.join(FIREFOX);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("profiles.ini"), ini).unwrap();
        root
    }

    /// The installer's `-t "TC,C,T"`: there, and trusted.
    #[test]
    fn a_certificate_with_full_trust_is_trusted() {
        let home = Sandbox::new("trusted");
        store(&home.path().join(".pki/nssdb"), &[Entry::Trusted(OURS)]);

        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores.len(), 1, "{check:?}");
        assert_eq!(check.stores[0].browser, "Chromium-based browsers");
        assert_eq!(check.stores[0].state, StoreState::Trusted);
        assert!(check.unmet().is_empty());
    }

    /// The trap measured on the reference machine: an older AdGuard CA of the
    /// same name, fully trusted, and the current one absent. A check by name
    /// would pass this store.
    #[test]
    fn an_older_certificate_of_the_same_name_does_not_count() {
        let home = Sandbox::new("older");
        store(&home.path().join(".pki/nssdb"), &[Entry::Trusted(OLD)]);

        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores[0].state, StoreState::Missing);
        assert_eq!(check.unmet().len(), 1);
    }

    /// And the reference machine's actual shape: both, each trusted. The
    /// current one is found whichever row comes first.
    #[test]
    fn the_current_certificate_is_found_beside_an_older_one() {
        let home = Sandbox::new("both");
        store(
            &home.path().join(".pki/nssdb"),
            &[Entry::Trusted(OLD), Entry::Trusted(OURS)],
        );
        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores[0].state, StoreState::Trusted);
    }

    /// Present is not trusted. Each shape was produced with `certutil` on a
    /// scratch store: `-t ",,"` leaves no trust row, `p` writes
    /// `CKT_NSS_NOT_TRUSTED`, and `c` writes `CKT_NSS_VALID_DELEGATOR`.
    #[test]
    fn a_certificate_without_full_trust_is_untrusted() {
        for (name, entry) in [
            ("bare", Entry::Bare(OURS)),
            ("distrusted", Entry::TrustedAs(OURS, 0xCE53_435A)),
            ("valid-only", Entry::TrustedAs(OURS, 0xCE53_435B)),
        ] {
            let home = Sandbox::new(name);
            store(&home.path().join(".pki/nssdb"), &[entry]);
            let check = BrowserStores::inspect(home.path(), OURS);
            assert_eq!(check.stores[0].state, StoreState::Untrusted, "{name}");
            assert_eq!(check.unmet().len(), 1, "{name}");
        }
    }

    /// A file that is not a store is reported as unreadable, and is not
    /// counted as a store the installer should change.
    #[test]
    fn an_unreadable_store_is_neither_trusted_nor_unmet() {
        let home = Sandbox::new("unreadable");
        let dir = home.path().join(".pki/nssdb");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(DATABASE), b"not a database at all, not even close").unwrap();

        let check = BrowserStores::inspect(home.path(), OURS);
        assert!(matches!(check.stores[0].state, StoreState::Unreadable(_)), "{check:?}");
        assert!(check.unmet().is_empty());
    }

    /// No `cert9.db`, no store: the browser has not made one yet, and the
    /// installer skips it too.
    #[test]
    fn a_directory_without_a_database_is_not_a_store() {
        let home = Sandbox::new("empty");
        fs::create_dir_all(home.path().join(".pki/nssdb")).unwrap();
        let root = firefox(
            home.path(),
            "[Profile0]\nName=default\nIsRelative=1\nPath=abcd.default\nDefault=1\n",
        );
        fs::create_dir_all(root.join("abcd.default")).unwrap();

        assert!(BrowserStores::inspect(home.path(), OURS).stores.is_empty());
    }

    /// Every profile is read, the default one is marked, and only the others
    /// need the installer's `-f`.
    #[test]
    fn every_firefox_profile_is_read_and_only_the_default_needs_no_flag() {
        let home = Sandbox::new("profiles");
        let root = firefox(
            home.path(),
            "[Profile1]\nName=work\nIsRelative=1\nPath=wxyz.work\n\n\
             [Profile0]\nName=default\nIsRelative=1\nPath=abcd.default\nDefault=1\n\n\
             [General]\nStartWithLastProfile=1\nVersion=2\n",
        );
        store(&root.join("wxyz.work"), &[]);
        store(&root.join("abcd.default"), &[Entry::Trusted(OURS)]);

        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores.len(), 2, "{check:?}");

        let work = &check.stores[0];
        assert_eq!(work.name(), "Firefox profile “work”");
        assert_eq!(work.anonymous_name(), "Firefox profile 1");
        assert_eq!(work.state, StoreState::Missing);
        assert_eq!(work.profile_flag(), Some(root.join("wxyz.work").as_path()));

        let default = &check.stores[1];
        assert_eq!(default.anonymous_name(), "Firefox profile 2 (default)");
        assert_eq!(default.state, StoreState::Trusted);
        assert_eq!(default.profile_flag(), None);
    }

    /// The snap root is found the way the installer's glob finds it, and an
    /// absolute profile path is taken as given.
    #[test]
    fn snap_firefox_and_absolute_profiles_are_found() {
        let home = Sandbox::new("snap");
        let elsewhere = home.path().join("profiles/mine");
        let root = home.path().join("snap/firefox/common/.mozilla/firefox");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("profiles.ini"),
            format!(
                "[Profile0]\nName=mine\nIsRelative=0\nPath={}\nDefault=1\n",
                elsewhere.display()
            ),
        )
        .unwrap();
        store(&elsewhere, &[Entry::Trusted(OURS)]);
        store(&home.path().join("snap/chromium/current/.pki/nssdb"), &[]);

        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores.len(), 2, "{check:?}");
        assert_eq!(check.stores[0].name(), "Firefox (snap) profile “mine”");
        assert_eq!(check.stores[0].database, elsewhere.join(DATABASE));
        assert_eq!(check.stores[1].browser, "Chromium (snap)");
        assert_eq!(check.stores[1].state, StoreState::Missing);
    }

    /// The installer's reading of `profiles.ini`, including what it ignores:
    /// the `Default=` of an `[Install…]` section, a header that is not
    /// `[Profile` and digits, and a section with no path.
    #[test]
    fn profiles_ini_is_read_with_the_installers_rules() {
        let ini = "[Install4F96D1932A9F858E]\nDefault=abcd.default\nLocked=1\n\n\
                   [Profile0]\nName=default\nIsRelative=1\nPath=abcd.default\n\n\
                   [ProfileX]\nName=odd\nPath=odd\nDefault=1\n\n\
                   [Profile1]\nName=empty\n\n\
                   [Profile2]\nName=abs\nIsRelative=0\nPath=/p/abs\nDefault=1\n";
        let listed = profiles(ini);
        assert_eq!(
            listed,
            vec![
                Listed {
                    name: "default".into(),
                    path: "abcd.default".into(),
                    relative: true,
                    default: false,
                },
                Listed {
                    name: "abs".into(),
                    path: "/p/abs".into(),
                    relative: false,
                    default: true,
                },
            ]
        );
    }

    /// This machine's own stores, against its own certificate. Not an
    /// assertion about what they hold — that is the user's business — but
    /// that real `cert9.db` files are read without error, which is what a new
    /// NSS schema would break. Skips when there is no certificate.
    #[test]
    fn the_real_stores_are_readable() {
        let Some(certificate) = crate::paths::certificate(crate::trust::DEFAULT_CERTIFICATE_NAME)
        else {
            eprintln!("skipping: AdGuard's data directory could not be located");
            return;
        };
        let Some(check) = BrowserStores::detect(&certificate) else {
            eprintln!("skipping: no certificate at {}", certificate.display());
            return;
        };
        for store in &check.stores {
            eprintln!("{}: {:?}", store.anonymous_name(), store.state);
            assert!(
                !matches!(store.state, StoreState::Unreadable(_)),
                "{}: {:?}",
                store.database.display(),
                store.state
            );
        }
    }

    /// What the check costs. It runs on the GTK main loop with the certificate
    /// check, on every window focus, so it gets the same kind of bound: wide,
    /// and a hundred times what it guards against. `--nocapture` prints the
    /// real figure.
    #[test]
    fn the_check_is_cheap_enough_for_the_main_loop() {
        let Some(certificate) = crate::paths::certificate(crate::trust::DEFAULT_CERTIFICATE_NAME)
        else {
            eprintln!("skipping: AdGuard's data directory could not be located");
            return;
        };
        if BrowserStores::detect(&certificate).is_none() {
            eprintln!("skipping: no certificate");
            return;
        }
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let _ = BrowserStores::detect(&certificate);
        }
        let each = started.elapsed() / 10;
        eprintln!("BrowserStores::detect: {each:?} per call");
        assert!(each < Duration::from_millis(50), "{each:?}");
    }
}
