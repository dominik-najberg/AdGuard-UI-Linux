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
//! # And where it does not, but browsers keep stores anyway
//!
//! The script's list is not every place a browser keeps a store, so since
//! the last item of [issue #21] this also reads the ones it misses, marked
//! [`Store::scanned`] `false` so the command and the wording can tell them
//! apart:
//!
//! - **Firefox's XDG layout**, `~/.config/mozilla/firefox`, where newer
//!   Firefox releases put new profiles instead of `~/.mozilla`.
//! - **Flatpak Firefox**, under `~/.var/app/org.mozilla.firefox`, in both of
//!   the layouts above.
//! - **Flatpak Chrome, Brave, Edge, Vivaldi and ungoogled Chromium**, each in
//!   `~/.var/app/<id>/.pki/nssdb`. Their Flathub manifests persist `.pki` into
//!   the app's own directory rather than sharing the user's home, read 30
//!   September 2026. Flatpak Chromium is not among them: its manifest grants
//!   the whole home directory, so it uses `~/.pki/nssdb` like a native one.
//!
//! None of these were measured on the reference machine, which has no Flatpak
//! and no XDG Firefox; the paths are the manifests' and Firefox's own. A
//! Firefox profile in a place the script does not scan is still reachable by
//! naming it with `-f`, so it stays inside the command. A Flatpak Chromium
//! store is not reachable by the script at all, and only [`add`] fixes it.
//!
//! Not covered, and said so rather than guessed at: distributions that make
//! NSS read the system store through p11-kit, where a store reported here as
//! missing may not matter.
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
//! Reading writes, spawns and escalates nothing. The databases are opened
//! read-only, and a browser that holds one open is not disturbed by it.
//!
//! # Writing: [`add`], the installer's own step and nothing more
//!
//! The one write is the line AdGuard's installer runs for each browser store,
//! `certutil -A -n <name> -t "TC,C,T" -i <pem> -d sql:<store>`, with the
//! `certutil` AdGuard ships beside it. It changes the user's own files, needs
//! no privilege, and is what the command on the Protection page would do to
//! the same store — so it can be a button where the system half, which needs
//! `sudo`, stays a command (`architecture.md` §6). NSS keeps these databases
//! consistent under concurrent writers, which is how the installer can run
//! while a browser is open. The store is read again afterwards, and [`add`]
//! reports success only when that reading says trusted: a `certutil` that exits
//! 0 is not taken as evidence, as no command's exit status is here.
//!
//! # Renewing: [`renew`], for a CA AdGuard regenerated
//!
//! AdGuard CLI generates a new CA whenever it has lost the old one's key —
//! measured four times in its own logs, each after a licence activation
//! (`cli::Error::Unlicensed`), though not after every one. Every store that
//! trusted the old CA then fails every filtered page, and still holds the old
//! certificate under the same nickname. That leftover is the evidence
//! [`Store::stale`] counts: someone, the installer or this application at the
//! user's request, put AdGuard's CA in this store before. So a store with a
//! stale copy is renewed without asking again — the old copies out, the
//! current one in with the installer's trust — while a store that never held
//! AdGuard's CA is still only ever changed by the *Add to Browsers* button.
//!
//! `certutil -D -n` deletes by nickname, one certificate a call, and which of
//! several same-named ones it takes is NSS's choice. So [`renew`] deletes them
//! all, current one included, and adds the current one back: the only order
//! that cannot leave a stale copy behind.
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

/// Certificates under the CA's nickname that are not the CA: earlier AdGuard
/// CAs. `CKA_LABEL` is `a3`, measured on the reference machine's stores, where
/// it holds the nickname as bytes — hence the cast, which makes a label stored
/// as text compare the same.
const STALE: &str = "
    SELECT count(*)
    FROM nssPublic
    WHERE a0 = ?2 AND CAST(a3 AS BLOB) = ?3 AND a11 != ?1
";

/// The certificate by its bytes, and its server-auth trust when a trust row
/// is there. One row per copy of the certificate; none when it is absent.
const SELECT: &str = "
    SELECT t.ace536358
    FROM nssPublic c
    LEFT JOIN nssPublic t ON t.a0 = ?3 AND t.a81 = c.a81 AND t.a82 = c.a82
    WHERE c.a0 = ?2 AND c.a11 = ?1
";

/// The Chromium stores: display name, the store directory relative to
/// `$HOME`, and whether the installer writes there. Its own two first, in its
/// order; then the Flatpak ones — see the module docs.
const CHROMIUM: [(&str, &str, bool); 7] = [
    ("Chromium-based browsers", ".pki/nssdb", true),
    ("Chromium (snap)", "snap/chromium/current/.pki/nssdb", true),
    ("Chrome (Flatpak)", ".var/app/com.google.Chrome/.pki/nssdb", false),
    ("Brave (Flatpak)", ".var/app/com.brave.Browser/.pki/nssdb", false),
    ("Edge (Flatpak)", ".var/app/com.microsoft.Edge/.pki/nssdb", false),
    ("Vivaldi (Flatpak)", ".var/app/com.vivaldi.Vivaldi/.pki/nssdb", false),
    (
        "Ungoogled Chromium (Flatpak)",
        ".var/app/io.github.ungoogled_software.ungoogled_chromium/.pki/nssdb",
        false,
    ),
];

/// Firefox's profile root outside a snap, relative to `$HOME`. The snap roots
/// are found by [`firefox_roots`], as the installer's glob finds them.
const FIREFOX: &str = ".mozilla/firefox";

/// Firefox's roots the installer does not scan, relative to `$HOME`, with the
/// name each is shown under.
const FIREFOX_UNSCANNED: [(&str, &str); 3] = [
    ("Firefox", ".config/mozilla/firefox"),
    ("Firefox (Flatpak)", ".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ("Firefox (Flatpak)", ".var/app/org.mozilla.firefox/config/mozilla/firefox"),
];

/// The trust the installer gives: a CA for websites, e-mail and code.
const INSTALLER_TRUST: &str = "TC,C,T";

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
    /// `Firefox`, `Firefox (snap)`, `Chromium-based browsers`,
    /// `Chromium (snap)`, or a `(Flatpak)` name from the module docs.
    pub browser: &'static str,
    /// In a place AdGuard's installer looks by itself. `false` for the XDG
    /// Firefox root and every Flatpak store.
    pub scanned: bool,
    /// The profile, for a Firefox store. The Chromium stores are one per
    /// user, whatever profiles the browser itself has.
    pub profile: Option<Profile>,
    /// The `cert9.db` that was read, so a row can name it.
    pub database: PathBuf,
    pub state: StoreState,
    /// Certificates under the CA's nickname that are not the current CA —
    /// AdGuard CAs from before a regeneration. Zero when none, or when the
    /// store could not be read. See the module docs on renewing.
    pub stale: usize,
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
    /// run passes over — any profile but the default one, and every profile
    /// under a root the installer does not scan. `None` for a Chromium store.
    pub fn profile_flag(&self) -> Option<&Path> {
        self.profile
            .as_ref()
            .filter(|profile| !(profile.default && self.scanned))
            .map(|profile| profile.dir.as_path())
    }

    /// Whether AdGuard's installer can reach this store at all, by itself or
    /// through `-f`. Only a Flatpak Chromium store is out of its reach.
    pub fn installer_reaches(&self) -> bool {
        self.scanned || self.profile.is_some()
    }

    /// The store directory, as `certutil -d sql:` takes it.
    pub fn dir(&self) -> &Path {
        self.database.parent().unwrap_or(&self.database)
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
        Some(Self::inspect_as(&crate::browser::home()?, &der, &nickname(certificate)))
    }

    /// The same check against an explicit `$HOME` and certificate.
    ///
    /// A parameter for the reason every check in this crate takes one: on the
    /// reference machine every store is trusted, so the states worth rendering
    /// are only reachable against stores built for the purpose — which a test
    /// may do in a temporary directory, and must never do to a real browser.
    pub fn inspect(home: &Path, der: &[u8]) -> Self {
        Self::inspect_as(home, der, crate::trust::DEFAULT_CERTIFICATE_NAME)
    }

    /// [`Self::inspect`], counting stale copies under `nickname`.
    pub fn inspect_as(home: &Path, der: &[u8], nickname: &str) -> Self {
        let mut stores = Vec::new();

        for (browser, root, scanned) in firefox_roots(home) {
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
                    scanned,
                    profile: Some(Profile {
                        name: listed.name,
                        number: index + 1,
                        default: listed.default,
                        dir,
                    }),
                    state: read_state(&database, der),
                    stale: stale(&database, der, nickname),
                    database,
                });
            }
        }

        for (browser, dir, scanned) in CHROMIUM {
            let database = home.join(dir).join(DATABASE);
            if database.is_file() {
                stores.push(Store {
                    browser,
                    scanned,
                    profile: None,
                    state: read_state(&database, der),
                    stale: stale(&database, der, nickname),
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

    /// Every store holding an earlier AdGuard CA — the ones [`renew`] may
    /// change without asking, because AdGuard's CA was put there before.
    /// Includes a store that already trusts the current CA beside the old
    /// one, which renewing only tidies.
    pub fn renewable(&self) -> Vec<&Store> {
        self.stores
            .iter()
            .filter(|store| store.stale > 0 && !matches!(store.state, StoreState::Unreadable(_)))
            .collect()
    }
}

/// Firefox's profile roots under `home`, with the name each is shown under
/// and whether the installer scans it: the ordinary one, then every snap whose
/// name starts `firefox`, sorted — which is what the installer's
/// `"$HOME"/snap/firefox*/common/.mozilla/firefox` expands to — then the ones
/// it does not know.
fn firefox_roots(home: &Path) -> Vec<(&'static str, PathBuf, bool)> {
    let mut roots = vec![("Firefox", home.join(FIREFOX), true)];

    let mut snaps: Vec<PathBuf> = fs::read_dir(home.join("snap"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("firefox"))
        .map(|entry| entry.path().join("common/.mozilla/firefox"))
        .collect();
    snaps.sort();
    roots.extend(snaps.into_iter().map(|root| ("Firefox (snap)", root, true)));
    roots.extend(FIREFOX_UNSCANNED.iter().map(|(name, dir)| (*name, home.join(dir), false)));

    roots.retain(|(_, root, _)| root.is_dir());
    roots
}

/// `certutil` for [`add`]: the copy AdGuard ships beside its CLI, which is the
/// version its installer was written against, or the system's when AdGuard's
/// is missing. `$ADGUARD_CERTUTIL` overrides both.
pub fn certutil() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ADGUARD_CERTUTIL") {
        return Some(PathBuf::from(explicit)).filter(|path| path.is_file());
    }
    let beside = crate::paths::cert_installer()
        .and_then(|installer| Some(installer.parent()?.join("certutil")))
        .filter(|path| path.is_file());
    beside.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("certutil"))
            .find(|path| path.is_file())
    })
}

/// Add AdGuard's CA to one store, trusted as the installer trusts it, and
/// check that it took.
///
/// `certificate` is the `.pem` [`crate::CaTrust::certificate`] names. Its file
/// name without the extension is the nickname, as the installer derives it.
/// Stdin is closed: `certutil` asks for a password only for a store that has
/// one, and a prompt nobody can answer is a failure to report, not a hang.
pub fn add(store: &Store, certutil: &Path, certificate: &Path) -> Result<(), String> {
    let der = crate::trust::der(certificate)
        .ok_or_else(|| format!("{} holds no certificate", certificate.display()))?;
    let nickname = nickname(certificate);
    let target = target(store);

    let output = std::process::Command::new(certutil)
        .arg("-A")
        .arg("-n")
        .arg(&nickname)
        .arg("-t")
        .arg(INSTALLER_TRUST)
        .arg("-i")
        .arg(certificate)
        .arg("-d")
        .arg(&target)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("could not run {}: {err}", certutil.display()))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        return Err(if said.is_empty() {
            format!("certutil failed ({})", output.status)
        } else {
            said.lines().last().unwrap_or(said).to_owned()
        });
    }
    match read_state(&store.database, &der) {
        StoreState::Trusted => Ok(()),
        other => Err(format!(
            "certutil reported success, but the store still reads as {other:?}"
        )),
    }
}

/// Replace every certificate under the CA's nickname in `store` with the
/// current CA, trusted as the installer trusts it, and check that it took.
///
/// Returns how many stale copies went. See the module docs for why the current
/// one is deleted too, and why a store with stale copies is renewed unasked.
pub fn renew(store: &Store, certutil: &Path, certificate: &Path) -> Result<usize, String> {
    let der = crate::trust::der(certificate)
        .ok_or_else(|| format!("{} holds no certificate", certificate.display()))?;
    let nickname = nickname(certificate);
    let target = target(store);

    // Bounded: a `certutil` that reports success and deletes nothing must not
    // spin. Each pass removes one; the reference machine had four.
    for _ in 0..RENEW_LIMIT {
        let deleted = std::process::Command::new(certutil)
            .arg("-D")
            .arg("-n")
            .arg(&nickname)
            .arg("-d")
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|err| format!("could not run {}: {err}", certutil.display()))?;
        if !deleted.success() {
            break;
        }
    }
    let left = stale(&store.database, &der, &nickname);
    if left > 0 {
        return Err(format!("{left} earlier AdGuard certificates are still in the store"));
    }
    add(store, certutil, certificate)?;
    Ok(store.stale)
}

/// The most deletions [`renew`] attempts in one store.
const RENEW_LIMIT: usize = 64;

/// The nickname the installer gives the CA: the certificate's file name
/// without its extension.
fn nickname(certificate: &Path) -> String {
    certificate
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::trust::DEFAULT_CERTIFICATE_NAME.to_owned())
}

/// The store as `certutil -d` takes it.
fn target(store: &Store) -> std::ffi::OsString {
    let mut target = std::ffi::OsString::from("sql:");
    target.push(store.dir());
    target
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

/// How many certificates under `nickname` in `database` are not `der`. Zero
/// when the store cannot be read: an unreadable store is never changed.
fn stale(database: &Path, der: &[u8], nickname: &str) -> usize {
    let count = || -> rusqlite::Result<i64> {
        let conn = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(200))?;
        conn.query_row(
            STALE,
            rusqlite::params![der, &CERTIFICATE[..], nickname.as_bytes()],
            |row| row.get(0),
        )
    };
    count().map_or(0, |n| usize::try_from(n).unwrap_or(0))
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

    /// The older CA beside the current one is counted as stale, and the store
    /// is renewable; a store with the current CA alone is not.
    #[test]
    fn an_older_certificate_of_the_same_name_is_stale() {
        let home = Sandbox::new("stale");
        store(
            &home.path().join(".pki/nssdb"),
            &[Entry::Trusted(OLD), Entry::Trusted(OURS)],
        );
        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores[0].stale, 1);
        assert_eq!(check.renewable().len(), 1);

        let home = Sandbox::new("fresh");
        store(&home.path().join(".pki/nssdb"), &[Entry::Trusted(OURS)]);
        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores[0].stale, 0);
        assert!(check.renewable().is_empty());
    }

    /// The case renewing exists for: the old CA trusted, the current one absent.
    /// And a store that never held AdGuard's CA is not renewable — only the
    /// button adds to it.
    #[test]
    fn a_regenerated_ca_is_renewable_and_a_never_trusted_store_is_not() {
        let home = Sandbox::new("regenerated");
        store(&home.path().join(".pki/nssdb"), &[Entry::Trusted(OLD)]);
        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.stores[0].state, StoreState::Missing);
        assert_eq!(check.renewable().len(), 1);

        let home = Sandbox::new("never");
        store(&home.path().join(".pki/nssdb"), &[]);
        let check = BrowserStores::inspect(home.path(), OURS);
        assert_eq!(check.unmet().len(), 1);
        assert!(check.renewable().is_empty());
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

    /// The stores the installer does not know: a Flatpak Chrome store it cannot
    /// reach at all, and Flatpak and XDG Firefox profiles it reaches only when
    /// named — even the default one.
    #[test]
    fn flatpak_and_xdg_stores_are_found_and_marked_unscanned() {
        let home = Sandbox::new("flatpak");
        store(&home.path().join(".var/app/com.google.Chrome/.pki/nssdb"), &[]);
        let root = home.path().join(".var/app/org.mozilla.firefox/.mozilla/firefox");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("profiles.ini"),
            "[Profile0]\nName=default\nIsRelative=1\nPath=f.default\nDefault=1\n",
        )
        .unwrap();
        store(&root.join("f.default"), &[]);
        let xdg = home.path().join(".config/mozilla/firefox");
        fs::create_dir_all(&xdg).unwrap();
        fs::write(
            xdg.join("profiles.ini"),
            "[Profile0]\nName=new\nIsRelative=1\nPath=n.default\nDefault=1\n",
        )
        .unwrap();
        store(&xdg.join("n.default"), &[Entry::Trusted(OURS)]);

        let check = BrowserStores::inspect(home.path(), OURS);
        let names: Vec<String> = check.stores.iter().map(Store::name).collect();
        assert_eq!(
            names,
            [
                "Firefox profile “new”",
                "Firefox (Flatpak) profile “default”",
                "Chrome (Flatpak)",
            ]
        );
        assert!(check.stores.iter().all(|store| !store.scanned));

        let flatpak_firefox = &check.stores[1];
        assert_eq!(flatpak_firefox.profile_flag(), Some(root.join("f.default").as_path()));
        assert!(flatpak_firefox.installer_reaches());

        let chrome = &check.stores[2];
        assert_eq!(chrome.profile_flag(), None);
        assert!(!chrome.installer_reaches());
        assert_eq!(check.unmet().len(), 2);
    }

    /// [`add`] with the real `certutil`, against a store made for the purpose
    /// in a temporary directory — never a browser's. Uses this machine's CA,
    /// which is public; skips when either is missing.
    #[test]
    fn add_makes_a_store_trust_the_certificate() {
        let (Some(certutil), Some(certificate)) = (
            certutil(),
            crate::paths::certificate(crate::trust::DEFAULT_CERTIFICATE_NAME)
                .filter(|path| path.is_file()),
        ) else {
            eprintln!("skipping: no certutil or no certificate on this machine");
            return;
        };
        let home = Sandbox::new("add");
        let dir = home.path().join(".var/app/com.brave.Browser/.pki/nssdb");
        fs::create_dir_all(&dir).unwrap();
        let made = std::process::Command::new(&certutil)
            .args(["-N", "--empty-password", "-d"])
            .arg(format!("sql:{}", dir.display()))
            .status()
            .unwrap();
        assert!(made.success());

        let der = crate::trust::der(&certificate).unwrap();
        let before = BrowserStores::inspect(home.path(), &der);
        assert_eq!(before.unmet().len(), 1, "{before:?}");
        assert_eq!(before.stores[0].browser, "Brave (Flatpak)");

        add(&before.stores[0], &certutil, &certificate).expect("add");
        let after = BrowserStores::inspect(home.path(), &der);
        assert_eq!(after.stores[0].state, StoreState::Trusted);

        // And a second time changes nothing and still succeeds.
        add(&after.stores[0], &certutil, &certificate).expect("add again");
    }

    /// [`renew`] with the real `certutil`: a store holding a different
    /// certificate under the CA's nickname — made here with `certutil -S`,
    /// standing in for a CA from before a regeneration — ends up with the
    /// current CA alone, trusted. Skips as [`add`]'s test does.
    #[test]
    fn renew_replaces_an_earlier_ca_with_the_current_one() {
        let (Some(certutil), Some(certificate)) = (
            certutil(),
            crate::paths::certificate(crate::trust::DEFAULT_CERTIFICATE_NAME)
                .filter(|path| path.is_file()),
        ) else {
            eprintln!("skipping: no certutil or no certificate on this machine");
            return;
        };
        let home = Sandbox::new("renew");
        let dir = home.path().join(".pki/nssdb");
        fs::create_dir_all(&dir).unwrap();
        let target = format!("sql:{}", dir.display());
        let made = std::process::Command::new(&certutil)
            .args(["-N", "--empty-password", "-d", &target])
            .status()
            .unwrap();
        assert!(made.success());
        let noise = home.path().join("noise");
        fs::write(&noise, [7u8; 64]).unwrap();
        let old = std::process::Command::new(&certutil)
            .args(["-S", "-x", "-n", &nickname(&certificate), "-s", "CN=Earlier AdGuard CA"])
            .args(["-t", INSTALLER_TRUST, "-k", "rsa", "-g", "2048", "-z"])
            .arg(&noise)
            .args(["-d", &target])
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(old.success());

        let der = crate::trust::der(&certificate).unwrap();
        let before = BrowserStores::inspect(home.path(), &der);
        assert_eq!(before.stores[0].state, StoreState::Missing, "{before:?}");
        assert_eq!(before.renewable().len(), 1, "{before:?}");

        assert_eq!(renew(&before.stores[0], &certutil, &certificate), Ok(1));
        let after = BrowserStores::inspect(home.path(), &der);
        assert_eq!(after.stores[0].state, StoreState::Trusted);
        assert_eq!(after.stores[0].stale, 0);
        assert!(after.renewable().is_empty());
    }

    /// A `certutil` that exits 0 and does nothing is not believed.
    #[test]
    fn add_checks_the_store_rather_than_the_exit_status() {
        let Some(certificate) = crate::paths::certificate(crate::trust::DEFAULT_CERTIFICATE_NAME)
            .filter(|path| path.is_file())
        else {
            eprintln!("skipping: no certificate on this machine");
            return;
        };
        let home = Sandbox::new("liar");
        store(&home.path().join(".pki/nssdb"), &[]);
        let der = crate::trust::der(&certificate).unwrap();
        let check = BrowserStores::inspect(home.path(), &der);
        let err = add(&check.stores[0], Path::new("/bin/true"), &certificate).unwrap_err();
        assert!(err.contains("still reads as Missing"), "{err}");
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
