//! Locating the `adguard-cli` installation.
//!
//! The CLI is a user-local install on the reference machine
//! (`~/.local/bin/adguard-cli` -> `~/.local/opt/adguard-cli/adguard-cli`),
//! but it may equally be on `$PATH` or system-wide, so probe rather than assume.

use std::env;
use std::path::{Path, PathBuf};

const BINARY: &str = "adguard-cli";
const ROOT_HELPER: &str = "adguard_root_helper";
const CERT_INSTALLER: &str = "install_cert.sh";
const DATA_SUBDIR: &str = "adguard-cli";
const CONFIG_FILE: &str = "proxy.yaml";

fn home() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

/// Locate the `adguard-cli` binary.
///
/// Order: `$ADGUARD_CLI` override, then `$PATH`, then the known user-local
/// install sites. Returns `None` when AdGuard CLI is not installed — callers
/// must surface that as a clear message rather than panicking.
pub fn cli_binary() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("ADGUARD_CLI") {
        let candidate = PathBuf::from(explicit);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(BINARY);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let home = home()?;
    [
        home.join(".local/bin").join(BINARY),
        home.join(".local/opt/adguard-cli").join(BINARY),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// AdGuard's own root helper, beside the **resolved** `adguard-cli` binary.
///
/// Resolved, not merely the path [`cli_binary`] returned: `$PATH` is searched
/// first and on the reference machine that finds `~/.local/bin/adguard-cli`,
/// which is a symlink into `~/.local/opt/adguard-cli/`. The helper is a sibling
/// of the real binary, so joining the *link's* parent finds nothing at all —
/// and "nothing" would be indistinguishable from AdGuard not being installed
/// (contract §8).
///
/// Returns the path whether or not anything is there; whether it exists, and
/// what its mode is, is [`crate::helper::RootHelper`]'s question. `None` means
/// only that the CLI itself could not be located.
pub fn root_helper() -> Option<PathBuf> {
    beside_binary(ROOT_HELPER)
}

/// AdGuard's own certificate installer, beside the **resolved** binary.
///
/// The script the CLI names as its manual-install route — `install_cert.sh`,
/// which appears in the binary's strings next to `get_manual_install_script`
/// (see [`crate::trust`]). It copies the CA into the system's anchor directory,
/// rebuilds the trust store and adds the certificate to Firefox and Chrome with
/// the `certutil` shipped beside it, elevating itself with `sudo` on the way.
///
/// Found the same way as [`root_helper`] and for the same measured reason: the
/// entry on `$PATH` is a symlink on the reference machine, and the installer is
/// a sibling of the real file rather than of the link.
///
/// Returns the path whether or not anything is there. `None` means only that
/// the CLI itself could not be located.
pub fn cert_installer() -> Option<PathBuf> {
    beside_binary(CERT_INSTALLER)
}

/// AdGuard's data directory holds the CA it generates for HTTPS filtering,
/// named after the `https_filtering.root_certificate_name` setting.
///
/// The naming rule is AdGuard's, and it is measured rather than assumed: the
/// binary composes the DER copy's path as `{}/{}.cer` and the file on disk is
/// `SSL/AdGuard CLI CA.cer`, against a config whose `root_certificate_name` is
/// `AdGuard CLI CA`. The PEM this returns is the same name in the data
/// directory itself, which is where `certificates_cache: '.'` puts it.
///
/// Note the certificate is *named* by a setting the user can change, so a
/// caller with a [`crate::Config`] in hand should pass
/// [`crate::Config::certificate_name`] rather than a constant — a renamed CA is
/// otherwise indistinguishable from an install that never generated one.
pub fn certificate(name: &str) -> Option<PathBuf> {
    Some(data_dir()?.join(format!("{name}.pem")))
}

/// AdGuard's native-messaging host, beside the **resolved** binary.
///
/// The program a browser launches on behalf of AdGuard's extension — the far
/// end of the `connectNative` call in [`crate::browser`]. It is what the
/// manifests `install-browser-integration` writes name in their `path`, so it
/// is also what those manifests are checked against.
///
/// Found the same way as [`root_helper`] and for the same measured reason: the
/// entry on `$PATH` is a symlink on the reference machine, and the host is a
/// sibling of the real file rather than of the link.
///
/// Returns the path whether or not anything is there — whether it exists is
/// [`crate::browser::BrowserIntegration`]'s question, and one it reports
/// separately. `None` means only that the CLI itself could not be located.
pub fn nm_host() -> Option<PathBuf> {
    beside_binary(crate::browser::HOST_BINARY)
}

/// A file shipped alongside the **resolved** `adguard-cli` binary.
fn beside_binary(name: &str) -> Option<PathBuf> {
    let binary = cli_binary()?;
    // `canonicalize` needs the target to exist, which it does — `cli_binary`
    // only returns paths that passed `is_file`. Falling back to the unresolved
    // path keeps a non-symlink install working if it ever failed.
    let resolved = std::fs::canonicalize(&binary).unwrap_or(binary);
    Some(resolved.parent()?.join(name))
}

/// AdGuard CLI's data directory: config, databases, logs, certificates.
///
/// Note this is AdGuard's own XDG data dir, not ours.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join(DATA_SUBDIR));
        }
    }
    Some(home()?.join(".local/share").join(DATA_SUBDIR))
}

/// AdGuard's data directory under an explicitly given `$XDG_DATA_HOME`.
///
/// [`data_dir`] answers for *this* process's environment, which is the right
/// answer everywhere except one place: [`crate::Cli::with_xdg_data_home`] sets
/// the variable on the child only, so a `Cli` pointed at a sandbox cannot ask
/// the environment where its own config lives. It has to be told.
pub fn data_dir_under(xdg_data_home: &Path) -> PathBuf {
    xdg_data_home.join(DATA_SUBDIR)
}

/// The main configuration file.
///
/// Read this for authoritative values; never write it. Roughly half of its
/// lines are upstream explanatory comments that a YAML serialiser would
/// destroy — writes go through `adguard-cli config set`.
///
/// **Its absence is meaningful, not merely inconvenient.** Measured on
/// v1.4.13: a data directory that has never been configured has no
/// `proxy.yaml` at all, and nothing creates one except `configure` — not
/// `config get`, not `config set`, not `activate`. Until it exists `config
/// set` refuses every real key, so "this file is missing" is exactly the
/// first-run condition (contract §5).
pub fn config_file() -> Option<PathBuf> {
    Some(data_dir()?.join(CONFIG_FILE))
}

/// [`config_file`] under an explicitly given `$XDG_DATA_HOME`.
pub fn config_file_under(xdg_data_home: &Path) -> PathBuf {
    data_dir_under(xdg_data_home).join(CONFIG_FILE)
}

/// The user's own HTTP filtering rules — the file behind the user-rules
/// pseudo-filter. Hand-editable; the CLI itself suggests editing it directly.
pub fn user_rules_file() -> Option<PathBuf> {
    Some(data_dir()?.join("user.txt"))
}

/// The user's own DNS filtering rules.
pub fn dns_user_rules_file() -> Option<PathBuf> {
    Some(data_dir()?.join("dns_user.txt"))
}

/// SQLite catalogue of HTTP/HTTPS filters. Open read-only.
pub fn filters_db() -> Option<PathBuf> {
    Some(data_dir()?.join("agflm_standard.db"))
}

/// SQLite catalogue of DNS filters. Open read-only.
pub fn dns_filters_db() -> Option<PathBuf> {
    Some(data_dir()?.join("agflm_dns.db"))
}
