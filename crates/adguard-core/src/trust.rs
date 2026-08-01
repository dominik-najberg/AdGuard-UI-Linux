//! Whether AdGuard's root CA is trusted by this machine.
//!
//! HTTPS filtering signs every connection it inspects with a CA generated on
//! this machine, so until that CA is in the system's trust store the filtering
//! it enables breaks the first HTTPS site the user opens. The state this module
//! reports is not a corner case: `configure` generates the certificate and then
//! **skips the one prompt that would install it**, silently, because installing
//! needs a password and there is no TTY (contract §7). Every install this
//! application sets up therefore ends with HTTPS filtering on and the CA
//! outside the trust store.
//!
//! **None of the privilege involved is ours**, exactly as with auto mode
//! (`architecture.md` §6). AdGuard ships the installer and names it itself —
//! measured from the binary's strings, where the format string sits beside the
//! symbol that builds it:
//!
//! ```text
//! get_manual_install_script
//! install_cert.sh
//!  -f "{}"
//! "{}" -c "{}"{}
//! ```
//!
//! So [`install_command`] is AdGuard's own line with this machine's two paths
//! in it, shown for the user to run and never run from here. The script asks
//! for the password itself, and it does the browser stores this module cannot
//! see (see [`CaTrust::is_trusted`]) in the same pass.
//!
//! Nothing here escalates, spawns anything, or writes. It reads three files.
//!
//! # What the check actually looks at
//!
//! Three facts, in the order the machine applies them, because a user who has
//! run the installer and still has broken HTTPS needs to know *which* step did
//! not take:
//!
//! 1. **generated** — AdGuard has a CA at all. It is `<name>.pem` beside
//!    `proxy.yaml`, where `<name>` is `https_filtering.root_certificate_name`.
//! 2. **anchored** — a byte-identical copy sits in the system's anchor
//!    directory, which is where `install_cert.sh` copies it.
//! 3. **bundled** — it is in the trusted bundle, which is what clients read.
//!    Only `update-ca-certificates` puts it there, and the installer runs that
//!    itself; an anchor without a bundle entry means that step failed or has
//!    not run yet.
//!
//! # Three traps, all measured
//!
//! **The bundle carries no names.** `/etc/ssl/certs/ca-certificates.crt` is 123
//! base64 bodies with nothing else between them, so `grep AdGuard` over it
//! returns nothing whether the certificate is trusted or not — measured here
//! against a machine where it *is* trusted. The comparison has to be on the
//! certificate's own body, which is what [`bodies`] extracts.
//!
//! **`update-ca-certificates` only reads `*.crt`.** Its own source:
//! `find -L "$LOCALCERTSDIR" -type f -name '*.crt'`. A `.pem` copied into the
//! anchor directory is ignored in silence — so the anchor this module looks for
//! is `<name>.crt`, the name `install_cert.sh` writes, and not the `.pem` the
//! file is called everywhere else.
//!
//! **AdGuard's installer checks the anchor *path*, not its contents:**
//!
//! ```text
//! if [ ! -f "${SYSTEM_CERT_PATH}" ]; then
//!     ...
//! else
//!     echo "Certificate already exists in system trust store."
//! ```
//!
//! So a CA that was regenerated after being installed leaves a file of the
//! right name holding the wrong certificate, and re-running the installer
//! reports success without replacing it. That state is [`CaTrust::stale`], and
//! it is the reason this module compares bytes rather than asking whether a
//! path exists.

use std::fs;
use std::path::{Path, PathBuf};

/// The certificate name to fall back on when `proxy.yaml` cannot be read or
/// does not carry the key: the CLI's own seeded default (contract §7).
pub const DEFAULT_CERTIFICATE_NAME: &str = "AdGuard CLI CA";

/// Where AdGuard's installer looks for the system's anchor directory, in its
/// order, taking the first that exists.
///
/// Copied from `install_cert.sh` rather than chosen: the point of this check is
/// to report on what that script would do, so a list of our own would be a
/// different question with the same name. Ubuntu is the first entry, and the
/// only one that exists on the reference machine.
const ANCHOR_DIRS: [&str; 4] = [
    "/usr/local/share/ca-certificates",
    "/usr/share/pki/trust/anchors",
    "/etc/pki/ca-trust/source/anchors",
    "/etc/ca-certificates/trust-source/anchors",
];

/// Where the regenerated bundle lands, first match wins.
///
/// **This list is ours, not AdGuard's** — the script names no bundle, it just
/// runs `update-ca-certificates` and `update-ca-trust` and lets each distribution
/// put the result where it keeps it. The first entry is Debian and Ubuntu; the
/// rest are the conventional paths on the distributions the anchor list above
/// implies, so the two lists cover the same machines.
const BUNDLES: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// The commands that rebuild the bundle from the anchor directory, in the order
/// `install_cert.sh` runs them. It requires only that one of the two succeed.
const REFRESH_COMMANDS: [&str; 2] = ["update-ca-certificates", "update-ca-trust"];

/// The three properties of AdGuard's CA on this machine, and the paths they
/// were read from.
///
/// Deliberately not a bool, for the same reason [`crate::RootHelper`] is not
/// one: "not trusted" is the single answer that cannot tell a user which step
/// to take next, and there are three different steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaTrust {
    /// The CA AdGuard generates, at `<data dir>/<certificate name>.pem`.
    /// Kept whether or not it exists, so a report can name what it looked for.
    pub certificate: PathBuf,
    /// The certificate is there and holds at least one PEM body. A file that
    /// exists but parses to nothing reads as **not** generated: there would be
    /// nothing to compare the trust store against, and reporting it as present
    /// would send the user to an installer that cannot succeed.
    pub generated: bool,
    /// Where `install_cert.sh` would copy it — `<anchor dir>/<name>.crt`.
    /// `None` when this machine has none of the four anchor directories, which
    /// is a fact about the distribution rather than about AdGuard.
    pub anchor: Option<PathBuf>,
    /// A file at [`Self::anchor`] holds this same certificate.
    pub anchored: bool,
    /// A file at [`Self::anchor`] holds a **different** certificate.
    ///
    /// Mutually exclusive with [`Self::anchored`]; both false means nothing is
    /// there. This is the state AdGuard's own installer cannot fix, because it
    /// tests for the path and not for the contents — see the module docs.
    pub stale: bool,
    /// The trusted bundle this machine keeps, if one of the known ones exists.
    pub bundle: Option<PathBuf>,
    /// The certificate's body appears in that bundle.
    pub bundled: bool,
}

impl CaTrust {
    /// Read the three properties, against explicitly given locations.
    ///
    /// **Every path is a parameter on purpose**, the same as
    /// [`crate::RootHelper::inspect`] and for a sharper version of the same
    /// reason: on the reference machine the CA *is* trusted, so it is the
    /// **unmet** branches that cannot be reached without pointing the check
    /// somewhere else. A constant buried in the function would leave every
    /// state but one unprovable, and the only way to produce the others for
    /// real would be to modify the machine's trust store — which is precisely
    /// the act this design exists to leave to the user.
    ///
    /// `anchor_dir` and `bundle` are the *resolved* locations, not lists;
    /// [`Self::detect`] does the resolving so that a test does not have to
    /// create four directories to control which one is chosen.
    pub fn inspect(
        certificate: impl Into<PathBuf>,
        anchor_dir: Option<&Path>,
        bundle: Option<&Path>,
    ) -> Self {
        let certificate = certificate.into();
        // One read, kept: the anchor and the bundle are both compared against
        // this, and re-reading between them would let a regeneration land in
        // the middle and produce a reading that was never true at any instant.
        let ours = bodies(&read(&certificate)).into_iter().next();

        let anchor = anchor_dir.map(|dir| dir.join(anchor_name(&certificate)));
        let (anchored, stale) = match (&ours, &anchor) {
            (Some(ours), Some(path)) => match bodies(&read(path)).first() {
                Some(theirs) => (theirs == ours, theirs != ours),
                // Nothing at the path, or a file that is not a certificate.
                // The second reads as absent rather than stale: the installer
                // would overwrite neither, and "a different certificate is
                // installed" would be a claim this check has not established.
                None => (false, false),
            },
            _ => (false, false),
        };

        let bundle = bundle.map(Path::to_path_buf);
        let bundled = match (&ours, &bundle) {
            (Some(ours), Some(path)) => contains(&read(path), ours),
            _ => false,
        };

        Self {
            certificate,
            generated: ours.is_some(),
            anchor,
            anchored,
            stale,
            bundle,
            bundled,
        }
    }

    /// Read this machine's own state, for the certificate named in
    /// `proxy.yaml`.
    ///
    /// `None` when AdGuard's data directory cannot be located at all, which is
    /// the same fact [`crate::paths::data_dir`] reports and not something to
    /// dress up as an untrusted certificate.
    ///
    /// The name is a parameter because it comes from a *setting* —
    /// `https_filtering.root_certificate_name`, which the user may change and
    /// the first-run assistant deliberately does not ask about (`model::SETUP`)
    /// — and reading `proxy.yaml` from here would make a file read out of what
    /// is otherwise three stats. Callers that have a [`crate::Config`] in hand
    /// pass [`crate::config::Config::certificate_name`]; callers that do not
    /// pass [`DEFAULT_CERTIFICATE_NAME`].
    pub fn detect(certificate_name: &str) -> Option<Self> {
        let certificate = crate::paths::certificate(certificate_name)?;
        Some(Self::inspect(
            certificate,
            anchor_dir().as_deref(),
            bundle().as_deref(),
        ))
    }

    /// Whether HTTPS filtering will actually work for a client that reads the
    /// system trust store.
    ///
    /// **That qualifier is the whole of it, and the UI has to carry it too.**
    /// Firefox and Chrome keep their own NSS databases and consult the system
    /// store for nothing; `install_cert.sh` adds the certificate to both — with
    /// the `certutil` AdGuard ships beside it — and this check sees neither.
    /// So a `true` here means the machine trusts the CA, never that every
    /// browser on it does.
    ///
    /// The bundle is what decides, not the anchor: an anchor is an instruction
    /// to `update-ca-certificates`, and until that has run it is a file in a
    /// directory that nothing reads.
    pub fn is_trusted(&self) -> bool {
        self.generated && self.bundled
    }

    /// What is missing, worst first, in wording a row can print. Empty when
    /// [`Self::is_trusted`].
    ///
    /// Only ever one entry: these are steps in a sequence, and naming the
    /// second when the first has not been taken would be advice to run a
    /// command that cannot work. [`crate::RootHelper::unmet`] lists all of its
    /// three because those are independent properties of one file.
    pub fn unmet(&self) -> Vec<&'static str> {
        if !self.generated {
            vec!["no certificate has been generated"]
        } else if self.stale {
            vec!["a different certificate of the same name is already installed"]
        } else if !self.anchored {
            vec!["it has not been installed into the system trust store"]
        } else if !self.bundled {
            vec!["the trust store has not been rebuilt since it was installed"]
        } else {
            Vec::new()
        }
    }
}

/// AdGuard's own manual-install command, with this machine's paths in it.
///
/// The shape is the CLI's own format string, `"{}" -c "{}"`, quotes included —
/// and they are not decoration here. The certificate is named after the
/// `root_certificate_name` setting, whose seeded default is `AdGuard CLI CA`,
/// so the path this command carries contains spaces on a stock install and an
/// unquoted version of it would hand the script three arguments and a usage
/// message.
///
/// Shown for the user to run, never executed from here. The script elevates
/// itself — `sudo_command='sudo'` when not already root — so this application
/// neither asks for a password nor holds one, which is the same rule the root
/// helper follows (`architecture.md` §6).
///
/// `None` when either path could not be put inside those quotes without
/// changing what a shell would do with the line; see [`quotable`].
pub fn install_command(installer: &Path, certificate: &Path) -> Option<String> {
    (quotable(installer) && quotable(certificate)).then(|| {
        format!(
            "\"{}\" -c \"{}\"",
            installer.display(),
            certificate.display()
        )
    })
}

/// Whether a path survives being put inside a double-quoted shell word.
///
/// **This application never runs these commands, which is exactly why this
/// matters.** It hands the user a line, tells them it is AdGuard's, and the
/// user pastes it into a shell — sometimes behind a `sudo`. The certificate's
/// path is not a constant: it is named by
/// `https_filtering.root_certificate_name`, an ordinary setting that `config
/// set` will write any string to. A name carrying `"` or `` ` `` or `$` would
/// close AdGuard's quoting and let the rest of it run as its own command, in a
/// line this application has just vouched for.
///
/// Double quotes rather than single ones because the format string is
/// AdGuard's own (contract §8), and re-quoting would mean showing a command
/// that is no longer the one upstream documents. So the rule is to refuse the
/// paths that cannot be shown safely rather than to rewrite them: the row falls
/// back to naming the state without a command, exactly as it does when the
/// installer is missing. A certificate is still detected, and still reported —
/// what is withheld is only the instruction.
///
/// Backslash is on the list because it escapes the closing quote, and the two
/// newline characters because a clipboard paste of a line containing one
/// submits it.
pub fn quotable(path: &Path) -> bool {
    !path
        .to_string_lossy()
        .contains(['"', '`', '$', '\\', '\n', '\r'])
}

/// The command that rebuilds the bundle from the anchor directory.
///
/// AdGuard's script runs both of these and needs only one to succeed, so this
/// picks whichever this machine actually has rather than guessing from the
/// anchor directory: a distribution may ship either, and an instruction naming
/// a command that is not installed is worse than no instruction.
pub fn refresh_command() -> String {
    let found = REFRESH_COMMANDS
        .iter()
        .find(|name| in_path(name))
        .unwrap_or(&REFRESH_COMMANDS[0]);
    format!("sudo {found}")
}

/// The first of AdGuard's four anchor directories that exists here.
///
/// `$SYSTEM_CERT_DIR` overrides the search, because `install_cert.sh` honours
/// exactly that variable for exactly that purpose — so a check pointed at a
/// throwaway directory reports on the same place the installer would write to.
pub fn anchor_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("SYSTEM_CERT_DIR") {
        let candidate = PathBuf::from(explicit);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    ANCHOR_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_dir())
}

/// The first known trusted bundle that exists here.
///
/// `$ADGUARD_CA_BUNDLE` overrides it. Unlike `$SYSTEM_CERT_DIR` this is not a
/// variable AdGuard honours — it is ours, and it exists because without it the
/// untrusted branches are unreachable on any machine where the certificate is
/// already installed. The reference machine is one: pointing the anchor
/// directory elsewhere still leaves the real bundle carrying the real CA, so
/// the check still says trusted and the rows still hide. The alternative to a
/// test override is removing a certificate from the system trust store to see
/// what the app does about it, which is the act this whole design exists to
/// leave to the user. `$ADGUARD_ROOT_HELPER` is there for the same reason.
pub fn bundle() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ADGUARD_CA_BUNDLE") {
        let candidate = PathBuf::from(explicit);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    BUNDLES
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
}

/// The name `install_cert.sh` gives the copy it installs: the certificate's
/// own file name with `.pem` replaced by `.crt`.
///
/// From the script — `CERT_NAME=$(basename "${CERT_PATH}" .pem)`, then
/// `SYSTEM_CERT_PATH="${SYSTEM_CERT_DIR}/${CERT_NAME}.crt"`. The extension is
/// not cosmetic: `update-ca-certificates` reads `*.crt` and nothing else.
fn anchor_name(certificate: &Path) -> String {
    let stem = certificate
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    format!("{stem}.crt")
}

/// A file's text, or empty when it cannot be read.
///
/// Lossy on purpose. A DER-encoded certificate — which is what AdGuard's own
/// `SSL/<name>.cer` copy is — is not UTF-8, and the alternative to lossy
/// decoding is a hard error for a file that simply holds no PEM. Either way
/// [`bodies`] finds nothing in it, which is the honest answer: this comparison
/// is defined on PEM, and a bundle is always PEM.
fn read(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// The interior of every PEM certificate block in `text`, in order, with its
/// whitespace still in it.
///
/// Comparing block interiors rather than files is what makes this work at all:
/// the same certificate is one file on its own and one block among 123 in the
/// bundle, with different line endings, a different trailing newline — the
/// bundle script appends one if it is missing — and no name attached to either.
///
/// Only `CERTIFICATE` blocks. A `.pem` beside a private key is a shape AdGuard
/// does not produce, but reading a `PRIVATE KEY` body into a comparison of
/// certificates would be a bug waiting for the install that does. An unclosed
/// block ends the walk rather than yielding the rest of the file, which is what
/// a truncated bundle looks like.
fn blocks(text: &str) -> impl Iterator<Item = &str> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let mut rest = text;
    std::iter::from_fn(move || {
        let start = rest.find(BEGIN)?;
        let after = &rest[start + BEGIN.len()..];
        let stop = after.find(END)?;
        let body = &after[..stop];
        rest = &after[stop + END.len()..];
        Some(body)
    })
}

/// The base64 body of every PEM certificate in `text`, whitespace removed.
fn bodies(text: &str) -> Vec<String> {
    blocks(text)
        .map(|block| block.split_whitespace().collect::<String>())
        .filter(|body| !body.is_empty())
        .collect()
}

/// Whether `body` — already normalised by [`bodies`] — is one of the
/// certificates in `text`.
///
/// Deliberately not `bodies(text).contains(body)`. This is the bundle read, it
/// runs on the GTK main loop every time the window regains focus, and the
/// bundle is 200 KB holding 123 certificates: collecting all of them to find
/// one costs 123 allocations per check, and it was **almost the entire cost of
/// the check**. Measured, debug build, ten calls: 4.84 ms each with `bodies`,
/// 0.52 ms each with this. Comparing character by character through the
/// whitespace allocates nothing.
fn contains(text: &str, body: &str) -> bool {
    !body.is_empty()
        && blocks(text).any(|block| {
            block
                .split_whitespace()
                .flat_map(str::chars)
                .eq(body.chars())
        })
}

/// Whether a bare command name resolves on `$PATH`. No execution.
fn in_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A throwaway directory of its own per test, so two tests cannot see each
    /// other's anchor files. Removed and recreated rather than merely created:
    /// a previous run's leftovers are exactly the stale state some of these
    /// tests are about, and inheriting it silently would make them pass for the
    /// wrong reason.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("adguard-ui-trust-test").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// Two structurally valid, definitely different certificates. The bodies do
    /// not have to decode — every comparison here is on the base64 text — and
    /// generating real ones would put an X.509 library in a crate that needs
    /// none.
    const ONE: &str = "-----BEGIN CERTIFICATE-----\nQUJDREVGRw==\n-----END CERTIFICATE-----\n";
    const TWO: &str = "-----BEGIN CERTIFICATE-----\nSElKS0xNTk8=\n-----END CERTIFICATE-----\n";

    /// The whole point of comparing bodies: the same certificate wrapped
    /// differently is the same certificate. Line breaks moved, indentation
    /// added, trailing newline gone.
    #[test]
    fn the_same_body_survives_being_rewrapped() {
        let wrapped = "-----BEGIN CERTIFICATE-----\nQUJD\n  REVG\nRw==\n-----END CERTIFICATE-----";
        assert_eq!(bodies(ONE), bodies(wrapped));
        assert_eq!(bodies(ONE), vec!["QUJDREVGRw==".to_string()]);
    }

    /// A bundle is many certificates in one file and the answer is membership,
    /// not equality — the trap that makes "the files differ" useless here.
    #[test]
    fn a_bundle_yields_every_body_in_it() {
        let bundle = format!("{ONE}{TWO}");
        assert_eq!(bodies(&bundle).len(), 2);
        assert_eq!(bodies(&bundle)[1], bodies(TWO)[0]);
    }

    /// Anything that is not a certificate block yields nothing, including a
    /// private key — which shares the file format and would otherwise compare
    /// equal to itself across two files and read as trust.
    #[test]
    fn nothing_but_certificate_blocks_is_extracted() {
        assert!(bodies("").is_empty());
        assert!(bodies("not a certificate at all").is_empty());
        assert!(bodies("-----BEGIN PRIVATE KEY-----\nQUJD\n-----END PRIVATE KEY-----").is_empty());
        // A block that was never closed. Truncation is what a half-written
        // bundle looks like, and taking the rest of the file as a body would
        // compare a fragment against a whole certificate.
        assert!(bodies("-----BEGIN CERTIFICATE-----\nQUJD\n").is_empty());
    }

    /// The reference machine's own state, and the one branch that needs no
    /// fabrication: generated, anchored, bundled.
    #[test]
    fn a_certificate_in_both_places_is_trusted() {
        let dir = scratch("trusted");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, ONE).expect("write the certificate");
        let anchors = dir.join("anchors");
        fs::create_dir_all(&anchors).expect("create the anchor directory");
        fs::write(anchors.join("Test CA.crt"), ONE).expect("write the anchor");
        let bundle = dir.join("bundle.crt");
        fs::write(&bundle, format!("{TWO}{ONE}")).expect("write the bundle");

        let trust = CaTrust::inspect(&certificate, Some(&anchors), Some(&bundle));
        assert!(trust.generated, "{trust:?}");
        assert!(trust.anchored, "{trust:?}");
        assert!(!trust.stale, "{trust:?}");
        assert!(trust.bundled, "{trust:?}");
        assert!(trust.is_trusted());
        assert!(trust.unmet().is_empty());
    }

    /// The state every install this app sets up ends in: the CA exists,
    /// nothing has installed it (contract §7).
    #[test]
    fn a_generated_certificate_alone_is_not_trusted() {
        let dir = scratch("generated-only");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, ONE).expect("write the certificate");
        let anchors = dir.join("anchors");
        fs::create_dir_all(&anchors).expect("create the anchor directory");
        let bundle = dir.join("bundle.crt");
        fs::write(&bundle, TWO).expect("write the bundle");

        let trust = CaTrust::inspect(&certificate, Some(&anchors), Some(&bundle));
        assert!(trust.generated, "{trust:?}");
        assert!(!trust.anchored, "{trust:?}");
        assert!(!trust.stale, "{trust:?}");
        assert!(!trust.bundled, "{trust:?}");
        assert!(!trust.is_trusted());
        assert_eq!(
            trust.unmet(),
            vec!["it has not been installed into the system trust store"]
        );
    }

    /// Installed but not rebuilt — the state that makes an existence check on
    /// the anchor file wrong, because nothing reads that directory until
    /// `update-ca-certificates` has.
    #[test]
    fn an_anchor_without_a_bundle_entry_is_not_trusted() {
        let dir = scratch("anchored-only");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, ONE).expect("write the certificate");
        let anchors = dir.join("anchors");
        fs::create_dir_all(&anchors).expect("create the anchor directory");
        fs::write(anchors.join("Test CA.crt"), ONE).expect("write the anchor");
        let bundle = dir.join("bundle.crt");
        fs::write(&bundle, TWO).expect("write the bundle");

        let trust = CaTrust::inspect(&certificate, Some(&anchors), Some(&bundle));
        assert!(trust.anchored, "{trust:?}");
        assert!(!trust.bundled, "{trust:?}");
        assert!(!trust.is_trusted());
        assert_eq!(
            trust.unmet(),
            vec!["the trust store has not been rebuilt since it was installed"]
        );
    }

    /// The state AdGuard's own installer will not repair: the right name, the
    /// wrong certificate. Its check is `[ ! -f "${SYSTEM_CERT_PATH}" ]`, so it
    /// prints "Certificate already exists in system trust store" and stops.
    #[test]
    fn a_different_certificate_at_the_anchor_path_is_stale_not_installed() {
        let dir = scratch("stale");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, ONE).expect("write the certificate");
        let anchors = dir.join("anchors");
        fs::create_dir_all(&anchors).expect("create the anchor directory");
        fs::write(anchors.join("Test CA.crt"), TWO).expect("write the old anchor");
        let bundle = dir.join("bundle.crt");
        fs::write(&bundle, TWO).expect("write the bundle");

        let trust = CaTrust::inspect(&certificate, Some(&anchors), Some(&bundle));
        assert!(trust.generated, "{trust:?}");
        assert!(!trust.anchored, "{trust:?}");
        assert!(trust.stale, "{trust:?}");
        // The *old* certificate is the one in the bundle, which is exactly how
        // this state hides: everything looks installed except the comparison
        // that matters.
        assert!(!trust.bundled, "{trust:?}");
        assert_eq!(
            trust.unmet(),
            vec!["a different certificate of the same name is already installed"]
        );
    }

    /// No certificate at all — a data directory that has never been configured,
    /// or a `root_certificate_name` that was changed after generation. Not an
    /// error: the path is what the report needs, and it is kept.
    #[test]
    fn an_absent_certificate_is_reported_with_the_path_it_looked_for() {
        let dir = scratch("absent");
        let certificate = dir.join("Test CA.pem");
        let anchors = dir.join("anchors");
        fs::create_dir_all(&anchors).expect("create the anchor directory");

        let trust = CaTrust::inspect(&certificate, Some(&anchors), None);
        assert!(!trust.generated, "{trust:?}");
        assert!(!trust.anchored && !trust.stale, "{trust:?}");
        assert!(!trust.is_trusted());
        assert_eq!(trust.certificate, certificate);
        assert_eq!(trust.unmet(), vec!["no certificate has been generated"]);
    }

    /// A file that exists but holds no certificate reads as *not generated*
    /// rather than as generated-and-untrusted. There would be nothing to
    /// compare with, so every later property would be false for a reason the
    /// row could not explain.
    #[test]
    fn an_unparseable_certificate_reads_as_not_generated() {
        let dir = scratch("unparseable");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, b"\x30\x82\x01\x0a not pem at all").expect("write the file");

        let trust = CaTrust::inspect(&certificate, None, None);
        assert!(!trust.generated, "{trust:?}");
        assert_eq!(trust.unmet(), vec!["no certificate has been generated"]);
    }

    /// A machine with none of the four anchor directories, or none of the known
    /// bundles: the paths are `None` rather than guessed, and nothing claims
    /// the certificate is untrusted *because* of a directory this project
    /// failed to find.
    #[test]
    fn absent_system_locations_stay_absent() {
        let dir = scratch("no-locations");
        let certificate = dir.join("Test CA.pem");
        fs::write(&certificate, ONE).expect("write the certificate");

        let trust = CaTrust::inspect(&certificate, None, None);
        assert!(trust.generated, "{trust:?}");
        assert_eq!(trust.anchor, None);
        assert_eq!(trust.bundle, None);
        assert!(!trust.is_trusted());
    }

    /// The anchor is `.crt`, whatever the certificate is called — the rule
    /// `update-ca-certificates` enforces by only reading that extension.
    #[test]
    fn the_anchor_is_the_certificates_name_with_a_crt_extension() {
        assert_eq!(anchor_name(Path::new("/x/AdGuard CLI CA.pem")), "AdGuard CLI CA.crt");
        assert_eq!(anchor_name(Path::new("/x/other.pem")), "other.crt");
    }

    /// AdGuard's own format string, quotes included, because the stock
    /// certificate name has spaces in it.
    #[test]
    fn the_install_command_quotes_both_paths() {
        let command = install_command(
            Path::new("/home/someone/.local/opt/adguard-cli/install_cert.sh"),
            Path::new("/home/someone/.local/share/adguard-cli/AdGuard CLI CA.pem"),
        );
        assert_eq!(
            command.as_deref(),
            Some(
                "\"/home/someone/.local/opt/adguard-cli/install_cert.sh\" \
                 -c \"/home/someone/.local/share/adguard-cli/AdGuard CLI CA.pem\""
            )
        );
    }

    /// A certificate name that would break out of AdGuard's quoting yields no
    /// command at all.
    ///
    /// `config set https_filtering.root_certificate_name` takes any string, and
    /// the row that would carry the result is one the user is invited to paste
    /// into a shell behind a `sudo`. Refused rather than re-quoted: the command
    /// is upstream's, and a version of it that is not upstream's would be a
    /// different claim than the one the row makes.
    #[test]
    fn a_certificate_name_that_escapes_the_quoting_yields_no_command() {
        let installer = Path::new("/opt/adguard-cli/install_cert.sh");
        let hostile = [
            "/data/x\" ; rm -rf ~ ; echo \".pem",
            "/data/$(id).pem",
            "/data/`id`.pem",
            "/data/x\\\".pem",
            "/data/one\nsudo rm -rf ~\n.pem",
        ];
        for name in hostile {
            assert!(!quotable(Path::new(name)), "{name}");
            assert_eq!(install_command(installer, Path::new(name)), None, "{name}");
        }

        // Spaces, brackets, apostrophes and non-ASCII stay perfectly shippable
        // — the check must not refuse an ordinary name to feel safe.
        for name in [
            "/data/AdGuard CLI CA.pem",
            "/data/AdGuard (work).pem",
            "/data/Zertifikat für AdGuard.pem",
            "/data/it's mine.pem",
        ] {
            assert!(quotable(Path::new(name)), "{name}");
            assert!(install_command(installer, Path::new(name)).is_some(), "{name}");
        }
    }

    /// The installer's own path is checked too. It is ours to locate, but it is
    /// found by joining onto `$ADGUARD_CLI`, which is an environment variable.
    #[test]
    fn an_unquotable_installer_path_yields_no_command_either() {
        assert_eq!(
            install_command(
                Path::new("/opt/`id`/install_cert.sh"),
                Path::new("/data/AdGuard CLI CA.pem")
            ),
            None
        );
    }

    /// Whichever of the two rebuild commands this machine has. Both absent is
    /// not a case worth inventing an answer for — the first is what Debian and
    /// Ubuntu ship and what the reference machine has.
    #[test]
    fn the_refresh_command_names_a_program_that_is_here() {
        let command = refresh_command();
        assert!(command.starts_with("sudo update-ca-"), "{command}");
        let named = command.trim_start_matches("sudo ");
        if in_path(REFRESH_COMMANDS[0]) || in_path(REFRESH_COMMANDS[1]) {
            assert!(in_path(named), "{command} names something not on PATH");
        }
    }

    /// What the check costs, because it runs on the GTK main loop whenever the
    /// window regains focus and the largest of its three reads is the system
    /// bundle — 200 KB and 123 certificates on the reference machine, against
    /// the root helper's single `stat`.
    ///
    /// An upper bound with a wide margin, as every timing assertion in this
    /// project is: a loaded machine must not fail it, and the number it is
    /// guarding against is a hundred times larger than the measurement. Run it
    /// with `--nocapture` to see the real figure.
    #[test]
    fn the_check_is_cheap_enough_for_the_main_loop() {
        let Some(_) = CaTrust::detect(DEFAULT_CERTIFICATE_NAME) else {
            eprintln!("skipping: AdGuard's data directory could not be located");
            return;
        };
        // Ten, so a single unlucky read cannot decide it either way.
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let _ = CaTrust::detect(DEFAULT_CERTIFICATE_NAME);
        }
        let each = started.elapsed() / 10;
        eprintln!("CaTrust::detect: {each:?} per call");
        assert!(each < std::time::Duration::from_millis(50), "{each:?}");
    }

    /// The reading this machine actually gives. Not an assertion that the CA is
    /// trusted here — that is the user's business and may change — but that the
    /// check resolves real locations and produces a coherent answer, so an
    /// AdGuard upgrade that moves the certificate is noticed here rather than
    /// in the UI. Skips when AdGuard is not installed.
    #[test]
    fn the_real_check_resolves_this_machines_locations() {
        let Some(trust) = CaTrust::detect(DEFAULT_CERTIFICATE_NAME) else {
            eprintln!("skipping: AdGuard's data directory could not be located");
            return;
        };
        assert!(
            trust.certificate.ends_with("AdGuard CLI CA.pem"),
            "{:?}",
            trust.certificate
        );
        if !trust.certificate.is_file() {
            eprintln!("skipping the rest: this machine has no AdGuard certificate");
            return;
        }
        assert!(trust.generated, "{trust:?}");
        // Captured, not asserted. `--nocapture` is how this machine's own
        // reading gets into a report without a probe of its own, and the state
        // it prints is the user's business rather than this test's.
        eprintln!("{trust:?}");
        // Whatever the trust state, the two readings have to agree: `bundled`
        // is the only thing `is_trusted` consults, and a mismatch would mean
        // the summary and the detail could disagree on a row.
        assert_eq!(trust.is_trusted(), trust.bundled, "{trust:?}");
        assert!(!(trust.anchored && trust.stale), "{trust:?}");
    }
}
