//! `filters install` against a **throwaway** data directory.
//!
//! Every case here installs a file this test wrote, through the `file://` leg
//! the CLI accepts on the same positional as a URL — so the suite is hermetic:
//! it reaches no network, depends on no third party's list staying up, and
//! cannot be broken by one going down. The network leg itself is not testable
//! this way and is recorded in `docs/cli-contract.md` §6 instead, where the
//! measurement includes the CLI's own 60-second deadline.
//!
//! ```text
//! cargo test -p adguard-core --test filters_sandbox -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d like every suite that shells out — it needs `adguard-cli`, and
//! a licence, since `filters install` is licence-gated. It never touches the
//! machine's own catalogue, and [`the_machine_catalogue_was_not_touched`]
//! asserts that rather than leaving it to be believed.

use std::path::{Path, PathBuf};

use adguard_core::filters::Catalogue;
use adguard_core::{Cli, Filter, FilterAction, FilterSet, Locale};

/// A scratch `$XDG_DATA_HOME` with the machine's licence lent to it.
///
/// Unlike `config_sandbox.rs` this needs no `proxy.yaml`: `filters install`
/// writes only to `agflm_standard.db`, which the CLI creates for itself on
/// first use. The licence is the one thing that has to be borrowed, because
/// the command is gated on it — measured, an unlicensed install refuses with
/// exit 1 and the usual complaint on stderr.
struct Sandbox {
    root: PathBuf,
    cli: Cli,
    locale: Locale,
}

impl Sandbox {
    /// `None` — with a printed reason — whenever the machine cannot host the
    /// test, so this skips rather than fails on a box without AdGuard.
    fn new(name: &str) -> Option<Self> {
        let cli = match Cli::discover() {
            Ok(cli) => cli,
            Err(err) => {
                eprintln!("skipping: {err}");
                return None;
            }
        };

        // One directory per test: the tests in this binary run concurrently,
        // and two `adguard-cli` invocations racing each other's initialisation
        // of the *same* fresh directory is a measured failure (contract §3).
        let root =
            std::env::temp_dir().join(format!("adguard-ui-filters-{name}-{}", std::process::id()));
        let data = root.join("adguard-cli");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&data).expect("create sandbox");

        let licence = adguard_core::paths::data_dir()?.join("adguard.conf");
        if !licence.is_file() {
            eprintln!("skipping: no licence on this machine to lend the sandbox");
            return None;
        }
        std::fs::copy(&licence, data.join("adguard.conf")).expect("lend the licence");

        let sandbox = Self {
            cli: cli.with_xdg_data_home(&root),
            root,
            locale: Locale::from_env(),
        };

        // Also the invocation that creates the databases everything below
        // reads. Proving the licence took beats assuming the copy sufficed.
        if sandbox.cli.license().is_err() {
            eprintln!("skipping: the sandbox did not come up licensed");
            return None;
        }
        Some(sandbox)
    }

    fn db(&self) -> PathBuf {
        self.root.join("adguard-cli").join("agflm_standard.db")
    }

    /// The custom rows, read straight from the sandbox's database.
    ///
    /// By explicit path, not [`Catalogue::open_set`]: that resolves
    /// `$XDG_DATA_HOME` from *this* process's environment, and the sandbox sets
    /// it on the child only — so `open_set` here would read, and these tests
    /// would assert against, the machine's real catalogue.
    fn customs(&self) -> Vec<Filter> {
        Catalogue::open(&self.db())
            .expect("sandbox catalogue should open")
            .custom_filters(&self.locale)
            .expect("custom filters should read")
    }

    /// Write a list into the sandbox and return the path to hand the CLI.
    fn list(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, body).expect("write list");
        path
    }

    fn install(&self, path: &Path) -> Result<(), adguard_core::Error> {
        self.cli
            .filters_install(FilterSet::Http, &path.display().to_string())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const TITLED: &str = "! Title: Sandbox Probe List\n\
     ! Description: Written by filters_sandbox.rs\n\
     ! Homepage: https://example.org/probe\n\
     ||probe-one.example^\n";

/// The happy path, decided by the database rather than by what was printed.
///
/// `filters install` reports success in a shape [`Cli::filters_install`] does
/// check — but a confirmation is never the evidence in this codebase, and here
/// there is not even an id to re-read: AdGuard assigns it. The new row is what
/// proves it, which is the whole reason `Catalogue::custom_filters` exists.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn installing_a_list_creates_a_custom_row() {
    let Some(sandbox) = Sandbox::new("install") else {
        return;
    };
    assert!(sandbox.customs().is_empty(), "a fresh sandbox has no custom filters");

    let list = sandbox.list("titled.txt", TITLED);
    sandbox.install(&list).expect("installing a readable list should be accepted");

    let customs = sandbox.customs();
    assert_eq!(customs.len(), 1, "expected exactly one custom row, got {customs:?}");
    let installed = &customs[0];
    eprintln!("installed -> {installed:?}");

    assert_eq!(installed.name, "Sandbox Probe List", "the `! Title:` header names the row");
    assert!(installed.is_custom(), "must land in the Custom filters group");
    assert!(installed.enabled && installed.installed, "install both adds and enables");
    assert!(!installed.trusted, "nothing here passes --trusted");
    assert!(
        installed.download_url.starts_with("file://"),
        "a local path comes back normalised, got {:?}",
        installed.download_url
    );

    // Negative ids, and not the user-rules sentinel — a range test on the sign
    // alone would confuse the two.
    assert!(installed.id < 0 && installed.id != Filter::USER_RULES_ID);
}

/// A list with no `! Title:` header stores an **empty** title, while the CLI's
/// own confirmation claims the URL as one.
///
/// This is the measurement `Filter::display_name` exists for. Without it the
/// row renders with no name at all: custom filters have no `filter_localisation`
/// rows, so the catalogue's whole `COALESCE` chain resolves to `''`.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn an_untitled_list_has_no_name_to_render() {
    let Some(sandbox) = Sandbox::new("untitled") else {
        return;
    };

    let list = sandbox.list("untitled.txt", "||probe-two.example^\n");
    sandbox.install(&list).expect("a list without headers still installs");

    let installed = sandbox.customs().pop().expect("one custom row");
    eprintln!("untitled -> name={:?} title={:?}", installed.name, installed.title);

    assert!(installed.title.is_empty(), "expected an empty title, got {:?}", installed.title);
    assert!(installed.name.is_empty(), "the localisation fallback has nothing to find either");
    assert_eq!(
        installed.display_name(),
        installed.download_url,
        "the URL is the only thing left to name the row with"
    );
}

/// The only thing AdGuard checks about the content is whether it *starts* with
/// HTML — and this pins the boundary from both sides.
///
/// It catches the single likeliest accident, a link that answers 200 with a
/// friendly error page. It catches nothing else: a file of JSON, of prose, or
/// of nothing at all installs as a filter list holding no rules and reports
/// success, leaving a switch reading *on* over a filter that filters nothing.
///
/// Both halves are asserted because an earlier revision of the contract claimed
/// content was never validated at all, generalised from one probe file that
/// happened to open with a line of text before its HTML — which is exactly the
/// case `text_then_html` reproduces.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn html_is_the_one_thing_rejected() {
    let Some(sandbox) = Sandbox::new("content") else {
        return;
    };

    for (name, body) in [
        ("html.txt", "<html><body>404 Not Found</body></html>\n"),
        ("doctype.txt", "<!DOCTYPE html>\n<html></html>\n"),
        ("indented.txt", "   <html>x</html>\n"),
    ] {
        let list = sandbox.list(name, body);
        let err = sandbox
            .install(&list)
            .expect_err("an HTML document must not install as a filter list");
        eprintln!("{name} -> {err}");
    }

    // ...and everything that is not an HTML *document* sails through, however
    // little it resembles a filter list.
    for (name, body) in [
        ("text-then-html.txt", "not a filter list\n<html><body>x</body></html>\n"),
        ("json.txt", "{\"json\": true}\n"),
        ("prose.txt", "this is not a filter list at all\n"),
        ("empty.txt", ""),
    ] {
        let list = sandbox.list(name, body);
        sandbox
            .install(&list)
            .unwrap_or_else(|err| panic!("{name} should have installed: {err}"));
    }

    assert_eq!(sandbox.customs().len(), 4, "the four non-HTML bodies were accepted");
}

/// Re-installing a URL already present is **refused** — the opposite of
/// `config list-add`, which appends a silent duplicate (contract §5).
///
/// So this one command is safe to issue speculatively, and the refusal carries
/// the CLI's own sentence without the `filters list` table that follows it.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn installing_the_same_url_twice_is_refused() {
    let Some(sandbox) = Sandbox::new("duplicate") else {
        return;
    };

    let list = sandbox.list("titled.txt", TITLED);
    sandbox.install(&list).expect("the first install is accepted");

    let err = sandbox.install(&list).expect_err("the second must be refused");
    eprintln!("duplicate -> {err}");
    assert!(
        matches!(err, adguard_core::Error::Refused { .. }),
        "expected a Refused, got {err:?}"
    );
    assert!(
        err.to_string().contains("already exists"),
        "should carry the CLI's own sentence: {err}"
    );
    assert!(
        !err.to_string().contains('|'),
        "the trailing `filters list` table must be dropped, not shown: {err}"
    );
    assert_eq!(sandbox.customs().len(), 1, "and nothing was added the second time");
}

/// Everything that can go wrong fetching a list produces one sentence.
///
/// A missing file is the case reachable without a network; a 404, a refused
/// connection and an unresolvable host are measured to be identical to it
/// (contract §6). Since they cannot be told apart, the UI must not claim to
/// know which happened — this pins the refusal, not a diagnosis.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_list_that_cannot_be_read_is_refused() {
    let Some(sandbox) = Sandbox::new("missing") else {
        return;
    };

    let err = sandbox
        .install(&sandbox.root.join("does-not-exist.txt"))
        .expect_err("a missing file must not read as installed");
    eprintln!("missing -> {err}");
    assert!(
        err.to_string().contains("Failed to install the filter"),
        "should carry the CLI's own sentence: {err}"
    );
    assert!(sandbox.customs().is_empty(), "nothing was installed");
}

/// A custom filter's switch goes through the same path as a catalogue one.
///
/// Worth its own test because the ids are negative: `filters disable -10001`
/// relies on CLI11 reading it as a positional rather than a flag, exactly as
/// the user-rules sentinel does, and nothing in the wrapper adds a `--` guard
/// for it.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn a_custom_filter_can_be_switched_off_and_back_on() {
    let Some(sandbox) = Sandbox::new("toggle") else {
        return;
    };

    let list = sandbox.list("titled.txt", TITLED);
    sandbox.install(&list).expect("install");
    let id = sandbox.customs()[0].id;

    sandbox
        .cli
        .filter_action(FilterSet::Http, FilterAction::Disable, id)
        .unwrap_or_else(|err| panic!("disabling {id} refused: {err}"));
    assert!(!sandbox.customs()[0].enabled, "the database should show it off");

    sandbox
        .cli
        .filter_action(FilterSet::Http, FilterAction::Enable, id)
        .unwrap_or_else(|err| panic!("enabling {id} refused: {err}"));
    assert!(sandbox.customs()[0].enabled, "and on again");
}

/// The safety assertion this suite owes the machine it runs on.
///
/// Mirrors `config_sandbox::the_machine_config_was_not_touched`. A sandbox that
/// silently leaked into the real data directory would show up as custom filters
/// nobody installed — and since [`Catalogue::open_set`] resolves
/// `$XDG_DATA_HOME` from this process rather than the child's, that leak is one
/// mistaken call away rather than hypothetical.
#[test]
#[ignore = "invokes the real adguard-cli"]
fn the_machine_catalogue_was_not_touched() {
    let Some(db) = FilterSet::Http.db_path() else {
        eprintln!("skipping: no filter database on this machine");
        return;
    };
    let Ok(catalogue) = Catalogue::open(&db) else {
        eprintln!("skipping: {} not readable", db.display());
        return;
    };
    let before = catalogue
        .custom_filters(&Locale::from_env())
        .expect("read the machine's custom filters");

    {
        let Some(sandbox) = Sandbox::new("isolation") else {
            return;
        };
        let list = sandbox.list("titled.txt", TITLED);
        sandbox.install(&list).expect("install into the sandbox");
        assert_eq!(sandbox.customs().len(), 1);
    }

    let after = Catalogue::open(&db)
        .expect("reopen the machine's catalogue")
        .custom_filters(&Locale::from_env())
        .expect("re-read the machine's custom filters");
    assert_eq!(before, after, "the sandbox leaked into the machine's own catalogue");
}
