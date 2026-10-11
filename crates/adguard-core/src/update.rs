//! Installing a newer **AdGuard UI**: download, verify, install, in that order.
//!
//! [`crate::release`] says whether there is a newer release; this is what the
//! About page's *Update and restart* does about it. It is the second exception
//! to `architecture.md` §6's "no privileged operation of our own", and the
//! section now says so and why.
//!
//! # How this copy was installed decides everything
//!
//! A release is three files — a `.deb`, an `.rpm` and a tarball for `~/.local`
//! — and which one is this machine's is read off where the running binary
//! lives ([`Install::detect`]), never guessed from the distribution:
//!
//! - **`/usr/bin`, owned by a package** — the matching package, installed by
//!   `apt-get` or `dnf` under `pkexec`. One password prompt, polkit's own.
//! - **`<prefix>/bin` under `$HOME`** — the tarball, unpacked and its own
//!   `install.sh` run with that prefix. No password: every file is the user's.
//! - **anything else** — a `cargo run`, a copy put somewhere by hand. Nothing
//!   is installed, because there is no telling what replacing it would break.
//!
//! # Nothing is installed that the release key did not sign
//!
//! Every file a release carries has a minisign signature beside it, made in the
//! release workflow by a key whose public half is compiled in from
//! `data/release-signing.pub`. The download is held in memory and checked
//! against it before a byte is written to disk, and the signature's *trusted
//! comment* — which the signature also covers — must name the file that was
//! asked for, so a validly signed file from a different release or for a
//! different machine is refused as firmly as a forged one.
//!
//! `SHA256SUMS` would not do this. It is published in the same release as the
//! files it describes, so anyone able to replace a package could replace its
//! checksum in the same breath. A release that carries no signature at all —
//! every release before this module existed — is [`Error::Unsigned`], and the
//! page falls back to naming it.
//!
//! # What is not defended against
//!
//! The verified file is written to `~/.cache/adguard-ui/updates`, a directory
//! only this user can enter, and handed by path to a root `apt-get`. A process
//! already running as this user could swap it in between. Such a process can
//! already wait for the user to type a password into anything, so this widens
//! nothing — but it is the reason the file is written into a private directory
//! rather than `/tmp`, and why it is written only after it has been verified.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::release::{self, Release};

/// The public half of the key the release workflow signs with.
///
/// A file rather than a string in this module so the workflow, the README and
/// this crate all point at one thing. Rotating it means a release built with
/// the new key, which every older copy will refuse — so the last release under
/// the old key has to be the one that ships the new one.
const PUBLIC_KEY: &str = include_str!("../../../data/release-signing.pub");

/// The binary's own name, in every one of the three layouts.
const BINARY: &str = "adguard-ui";

/// A package is a few megabytes. The cap is what stops a redirected or hostile
/// endpoint from being read into memory indefinitely.
const PACKAGE_LIMIT: u64 = 128 * 1024 * 1024;

/// A minisign signature is four short lines.
const SIGNATURE_LIMIT: u64 = 16 * 1024;

/// Long enough for a slow line to fetch a few megabytes. A download that takes
/// longer than this is better restarted than waited on.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// How long `apt-get` waits for another package manager — usually
/// `unattended-upgrades` — to let go of the lock, rather than failing at once.
const APT_LOCK_WAIT: &str = "DPkg::Lock::Timeout=120";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "this copy of AdGuard UI was not installed from a release package, so it cannot \
         update itself"
    )]
    Unmanaged,

    #[error("release {0} has no package for this computer")]
    NoPackage(String),

    #[error("could not download {name}: {reason}")]
    Download { name: String, reason: String },

    /// The release carries no signature for this file. Every release made before
    /// the workflow signed them, and a release whose signing step was skipped.
    #[error("{0} is not signed, so it will not be installed automatically")]
    Unsigned(String),

    #[error("{name} failed its signature check and was not installed: {reason}")]
    Forged { name: String, reason: String },

    #[error("could not save the download: {0}")]
    Io(#[from] io::Error),

    /// The polkit dialog was dismissed. Not a failure, and not reported as one.
    #[error("the update was cancelled")]
    Cancelled,

    #[error("{command} failed: {detail}")]
    Failed { command: String, detail: String },
}

/// How the running copy of AdGuard UI got where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// `/usr/bin/adguard-ui`, owned by the `adguard-ui` Debian package.
    Deb { exe: PathBuf },
    /// `/usr/bin/adguard-ui`, owned by the `adguard-ui` RPM package.
    Rpm { exe: PathBuf },
    /// `<prefix>/bin/adguard-ui` under the home directory: the tarball's
    /// `install.sh`, which defaults the prefix to `~/.local`.
    Local { prefix: PathBuf, exe: PathBuf },
    /// Anywhere else. Updating it is not this application's to attempt.
    Unmanaged { exe: PathBuf },
}

impl Install {
    /// Look at the running binary and decide.
    ///
    /// Call this once, at start-up. After `apt-get` has replaced the binary,
    /// `/proc/self/exe` reads `/usr/bin/adguard-ui (deleted)` — handled, but a
    /// reading taken before anything moved is the one to trust.
    pub fn detect() -> Self {
        let exe = running_exe();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        classify(exe, home.as_deref(), &owner_of, &on_path)
    }

    /// The binary to start again once an update has gone in.
    pub fn exe(&self) -> &Path {
        match self {
            Self::Deb { exe } | Self::Rpm { exe } | Self::Local { exe, .. } | Self::Unmanaged { exe } => {
                exe
            }
        }
    }

    /// Whether [`apply`] has anything it could do here.
    pub fn can_update(&self) -> bool {
        !matches!(self, Self::Unmanaged { .. })
    }

    /// Whether installing needs the user's password. Said on the button before
    /// it is pressed, so the polkit dialog is not a surprise.
    pub fn needs_password(&self) -> bool {
        matches!(self, Self::Deb { .. } | Self::Rpm { .. })
    }

    /// The file of `release` this install wants, spelled as the packaging
    /// scripts spell it.
    ///
    /// `None` for an unmanaged copy, for a processor no release is built for,
    /// and for a tag that is not a plain version — the last because the name is
    /// about to become part of a URL and a path, and [`Release::version`] is
    /// what guarantees it holds nothing but digits and dots.
    pub fn asset(&self, release: &Release) -> Option<String> {
        let version = release.version()?;
        asset_name(self, &version, std::env::consts::ARCH)
    }
}

/// The three spellings, apart from the machine they are usually asked about.
fn asset_name(install: &Install, version: &str, arch: &str) -> Option<String> {
    match install {
        // `dpkg --print-architecture`, which is not `uname -m`.
        Install::Deb { .. } => {
            let arch = match arch {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                _ => return None,
            };
            Some(format!("{BINARY}_{version}_{arch}.deb"))
        }
        // `rpm --eval %{_arch}` and `uname -m` agree on both of these.
        Install::Rpm { .. } => Some(format!("{BINARY}-{version}-1.{arch}.rpm")),
        Install::Local { .. } => Some(format!("{BINARY}-{version}-{arch}.tar.gz")),
        Install::Unmanaged { .. } => None,
    }
}

/// A package manager that owns a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Dpkg,
    Rpm,
}

/// The rule, apart from the machine.
///
/// `owner` asks the package databases who owns a file and `present` whether a
/// command is installed; both are parameters so every branch is testable
/// without a `.deb` installed.
fn classify(
    exe: PathBuf,
    home: Option<&Path>,
    owner: &dyn Fn(&Path) -> Option<Manager>,
    present: &dyn Fn(&str) -> bool,
) -> Install {
    if exe.file_name().is_none_or(|name| name != BINARY) {
        return Install::Unmanaged { exe };
    }

    if exe == Path::new("/usr/bin").join(BINARY) {
        // Owned, *and* the tool that installs a local file is there. A `.deb`
        // on a machine without apt has nothing to hand the next one to.
        return match owner(&exe) {
            Some(Manager::Dpkg) if present("apt-get") => Install::Deb { exe },
            Some(Manager::Rpm) if present("dnf") => Install::Rpm { exe },
            _ => Install::Unmanaged { exe },
        };
    }

    // `<prefix>/bin/adguard-ui`, with the prefix inside the home directory.
    // `target/debug/adguard-ui` fails the `bin` test, which is the point: a
    // working copy must never be "updated" by a release.
    let prefix = exe
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent);
    match (prefix, home) {
        (Some(prefix), Some(home)) if prefix.starts_with(home) && prefix != home => Install::Local {
            prefix: prefix.to_owned(),
            exe,
        },
        _ => Install::Unmanaged { exe },
    }
}

/// `/proc/self/exe`, without the ` (deleted)` the kernel appends once the file
/// behind it has been replaced.
fn running_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    match exe.to_str().and_then(|exe| exe.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => exe,
    }
}

/// Which package database, if either, owns `path` as part of `adguard-ui`.
///
/// The package name is checked rather than only the exit status: a file
/// claimed by some other package is not one this application knows how to
/// update.
fn owner_of(path: &Path) -> Option<Manager> {
    let asked = |program: &str, args: &[&str]| -> Option<String> {
        let output = Command::new(program)
            .args(args)
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    // `adguard-ui: /usr/bin/adguard-ui`
    if asked("dpkg-query", &["-S"]).is_some_and(|said| said.starts_with(&format!("{BINARY}:"))) {
        return Some(Manager::Dpkg);
    }
    if asked("rpm", &["-qf", "--queryformat", "%{NAME}"]).is_some_and(|said| said.trim() == BINARY) {
        return Some(Manager::Rpm);
    }
    None
}

/// Whether `program` is on `$PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// A release file that has been downloaded and verified.
#[derive(Debug)]
pub struct Downloaded {
    path: PathBuf,
    name: String,
}

/// Fetch this install's file of `release` and its signature, verify the one
/// against the other, and only then write it to disk.
///
/// Blocking, and called from a worker thread.
pub fn download(install: &Install, release: &Release) -> Result<Downloaded, Error> {
    if !install.can_update() {
        return Err(Error::Unmanaged);
    }
    let name = install.asset(release).ok_or_else(|| Error::NoPackage(release.tag.clone()))?;
    let agent = release::agent(DOWNLOAD_TIMEOUT);

    let fetch = |url: String, limit: u64| -> Result<Vec<u8>, ureq::Error> {
        agent.get(url).call()?.body_mut().with_config().limit(limit).read_to_vec()
    };
    let failed = |err: ureq::Error| Error::Download {
        name: name.clone(),
        reason: err.to_string(),
    };

    // The signature first: it is tiny, and a release without one is not worth
    // downloading the package of.
    let signature = match fetch(release.asset_url(&format!("{name}.minisig")), SIGNATURE_LIMIT) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(ureq::Error::StatusCode(404)) => return Err(Error::Unsigned(name)),
        Err(err) => return Err(failed(err)),
    };
    let package = match fetch(release.asset_url(&name), PACKAGE_LIMIT) {
        Ok(bytes) => bytes,
        // Signed but missing: the release was published without this file.
        Err(ureq::Error::StatusCode(404)) => return Err(Error::NoPackage(release.tag.clone())),
        Err(err) => return Err(failed(err)),
    };

    verify(&package, &signature, &name, PUBLIC_KEY)?;

    let dir = workspace()?;
    let path = dir.join(&name);
    write_private(&path, &package)?;
    Ok(Downloaded { path, name })
}

/// Check `bytes` against a minisign `signature` by `key`, and check the
/// signature was made for a file called `name`.
fn verify(bytes: &[u8], signature: &str, name: &str, key: &str) -> Result<(), Error> {
    let forged = |reason: String| Error::Forged {
        name: name.to_owned(),
        reason,
    };
    let key = minisign_verify::PublicKey::decode(key.trim())
        .map_err(|err| forged(format!("the built-in key is unreadable ({err})")))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|err| forged(format!("the signature is unreadable ({err})")))?;
    // `false`: only the pre-hashed form minisign has made by default since
    // 0.10. The legacy form signs the raw file, and nothing here produces it.
    key.verify(bytes, &signature, false).map_err(|err| forged(err.to_string()))?;

    // minisign writes `timestamp:<n>\tfile:<name>\thashed` and signs it with the
    // rest. A token rather than a substring, so `file:adguard-ui.deb` cannot be
    // satisfied by `file:adguard-ui.deb.old`.
    let named = signature
        .trusted_comment()
        .split_whitespace()
        .any(|token| token.strip_prefix("file:") == Some(name));
    if !named {
        return Err(forged(format!(
            "it was signed as a different file ({})",
            signature.trusted_comment()
        )));
    }
    Ok(())
}

/// `~/.cache/adguard-ui/updates`, created private and emptied of anything an
/// earlier attempt left behind.
fn workspace() -> Result<PathBuf, Error> {
    let cache = cache_home().ok_or_else(|| {
        Error::Io(io::Error::new(io::ErrorKind::NotFound, "there is no home directory"))
    })?;
    let dir = cache.join(BINARY).join("updates");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    // `recursive` leaves an existing directory's mode alone.
    fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    for entry in fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    Ok(dir)
}

/// `$XDG_CACHE_HOME`, or `~/.cache`, with the spec's rule about relative values.
fn cache_home() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg);
        }
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache"))
}

/// Written beside its final name and renamed into place, readable by its owner
/// only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let part = path.with_extension("part");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&part, path)?;
    Ok(())
}

/// Install what [`download`] fetched.
///
/// Blocking for as long as the package manager runs, which includes the time
/// the user spends at the password prompt. Called from a worker thread.
pub fn apply(install: &Install, downloaded: &Downloaded) -> Result<(), Error> {
    let result = match install {
        Install::Deb { .. } => privileged(&[
            "apt-get",
            "install",
            "--yes",
            "-o",
            APT_LOCK_WAIT,
            path_str(&downloaded.path)?,
        ]),
        Install::Rpm { .. } => privileged(&["dnf", "install", "--assumeyes", path_str(&downloaded.path)?]),
        Install::Local { prefix, .. } => unpack_and_install(downloaded, prefix),
        Install::Unmanaged { .. } => Err(Error::Unmanaged),
    };
    // Whatever happened, the file has served its purpose. A failed attempt
    // downloads again rather than trusting a file that sat on disk meanwhile.
    if let Some(dir) = downloaded.path.parent() {
        let _ = fs::remove_file(&downloaded.path);
        let _ = fs::remove_dir_all(dir.join(extracted_dir(&downloaded.name)));
    }
    result
}

fn path_str(path: &Path) -> Result<&str, Error> {
    path.to_str().ok_or_else(|| {
        Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "the cache directory's path is not UTF-8"))
    })
}

/// Run a package manager as root through `pkexec`.
///
/// The polkit dialog names the command, which is why it is the package manager
/// itself that is run and not a wrapper script of ours: what the user is asked
/// to authorise is exactly what runs.
fn privileged(argv: &[&str]) -> Result<(), Error> {
    let output = Command::new("pkexec")
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error::Failed {
            command: "pkexec".to_owned(),
            detail: if err.kind() == io::ErrorKind::NotFound {
                "it is not installed, so there is no way to ask for your password — install \
                 the release from its page instead"
                    .to_owned()
            } else {
                err.to_string()
            },
        })?;
    match output.status.code() {
        Some(0) => Ok(()),
        // pkexec(1): 126 when the dialog was dismissed. 127 is "not authorized",
        // which is also what a wrong password three times reads as — a failure
        // worth its own words, not a cancel.
        Some(126) => Err(Error::Cancelled),
        Some(127) => Err(Error::Failed {
            command: argv[0].to_owned(),
            detail: "the system did not authorise it".to_owned(),
        }),
        _ => Err(Error::Failed {
            command: argv[0].to_owned(),
            detail: last_line(&output.stderr)
                .or_else(|| last_line(&output.stdout))
                .unwrap_or_else(|| output.status.to_string()),
        }),
    }
}

/// The tarball unpacks to `adguard-ui-<version>-<arch>/`.
fn extracted_dir(name: &str) -> &str {
    name.strip_suffix(".tar.gz").unwrap_or(name)
}

/// Unpack the tarball beside itself and run the `install.sh` it carries, with
/// the prefix this copy was installed into.
///
/// The release's own script rather than a copy of its steps here, so the
/// desktop entry and icons move with the binary, and so the two cannot drift.
/// It is part of what the signature covered. `install` replaces a running
/// binary by unlinking it first (measured: a plain `cp` over it fails with
/// `Text file busy`, `install` succeeds), so this process keeps running from
/// the old inode until it restarts.
fn unpack_and_install(downloaded: &Downloaded, prefix: &Path) -> Result<(), Error> {
    let dir = downloaded.path.parent().unwrap_or(Path::new("."));
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&downloaded.path)
        .arg("-C")
        // GNU tar strips a leading `/` from member names and refuses any that
        // contain `..`, so the archive cannot write outside this directory —
        // not that a signed one would try.
        .arg(dir))?;
    let script = dir.join(extracted_dir(&downloaded.name)).join("install.sh");
    if !script.is_file() {
        return Err(Error::Failed {
            command: "install.sh".to_owned(),
            detail: format!("{} does not carry one", downloaded.name),
        });
    }
    run(Command::new(&script).env("PREFIX", prefix))
}

fn run(command: &mut Command) -> Result<(), Error> {
    let name = command.get_program().to_string_lossy().into_owned();
    let output = command.stdin(Stdio::null()).output().map_err(|err| Error::Failed {
        command: name.clone(),
        detail: err.to_string(),
    })?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Failed {
        command: name,
        detail: last_line(&output.stderr).unwrap_or_else(|| output.status.to_string()),
    })
}

/// The last non-empty line a command wrote, which is where every one of these
/// tools puts the reason it stopped.
fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Whether AdGuard UI looks for updates by itself, once a day.
///
/// On unless switched off. `~/.config/adguard-ui/updates` holds the choice,
/// and only once it has been made: a missing or unreadable file is the
/// default, so nothing is written for a user who never touches the switch.
pub struct Preferences {
    path: PathBuf,
}

/// The one line the file holds when the check is off.
const AUTOMATIC_OFF: &str = "check-automatically=false";
const AUTOMATIC_ON: &str = "check-automatically=true";

impl Preferences {
    pub fn locate() -> Option<Self> {
        Some(Self::in_config_home(&crate::autostart::config_home()?))
    }

    pub fn in_config_home(config: &Path) -> Self {
        Self {
            path: config.join(BINARY).join("updates"),
        }
    }

    pub fn automatic(&self) -> bool {
        fs::read_to_string(&self.path)
            .map(|text| !text.lines().any(|line| line.trim() == AUTOMATIC_OFF))
            .unwrap_or(true)
    }

    pub fn set_automatic(&self, on: bool) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let line = if on { AUTOMATIC_ON } else { AUTOMATIC_OFF };
        fs::write(&self.path, format!("{line}\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key made for these tests alone, and a file it signed with `minisign
    /// -S` 0.12 — the same tool and default trusted comment the release
    /// workflow uses. Its secret half was thrown away.
    const TEST_KEY: &str = "untrusted comment: minisign public key D7800A20D817D028
RWQo0BfYIAqA133jMbNuNfaiHiMJtR26+tEOwhra9vdJDsxIdw1AWpyd
";
    const SIGNED: &[u8] = b"AdGuard UI test payload\n";
    const SIGNED_AS: &str = "adguard-ui_9.9.9_amd64.deb";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUQo0BfYIAqA1/4hDBWDVTUzCAGw68+aCuis2rAOh9VFfRHiiVHf+8KkjQ+wtctVmyuQ1vy3iVvRB5PWpo8KyvZ48k543NHKlgE=
trusted comment: timestamp:1791709193\tfile:adguard-ui_9.9.9_amd64.deb\thashed
Dj6IpU7iOcflk/s+KAx6yXgKPp61rJvR74B3ocI6UndRHbwEvfYrwrEVJ5+bHFiYQJbLLtQ+bkVwjERbE9SOBA==
";

    #[test]
    fn a_signed_file_verifies() {
        verify(SIGNED, SIGNATURE, SIGNED_AS, TEST_KEY).expect("the fixture is signed");
    }

    #[test]
    fn a_changed_byte_is_refused() {
        let mut tampered = SIGNED.to_vec();
        tampered[0] ^= 1;
        assert!(matches!(
            verify(&tampered, SIGNATURE, SIGNED_AS, TEST_KEY),
            Err(Error::Forged { .. })
        ));
    }

    /// A genuine signature of a genuine release file, offered as a different
    /// file — last year's package served under this year's name.
    #[test]
    fn a_signature_for_another_file_is_refused() {
        let err = verify(SIGNED, SIGNATURE, "adguard-ui_9.9.10_amd64.deb", TEST_KEY);
        assert!(matches!(err, Err(Error::Forged { .. })), "{err:?}");
        let err = verify(SIGNED, SIGNATURE, "adguard-ui_9.9.9_amd64", TEST_KEY);
        assert!(matches!(err, Err(Error::Forged { .. })), "a prefix of the name is not the name");
    }

    /// The trusted comment is covered by the signature, so editing it to name
    /// the file wanted is itself a forgery.
    #[test]
    fn an_edited_trusted_comment_is_refused() {
        let edited = SIGNATURE.replace("9.9.9", "9.9.10");
        let err = verify(SIGNED, &edited, "adguard-ui_9.9.10_amd64.deb", TEST_KEY);
        assert!(matches!(err, Err(Error::Forged { .. })), "{err:?}");
    }

    /// The fixture's key is not the release key, so nothing the tests can sign
    /// would pass in a real download.
    #[test]
    fn the_release_key_is_not_the_test_key() {
        assert!(matches!(
            verify(SIGNED, SIGNATURE, SIGNED_AS, PUBLIC_KEY),
            Err(Error::Forged { .. })
        ));
        minisign_verify::PublicKey::decode(PUBLIC_KEY.trim()).expect("the release key parses");
    }

    #[test]
    fn garbage_is_a_forgery_not_a_panic() {
        assert!(matches!(verify(SIGNED, "", SIGNED_AS, TEST_KEY), Err(Error::Forged { .. })));
        assert!(matches!(
            verify(SIGNED, "<html>Not Found</html>", SIGNED_AS, TEST_KEY),
            Err(Error::Forged { .. })
        ));
    }

    fn deb() -> Install {
        Install::Deb { exe: "/usr/bin/adguard-ui".into() }
    }

    /// The names `packaging/deb.sh`, `rpm.sh` and `tarball.sh` write.
    #[test]
    fn asset_names_match_the_packaging_scripts() {
        let local = Install::Local { prefix: "/home/u/.local".into(), exe: "/home/u/.local/bin/adguard-ui".into() };
        let rpm = Install::Rpm { exe: "/usr/bin/adguard-ui".into() };
        assert_eq!(asset_name(&deb(), "1.10.0", "x86_64").as_deref(), Some("adguard-ui_1.10.0_amd64.deb"));
        assert_eq!(asset_name(&deb(), "1.10.0", "aarch64").as_deref(), Some("adguard-ui_1.10.0_arm64.deb"));
        assert_eq!(asset_name(&deb(), "1.10.0", "riscv64"), None);
        assert_eq!(asset_name(&rpm, "1.10.0", "x86_64").as_deref(), Some("adguard-ui-1.10.0-1.x86_64.rpm"));
        assert_eq!(
            asset_name(&local, "1.10.0", "x86_64").as_deref(),
            Some("adguard-ui-1.10.0-x86_64.tar.gz")
        );
        assert_eq!(asset_name(&Install::Unmanaged { exe: "/x".into() }, "1.10.0", "x86_64"), None);
    }

    #[test]
    fn a_prerelease_tag_names_no_file() {
        let release = Release { tag: "v1.10.0-rc1".into() };
        assert_eq!(deb().asset(&release), None);
    }

    fn classified(exe: &str, owner: Option<Manager>, tools: &[&str]) -> Install {
        classify(
            exe.into(),
            Some(Path::new("/home/u")),
            &move |_| owner,
            &|program| tools.contains(&program),
        )
    }

    #[test]
    fn a_packaged_binary_is_updated_by_its_package_manager() {
        assert_eq!(classified("/usr/bin/adguard-ui", Some(Manager::Dpkg), &["apt-get"]), deb());
        assert_eq!(
            classified("/usr/bin/adguard-ui", Some(Manager::Rpm), &["dnf"]),
            Install::Rpm { exe: "/usr/bin/adguard-ui".into() }
        );
    }

    /// Owned but with nothing to install the next one with, or not owned at
    /// all — a file someone copied there.
    #[test]
    fn a_system_binary_without_its_tools_is_left_alone() {
        let unmanaged = Install::Unmanaged { exe: "/usr/bin/adguard-ui".into() };
        assert_eq!(classified("/usr/bin/adguard-ui", Some(Manager::Dpkg), &[]), unmanaged);
        assert_eq!(classified("/usr/bin/adguard-ui", Some(Manager::Rpm), &["apt-get"]), unmanaged);
        assert_eq!(classified("/usr/bin/adguard-ui", None, &["apt-get", "dnf"]), unmanaged);
    }

    #[test]
    fn a_binary_under_home_bin_is_a_tarball_install() {
        assert_eq!(
            classified("/home/u/.local/bin/adguard-ui", None, &[]),
            Install::Local { prefix: "/home/u/.local".into(), exe: "/home/u/.local/bin/adguard-ui".into() }
        );
        assert_eq!(
            classified("/home/u/opt/bin/adguard-ui", None, &[]),
            Install::Local { prefix: "/home/u/opt".into(), exe: "/home/u/opt/bin/adguard-ui".into() }
        );
    }

    /// A working copy is never replaced by a release, and neither is anything
    /// outside the home directory that no package owns.
    #[test]
    fn builds_and_strays_are_unmanaged() {
        for exe in [
            "/home/u/src/AdGuard-UI-Linux/target/debug/adguard-ui",
            "/home/u/src/AdGuard-UI-Linux/target/release/adguard-ui",
            "/usr/local/bin/adguard-ui",
            "/opt/bin/adguard-ui",
            "/home/u/bin/adguard-ui-old",
            "/home/u/bin/adguard-ui",
        ] {
            let install = classified(exe, None, &["apt-get", "dnf"]);
            // `/home/u/bin` is `<home>/bin` with the home directory as its
            // prefix — `install.sh` would never write there with PREFIX=$HOME
            // by default, and replacing whatever else lives in `~` is not ours.
            assert_eq!(install, Install::Unmanaged { exe: exe.into() }, "{exe}");
            assert!(!install.can_update());
        }
    }

    #[test]
    fn the_extracted_directory_is_the_archive_name() {
        assert_eq!(extracted_dir("adguard-ui-1.10.0-x86_64.tar.gz"), "adguard-ui-1.10.0-x86_64");
    }

    #[test]
    fn automatic_checking_is_on_until_switched_off() {
        let dir = std::env::temp_dir().join(format!("adguard-ui-prefs-{}", std::process::id()));
        let prefs = Preferences::in_config_home(&dir);
        assert!(prefs.automatic(), "no file is the default, and the default is on");
        prefs.set_automatic(false).unwrap();
        assert!(!prefs.automatic());
        prefs.set_automatic(true).unwrap();
        assert!(prefs.automatic());
        fs::write(dir.join("adguard-ui/updates"), "nonsense\n").unwrap();
        assert!(prefs.automatic(), "an unreadable choice is no choice");
        let _ = fs::remove_dir_all(&dir);
    }
}
