//! Locating the `adguard-cli` installation.
//!
//! The CLI is a user-local install on the reference machine
//! (`~/.local/bin/adguard-cli` -> `~/.local/opt/adguard-cli/adguard-cli`),
//! but it may equally be on `$PATH` or system-wide, so probe rather than assume.

use std::env;
use std::path::PathBuf;

const BINARY: &str = "adguard-cli";
const DATA_SUBDIR: &str = "adguard-cli";

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

/// The main configuration file.
///
/// Read this for authoritative values; never write it. Roughly half of its
/// lines are upstream explanatory comments that a YAML serialiser would
/// destroy — writes go through `adguard-cli config set`.
pub fn config_file() -> Option<PathBuf> {
    Some(data_dir()?.join("proxy.yaml"))
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
