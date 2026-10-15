//! Installing **AdGuard CLI** where it is missing (#51) — AdGuard's command,
//! not ours.
//!
//! AdGuard documents one route onto Linux, its knowledge base's *Installation,
//! setup, and removal* page: a `curl … | sh` of the `install.sh` on the branch
//! of AdGuardCLI named for the update channel. That script downloads the
//! release tarball from GitHub, asks for the administrator password to create
//! `/opt/adguard-cli`, and asks whether to link the binary into
//! `/usr/local/bin`. There are no distribution packages to choose between —
//! the same tarball serves every distribution — so nothing here looks at which
//! one this is.
//!
//! [`command`] is that line **byte for byte**, the way the certificate and the
//! root helper commands elsewhere in this crate are AdGuard's own lines. The
//! whole case for offering it is that it is the one upstream documents and
//! supports; a variant of ours — an `-o` into `~/.local` to spare the password,
//! say — would be a layout AdGuard's own instructions and support no longer
//! describe. It is handed to a terminal the user drives, where the password is
//! typed into `sudo` and the link question is answered, and this application
//! sees neither (`architecture.md` §6).
//!
//! **What it does not do is check what it downloads.** AdGuard's script fetches
//! and unpacks the tarball without verifying its signature; it only prints the
//! command that would. That is upstream's documented route as it stands, and
//! the page offering the button says plainly that it downloads from GitHub.

/// AdGuard publishes builds for these two CPUs and no other; the script itself
/// stops with "Unsupported CPU type" anywhere else, so the button is not
/// offered there.
const SUPPORTED_ARCHES: [&str; 2] = ["x86_64", "aarch64"];

/// AdGuard's three update channels, each its own branch of AdGuardCLI with its
/// own `install.sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Release,
    Beta,
    Nightly,
}

impl Channel {
    /// In the order the knowledge base lists them, which is the order the
    /// picker shows them, with the default first.
    pub const ALL: [Self; 3] = [Self::Release, Self::Beta, Self::Nightly];

    /// The branch name, which is also the word AdGuard uses for the channel.
    pub fn branch(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Beta => "beta",
            Self::Nightly => "nightly",
        }
    }

    /// How the picker names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Release => "Release",
            Self::Beta => "Beta",
            Self::Nightly => "Nightly",
        }
    }
}

/// AdGuard's documented install command for `channel`, unchanged.
pub fn command(channel: Channel) -> String {
    format!(
        "curl -fsSL https://raw.githubusercontent.com/AdguardTeam/AdGuardCLI/{}/install.sh | sh -s -- -v",
        channel.branch()
    )
}

/// Whether AdGuard ships a build for the CPU this was compiled for.
///
/// The compile target rather than `uname -m`: this binary runs on the machine
/// it was built for, and that is the CPU the downloaded AdGuard has to match.
pub fn supported_arch() -> bool {
    arch_supported(std::env::consts::ARCH)
}

fn arch_supported(arch: &str) -> bool {
    SUPPORTED_ARCHES.contains(&arch)
}

/// The CPU name to give when [`supported_arch`] is false.
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines on adguard.com/kb/adguard-for-linux/installation/, as of
    /// 15 October 2026. If upstream changes them, this is what should fail.
    #[test]
    fn each_channel_is_the_knowledge_bases_line_unchanged() {
        assert_eq!(
            command(Channel::Release),
            "curl -fsSL https://raw.githubusercontent.com/AdguardTeam/AdGuardCLI/release/install.sh | sh -s -- -v"
        );
        assert_eq!(
            command(Channel::Beta),
            "curl -fsSL https://raw.githubusercontent.com/AdguardTeam/AdGuardCLI/beta/install.sh | sh -s -- -v"
        );
        assert_eq!(
            command(Channel::Nightly),
            "curl -fsSL https://raw.githubusercontent.com/AdguardTeam/AdGuardCLI/nightly/install.sh | sh -s -- -v"
        );
    }

    #[test]
    fn release_is_the_default_the_picker_opens_on() {
        assert_eq!(Channel::ALL[0], Channel::Release);
    }

    /// `install.sh` maps `arm64` to `aarch64` itself, but Rust never reports
    /// `arm64` on Linux, so only the two names it does report are accepted.
    #[test]
    fn only_the_two_cpus_adguard_builds_for_are_offered() {
        assert!(arch_supported("x86_64"));
        assert!(arch_supported("aarch64"));
        assert!(!arch_supported("riscv64"));
        assert!(!arch_supported("x86"));
        assert!(!arch_supported("arm"));
    }
}
