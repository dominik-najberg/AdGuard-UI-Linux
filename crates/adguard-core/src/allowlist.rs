//! Turning AdGuard's filtering off for one site, from the website check.
//!
//! **The one thing this application writes outside `adguard-cli`.** AdGuard
//! CLI has no command for a user rule: its rules are `user.txt`, which the CLI
//! itself suggests editing by hand ([`crate::paths::user_rules_file`]). So an
//! exception is a line appended to that file, and taking it back is that line
//! removed — no other line is touched, and comments and rules the user wrote
//! stay exactly as they were.
//!
//! # The rule, measured
//!
//! `@@||<site>^$document,important`. `$document` switches every kind of
//! filtering off on the site's pages and lets the pages themselves through;
//! `$important` is there because without it an `$important` blocking rule
//! still wins. Both measured on 24 October 2026 against v1.5.0, through the
//! manual proxy (contract §6, *Allowing a site*).
//!
//! # Written, then reloaded
//!
//! AdGuard does not watch `user.txt`, and `filters enable` on the user-rules
//! row makes the running proxy read it again — [`Cli::reload_user_rules`].
//! Without that a written exception would do nothing until protection was next
//! restarted, while the page said the site was allowed.

use std::io;
use std::path::Path;

use crate::Cli;

/// The exception that allows `host`.
pub fn rule(host: &str) -> String {
    format!("@@||{host}^$document,important")
}

/// Whether `text` — `user.txt` — holds [`rule`] for `host`.
pub fn is_allowed(text: &str, host: &str) -> bool {
    let rule = rule(host);
    text.lines().any(|line| line.trim() == rule)
}

/// What [`allow`] or [`disallow`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Written, and AdGuard read the file again.
    Done,
    /// The file already said so; nothing was written.
    Unchanged,
    /// Written, and AdGuard refused to read it again — so it takes effect when
    /// protection next starts. Carries AdGuard's own words.
    NotReloaded(String),
}

/// Add the exception for `host` to `user.txt`, and have AdGuard read it.
/// **Blocking**: one `adguard-cli` invocation.
pub fn allow(cli: &Cli, host: &str) -> io::Result<Outcome> {
    let path = crate::paths::user_rules_file().ok_or_else(no_data_dir)?;
    if !add(&path, host)? {
        return Ok(Outcome::Unchanged);
    }
    Ok(reload(cli))
}

/// Take the exception for `host` out of `user.txt` again.
pub fn disallow(cli: &Cli, host: &str) -> io::Result<Outcome> {
    let path = crate::paths::user_rules_file().ok_or_else(no_data_dir)?;
    if !remove(&path, host)? {
        return Ok(Outcome::Unchanged);
    }
    Ok(reload(cli))
}

fn reload(cli: &Cli) -> Outcome {
    match cli.reload_user_rules() {
        Ok(()) => Outcome::Done,
        Err(err) => Outcome::NotReloaded(err.to_string()),
    }
}

fn no_data_dir() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "AdGuard's data directory could not be found")
}

/// Append the exception for `host` to the file at `path`, creating it if need
/// be. `false` when it is already there.
pub fn add(path: &Path, host: &str) -> io::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if is_allowed(&text, host) {
        return Ok(false);
    }
    let mut next = text;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&rule(host));
    next.push('\n');
    crate::filters::write_user_rules(path, &next)?;
    Ok(true)
}

/// Remove every line that is exactly the exception for `host`. `false` when
/// there was none.
pub fn remove(path: &Path, host: &str) -> io::Result<bool> {
    let text = std::fs::read_to_string(path)?;
    let rule = rule(host);
    let kept: Vec<&str> = text.lines().filter(|line| line.trim() != rule).collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    let mut next = kept.join("\n");
    if text.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    crate::filters::write_user_rules(path, &next)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, text: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("adguard-ui-allowlist-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user.txt");
        let _ = std::fs::remove_file(&path);
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    #[test]
    fn the_rule_is_the_measured_one() {
        assert_eq!(rule("example.com"), "@@||example.com^$document,important");
    }

    #[test]
    fn adding_appends_one_line_and_keeps_everything_else() {
        let path = scratch("add", Some("! User filter\n||ads.example^"));
        assert!(add(&path, "example.com").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "! User filter\n||ads.example^\n@@||example.com^$document,important\n"
        );
        assert!(!add(&path, "example.com").unwrap(), "a second add writes nothing");
    }

    #[test]
    fn adding_creates_a_missing_file() {
        let path = scratch("create", None);
        assert!(add(&path, "example.com").unwrap());
        assert!(is_allowed(&std::fs::read_to_string(&path).unwrap(), "example.com"));
    }

    #[test]
    fn removing_takes_out_only_that_line() {
        let path = scratch(
            "remove",
            Some("! User filter\n@@||example.com^$document,important\n@@||example.com^\n"),
        );
        assert!(remove(&path, "example.com").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "! User filter\n@@||example.com^\n");
        assert!(!remove(&path, "example.com").unwrap());
    }

    #[test]
    fn only_the_exact_rule_counts_as_allowed() {
        assert!(is_allowed("  @@||example.com^$document,important  \n", "example.com"));
        assert!(!is_allowed("@@||example.com^$document\n", "example.com"));
        assert!(!is_allowed("@@||www.example.com^$document,important\n", "example.com"));
    }
}
