//! Whether a newer **AdGuard UI** has been released.
//!
//! The other half of the About page's update story, and the only place this
//! application touches the network on its own. Everything else it causes goes
//! through `adguard-cli`, which is why this module is small, blunt, and
//! reachable only from a button the user pressed.
//!
//! # This application has no update channel, which is why a check is all there is
//!
//! Releases are a `.deb` and a tarball on GitHub. There is no apt repository, so
//! `apt upgrade` will not move an installed package, and nothing here can
//! install anything: the check reports, names the release, and offers to open
//! the page. That is the same posture the AdGuard CLI half takes for a different
//! reason (`architecture.md` §6) and it happens to land in the same place.
//!
//! # Two measured decisions
//!
//! **The platform certificate verifier, not bundled roots.** On a machine
//! filtering system-wide — which is what this application exists to configure —
//! AdGuard intercepts this very connection and re-signs it with its own CA. That
//! CA is in the *system* trust store, so a client carrying Mozilla's bundled
//! roots would fail on exactly the machines that matter. Measured 9 August 2026:
//! `gio cat` against this endpoint succeeds on this machine, which is in `auto`
//! mode with system-wide filtering enabled, precisely because it uses the system
//! store. The feature selection in the workspace manifest says the same thing.
//!
//! **The response is scanned for one field, not deserialised.** A JSON parser
//! for a single string would be a second dependency behind the first, and this
//! crate already reads far less forgiving formats by hand. An answer that does
//! not carry the field is an [`Error::Unreadable`] rather than a guess — the
//! rule [`crate::cli`] follows for every CLI output it cannot recognise.

use std::time::Duration;

/// The repository releases are cut from.
///
/// Pinned by a test against the AppStream metadata, which is the file a fork or
/// a rename would be expected to update.
const REPO: &str = "dominik-najberg/AdGuard-UI-Linux";

/// GitHub's newest **non-prerelease** release. Betas do not appear here, which
/// is the behaviour wanted: a user on a stable build should not be told a
/// release candidate is available.
fn endpoint() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// Generous, because it is a user-initiated check with a visible outcome, and
/// short, because the user is watching a button.
const TIMEOUT: Duration = Duration::from_secs(15);

/// At most this much of the answer is read. The real one is ~2 KB; the cap is
/// what stops a redirected or hostile endpoint from being read into memory
/// indefinitely.
const LIMIT: u64 = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not reach github.com: {0}")]
    Unreachable(String),

    /// GitHub answered, but not with a release. The rate limit lands here —
    /// unauthenticated callers get 60 requests an hour per address, which a
    /// button cannot plausibly exhaust but a loop could.
    #[error("github.com answered {status}")]
    Status { status: u16 },

    #[error("github.com's answer did not name a release")]
    Unreadable,
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The tag, as GitHub spells it — `v1.3.0`.
    pub tag: String,
}

impl Release {
    /// Where a person reads about it. Derived rather than taken from the
    /// answer's `html_url`, so nothing this application opens in a browser came
    /// out of a network response.
    pub fn url(&self) -> String {
        format!("https://github.com/{REPO}/releases/tag/{}", self.tag)
    }
}

/// How the running build stands against the newest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Nothing to do.
    Current,
    /// A newer release exists.
    Behind(Release),
    /// The running build is newer than anything released — a local build, or a
    /// tag that has not been published yet. Worth saying rather than hiding: it
    /// tells a maintainer their working copy is ahead, and it stops the check
    /// reporting "up to date" for a version that was never released.
    Ahead(Release),
    /// The two could not be compared — a pre-release suffix, or a tag in a shape
    /// this does not parse. The release is still named, and the user judges.
    Unknown(Release),
}

/// Ask GitHub for the newest release.
///
/// Blocking, and called from a worker thread like every other slow thing in this
/// project.
pub fn latest() -> Result<Release, Error> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        // GitHub refuses requests without one. It names the application and its
        // version and nothing else — no machine identifier, and nothing that
        // says anything about the user.
        .user_agent(concat!("AdGuard-UI-Linux/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let mut response = agent
        .get(endpoint())
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(status) => Error::Status { status },
            other => Error::Unreachable(other.to_string()),
        })?;

    let body = response
        .body_mut()
        .with_config()
        .limit(LIMIT)
        .read_to_string()
        .map_err(|err| Error::Unreachable(err.to_string()))?;

    tag_of(&body).map(|tag| Release { tag }).ok_or(Error::Unreadable)
}

/// Pull `tag_name` out of GitHub's answer.
///
/// A scan rather than a parse, for the one field wanted. It accepts the whitespace
/// GitHub does not currently emit (`"tag_name" : "v1"`) because that costs three
/// lines and a reformatted response is not a reason to stop working, and it
/// refuses anything else — an answer without the field is [`Error::Unreadable`],
/// never a version invented from the parts that were recognisable.
fn tag_of(body: &str) -> Option<String> {
    let after_key = body.split_once("\"tag_name\"")?.1;
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let opened = after_colon.trim_start().strip_prefix('"')?;
    let (tag, _) = opened.split_once('"')?;
    let tag = tag.trim();
    (!tag.is_empty()).then(|| tag.to_owned())
}

/// Compare the running build against a release.
///
/// `running` is this crate's own `CARGO_PKG_VERSION` in every real call; it is a
/// parameter so the comparison is testable without rebuilding the world.
pub fn standing(running: &str, release: Release) -> Standing {
    let (Some(mine), Some(theirs)) = (parts(running), parts(&release.tag)) else {
        return Standing::Unknown(release);
    };
    match compare(&mine, &theirs) {
        std::cmp::Ordering::Equal => Standing::Current,
        std::cmp::Ordering::Less => Standing::Behind(release),
        std::cmp::Ordering::Greater => Standing::Ahead(release),
    }
}

/// `v1.2.0` -> `[1, 2, 0]`. `None` for anything that is not purely dotted
/// numbers, which includes every pre-release spelling — those are reported as
/// [`Standing::Unknown`] rather than guessed at.
fn parts(version: &str) -> Option<Vec<u64>> {
    let version = version.trim().strip_prefix('v').unwrap_or(version.trim());
    if version.is_empty() {
        return None;
    }
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// Component-wise, with a missing component reading as zero so `1.2` and `1.2.0`
/// are the same version rather than incomparable.
fn compare(mine: &[u64], theirs: &[u64]) -> std::cmp::Ordering {
    let width = mine.len().max(theirs.len());
    for i in 0..width {
        let a = mine.get(i).copied().unwrap_or(0);
        let b = theirs.get(i).copied().unwrap_or(0);
        match a.cmp(&b) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real answer's shape, trimmed to the fields around the one that
    /// matters. Captured 9 August 2026.
    const REAL: &str = r#"{"url":"https://api.github.com/repos/dominik-najberg/AdGuard-UI-Linux/releases/364533954",
        "id":364533954,"tag_name":"v1.2.0","target_commitish":"main","name":"1.2.0","draft":false,
        "prerelease":false,"created_at":"2026-08-05T00:00:00Z"}"#;

    #[test]
    fn reads_the_tag_out_of_a_real_answer() {
        assert_eq!(tag_of(REAL).as_deref(), Some("v1.2.0"));
    }

    #[test]
    fn tolerates_whitespace_around_the_colon() {
        assert_eq!(tag_of(r#"{"tag_name" : "v9.9.9"}"#).as_deref(), Some("v9.9.9"));
    }

    /// An answer without the field is a failure, never a version assembled from
    /// whatever else was in it.
    #[test]
    fn an_answer_without_a_tag_is_unreadable() {
        assert_eq!(tag_of(r#"{"message":"Not Found"}"#), None);
        assert_eq!(tag_of(""), None);
        assert_eq!(tag_of(r#"{"tag_name":""}"#), None);
        assert_eq!(tag_of(r#"{"tag_name":}"#), None);
    }

    fn release(tag: &str) -> Release {
        Release { tag: tag.to_owned() }
    }

    #[test]
    fn a_newer_release_is_behind() {
        assert_eq!(standing("1.2.0", release("v1.3.0")), Standing::Behind(release("v1.3.0")));
        assert_eq!(standing("1.2.0", release("v2.0.0")), Standing::Behind(release("v2.0.0")));
    }

    #[test]
    fn the_same_version_is_current() {
        assert_eq!(standing("1.2.0", release("v1.2.0")), Standing::Current);
        // The `v` is GitHub's, not the version's.
        assert_eq!(standing("1.2.0", release("1.2.0")), Standing::Current);
    }

    /// A local build between releases. Reporting it as "up to date" would tell a
    /// maintainer their working copy matched a release it does not.
    #[test]
    fn a_working_copy_ahead_of_the_releases_says_so() {
        assert_eq!(standing("1.3.0", release("v1.2.0")), Standing::Ahead(release("v1.2.0")));
    }

    /// The trap in string comparison, and the reason this parses integers:
    /// `"1.10.0" < "1.9.0"` as text, which would announce a downgrade as an
    /// update on the tenth patch release of any series.
    #[test]
    fn ten_is_newer_than_nine() {
        assert_eq!(standing("1.9.0", release("v1.10.0")), Standing::Behind(release("v1.10.0")));
        assert_eq!(standing("1.10.0", release("v1.9.0")), Standing::Ahead(release("v1.9.0")));
    }

    #[test]
    fn a_missing_component_reads_as_zero() {
        assert_eq!(standing("1.2", release("v1.2.0")), Standing::Current);
        assert_eq!(standing("1.2.0", release("v1.2")), Standing::Current);
        assert_eq!(standing("1.2", release("v1.2.1")), Standing::Behind(release("v1.2.1")));
    }

    /// A pre-release is not guessed at in either direction — it is named and
    /// left to the user, because "is 1.3.0-rc1 newer than 1.2.0" is a question
    /// about a release process rather than about numbers.
    #[test]
    fn a_prerelease_tag_is_unknown_rather_than_guessed() {
        assert_eq!(
            standing("1.2.0", release("v1.3.0-rc1")),
            Standing::Unknown(release("v1.3.0-rc1"))
        );
        assert_eq!(standing("1.2.0", release("latest")), Standing::Unknown(release("latest")));
    }

    /// Nothing opened in a browser comes out of a network response: the URL is
    /// built from a constant and the tag.
    #[test]
    fn the_release_url_is_derived_and_not_taken_from_the_answer() {
        assert_eq!(
            release("v1.3.0").url(),
            "https://github.com/dominik-najberg/AdGuard-UI-Linux/releases/tag/v1.3.0"
        );
        assert!(endpoint().starts_with("https://api.github.com/repos/"));
    }

    /// The repository this asks about and the one the package advertises are the
    /// same project, and nothing but this connects them.
    #[test]
    fn the_repository_matches_the_appstream_metadata() {
        const METAINFO: &str =
            include_str!("../../../data/io.github.dominik-najberg.AdGuardUI.metainfo.xml");
        assert!(METAINFO.contains(REPO), "the release check names a different repository");
    }
}
