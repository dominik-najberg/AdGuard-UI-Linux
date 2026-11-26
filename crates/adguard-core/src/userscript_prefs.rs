//! Whether the Extensions page installs userscript updates by itself.
//!
//! **Off unless switched on**, the opposite of [`crate::update::Preferences`]
//! for the application's own updates. Checking is harmless and always happens
//! (the page's daily badge check); installing replaces code that runs inside
//! every page the user visits, from a server whoever wrote the script
//! controls. That is the user's to turn on, not ours.
//!
//! `~/.config/adguard-ui/userscripts` holds the choice once it has been made,
//! in the one-line shape `update.rs` uses for its own: a missing or unreadable
//! file is the default, so nothing is written for someone who never touches
//! the switch.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const ON: &str = "update-automatically=true";
const OFF: &str = "update-automatically=false";
const FILE: &str = "userscripts";
const SUBDIR: &str = "adguard-ui";

pub struct AutoUpdate {
    path: PathBuf,
}

impl AutoUpdate {
    pub fn locate() -> Option<Self> {
        Some(Self::in_config_home(&crate::autostart::config_home()?))
    }

    pub fn in_config_home(config: &Path) -> Self {
        Self {
            path: config.join(SUBDIR).join(FILE),
        }
    }

    pub fn enabled(&self) -> bool {
        fs::read_to_string(&self.path)
            .map(|text| text.lines().any(|line| line.trim() == ON))
            .unwrap_or(false)
    }

    pub fn set(&self, on: bool) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.path, format!("{}\n", if on { ON } else { OFF }))
    }
}

/// Whether a script with an update waiting may be updated without asking.
///
/// Only one that is switched on, not edited, with no edit set aside, and
/// reachable by the CLI. A switched-off script would be switched back on by
/// the install (contract §15); an edited one would go through a merge whose
/// clash needs an answer nobody is there to give; and an ambiguous one is one
/// this page cannot switch off again afterwards. Each of those keeps its badge
/// and waits for *Update to x.y*.
pub fn eligible(enabled: bool, edited: bool, edit_set_aside: bool, actionable: bool) -> bool {
    enabled && !edited && !edit_set_aside && actionable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_until_switched_on_and_remembered() {
        let dir = std::env::temp_dir().join(format!("userscript-prefs-{}", std::process::id()));
        let prefs = AutoUpdate::in_config_home(&dir);
        assert!(!prefs.enabled(), "off with no file");
        prefs.set(true).unwrap();
        assert!(prefs.enabled());
        prefs.set(false).unwrap();
        assert!(!prefs.enabled());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_plain_switched_on_scripts_update_by_themselves() {
        assert!(eligible(true, false, false, true));
        assert!(!eligible(false, false, false, true), "switched off");
        assert!(!eligible(true, true, false, true), "edited");
        assert!(!eligible(true, false, true, true), "an edit set aside");
        assert!(!eligible(true, false, false, false), "ambiguous");
    }
}
