//! The About page: two version numbers, and the one control that reaches
//! AdGuard's servers on purpose.
//!
//! Built for [issue #4](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/4),
//! which asked for a way to check for updates from the main window. An About
//! page is where it belongs rather than a header-bar button, because the action
//! is *about this installation* — and because the app has never had anywhere to
//! show its own version or the CLI's. `Cli::version` has existed since v1.0 and
//! nothing had ever called it.
//!
//! # Everything on this page is shaped by contract §14
//!
//! **The command is misnamed and the page may not repeat the mistake.**
//! `check-update` does not check: it *downloads* filters, DNS filters,
//! userscripts, Safe Browsing and CRLite, and only the application is checked.
//! So the button says *Update now* and the group says what that means. A control
//! labelled "Check for updates" would misdescribe five sixths of what it does.
//!
//! **Nothing is counted.** Safe Browsing and CRLite answer `Updated` on every
//! run of a working install — seven times out of seven, measured — so a summary
//! like "2 of 6 updated" would render identically forever while appearing to
//! report on the user's machine. The six verdicts are shown and never added up.
//!
//! **A component failure is an ordinary outcome, not an error state.** Five of
//! fourteen measured runs carried one and every one cleared on the next run. So
//! a failure is a row with a retry beside it, and the page does not otherwise
//! change character.
//!
//! **The application half is reported and never applied.** `adguard-cli update`
//! re-runs an installer over a suid root helper, and this application performs
//! no privileged operation of its own (`architecture.md` §1, §6) — it detects
//! and instructs, as it already does for that helper and for the certificate.
//! The command is named; running it is the user's.
//!
//! # What this page deliberately does not do
//!
//! **The Status page's poll is left running.** The activation path stands it
//! down, and this was built expecting to do the same — but measured against a
//! live daemon, eight `status` calls during an in-flight `check-update` came
//! back in 0.03 s each. There is nothing to hold. What *is* guarded is the
//! affordance: the button desensitises itself, which is what makes a duplicate
//! check impossible rather than merely discouraged.
//!
//! **Refreshing the page does not run the command.** [`Self::reload`] is what
//! the header-bar refresh button calls on whichever page is showing, and on
//! every other page that is a cheap re-read. Here the equivalent would be a
//! network operation with side effects on the user's filters, fired by a button
//! that means "show me what is true now" everywhere else in the app.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adguard_core::{Catalogue, Cli, FilterSet, UpdateReport, Verdict};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{abbreviate, toast, worker};

/// Shown in a value row before its reading has arrived, as on the Status page.
const PLACEHOLDER: &str = "—";

/// The button, in its two states.
const UPDATE: &str = "Update now";
const WORKING: &str = "Updating…";

/// What the update actually does, in the one place the user meets it.
///
/// Says "downloads" first and "checks" last, in that order, because that is the
/// proportion: five components are fetched and one is asked about.
const WHAT_IT_DOES: &str =
    "Downloads the current filter lists, DNS filter lists, userscripts, Safe Browsing data \
     and certificate revocation data, then asks whether a newer AdGuard CLI has been \
     released. AdGuard does this on its own as well — this is for when you would rather \
     not wait.";

/// Why there is no button to install an application update.
const WHY_NO_BUTTON: &str =
    "Updating AdGuard replaces its privileged helper, and this application never performs \
     privileged operations — so this one is yours to run.";

/// Beside the "last changed" figure, and load-bearing rather than decorative:
/// the column behind that figure moves only when data arrives, so a long gap is
/// the *absence of upstream revisions* and not the absence of checking.
const WHAT_A_GAP_MEANS: &str =
    "AdGuard refreshes on its own every few hours. This is when something last \
     changed, so a long gap means the lists have not been revised — not that \
     nothing has looked.";

/// The command that does it. Named, never invoked (contract §14).
const UPDATE_COMMAND: &str = "adguard-cli update";

const HOMEPAGE: &str = "https://github.com/dominik-najberg/AdGuard-UI-Linux";
const ISSUES: &str = "https://github.com/dominik-najberg/AdGuard-UI-Linux/issues";

pub struct AboutPage {
    page: adw::PreferencesPage,
    cli: Cli,
    toasts: adw::ToastOverlay,

    /// The CLI's own version banner, read off the main thread like everything
    /// else it says.
    cli_version: adw::ActionRow,

    /// When new filter data last arrived, from the catalogues themselves.
    ///
    /// The answer to the question a check-on-launch would have been built to
    /// answer, at the cost of two SQLite reads instead of a network fetch — and
    /// a better answer, because a silent launch check tells the user nothing by
    /// construction. See [`freshness`] for why it says *changed*.
    ///
    /// [`freshness`]: Self::read_freshness
    freshness: gtk::Label,

    check: gtk::Button,
    /// A check is in flight. The button is insensitive for the same span, and
    /// this is the half that cannot be got around by a second code path.
    busy: Cell<bool>,

    /// One row per component, rebuilt from each report. Empty and hidden until
    /// the first run — there is nothing truthful to show before then.
    results: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,

    /// AdGuard's word about the application, shown only when it has one.
    notice: adw::PreferencesGroup,
    notice_row: adw::ActionRow,

    /// Notified after every successful run, so the window can re-read the
    /// catalogues that moved.
    ///
    /// The shape `StatusPage::connect_navigate` uses, and for the same reason:
    /// this page knows what changed and nothing else, while which page renders a
    /// catalogue is `main_view`'s business. It cannot hold those pages itself —
    /// it is built alongside them and the sidebar already holds it.
    checked: RefCell<Option<Box<dyn Fn(&UpdateReport)>>>,
}

impl AboutPage {
    pub fn new(cli: Cli, toasts: adw::ToastOverlay) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        // ---- versions ----

        let versions = adw::PreferencesGroup::builder().title("Versions").build();
        let app_version = row("AdGuard UI", env!("CARGO_PKG_VERSION"));
        let cli_version = row("AdGuard CLI", PLACEHOLDER);
        let binary = row("AdGuard CLI binary", &abbreviate(cli.binary()));
        binary.set_subtitle_lines(2);
        for r in [&app_version, &cli_version, &binary] {
            versions.add(r);
        }
        page.add(&versions);

        // ---- the update control ----

        let updates = adw::PreferencesGroup::builder()
            .title("Updates")
            .description(WHAT_IT_DOES)
            .build();

        // Above the button, because it is the fact you read in order to decide
        // whether to press it.
        //
        // The subtitle is not filler. This column moves only when data actually
        // arrives (contract §14), so a gap of days means *nothing has changed
        // upstream* — and without the sentence saying so, "8 days ago" reads as
        // an install that has stopped working and sends the user to press a
        // button that will report `Up to date`.
        let freshness_row = row("Filter data last changed", WHAT_A_GAP_MEANS);
        freshness_row.set_subtitle_lines(3);
        let freshness = gtk::Label::builder()
            .label(PLACEHOLDER)
            .valign(gtk::Align::Center)
            .build();
        freshness.add_css_class("dim-label");
        freshness_row.add_suffix(&freshness);
        updates.add(&freshness_row);

        let action = adw::ActionRow::builder()
            .title("Filters and protection data")
            .subtitle("Filters, DNS filters, userscripts, Safe Browsing, certificate revocation")
            .build();
        action.set_use_markup(false);
        action.set_subtitle_lines(2);
        let check = gtk::Button::builder()
            .label(UPDATE)
            .valign(gtk::Align::Center)
            .build();
        check.add_css_class("suggested-action");
        action.add_suffix(&check);
        action.set_activatable_widget(Some(&check));
        updates.add(&action);
        page.add(&updates);

        // Built empty and hidden: before a run there is no honest thing for it
        // to say, and a group of six "unknown" rows would be six claims.
        let results = adw::PreferencesGroup::new();
        results.set_visible(false);
        page.add(&results);

        // ---- the application update, if AdGuard mentions one ----

        let notice = adw::PreferencesGroup::builder()
            .title("AdGuard CLI update")
            .description(WHY_NO_BUTTON)
            .build();
        notice.set_visible(false);
        let notice_row = row("AdGuard reports", PLACEHOLDER);
        notice_row.set_subtitle_lines(3);
        notice_row.set_subtitle_selectable(true);
        let command = row("To update, run", UPDATE_COMMAND);
        command.set_subtitle_selectable(true);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy the command"));
        copy.set_valign(gtk::Align::Center);
        copy.add_css_class("flat");
        copy.connect_clicked({
            let toasts = toasts.clone();
            move |button| {
                button.clipboard().set_text(UPDATE_COMMAND);
                toasts.add_toast(toast("Command copied"));
            }
        });
        command.add_suffix(&copy);
        notice.add(&notice_row);
        notice.add(&command);
        page.add(&notice);

        // ---- the project ----

        // "Project", not "AdGuard UI": the Versions group above already has a
        // row by that name, and two headings apart they would read as the same
        // thing said twice.
        let project = adw::PreferencesGroup::builder().title("Project").build();
        project.add(&link_row("Project page", HOMEPAGE));
        project.add(&link_row("Report an issue", ISSUES));
        project.add(&row("Licence", "GPL-3.0-or-later"));
        page.add(&project);

        let this = Rc::new(Self {
            page,
            cli,
            toasts,
            cli_version,
            freshness,
            check: check.clone(),
            busy: Cell::new(false),
            results,
            rows: RefCell::new(Vec::new()),
            notice,
            notice_row,
            checked: RefCell::new(None),
        });

        check.connect_clicked({
            let this = this.clone();
            move |_| this.check_now()
        });

        this.read_version();
        this.read_freshness();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    /// Re-read what this page can read cheaply — which is the version, and not
    /// the update state.
    ///
    /// The header-bar refresh button lands here. It means "show me what is true
    /// now" on every page, and on this one the update rows are a record of a
    /// command that ran at a particular moment rather than a reading of anything
    /// — so refreshing leaves them alone. Re-running the command would be a
    /// network fetch with side effects on the user's filters, fired by a button
    /// that is a no-op everywhere else.
    pub fn reload(self: &Rc<Self>) {
        self.read_version();
        self.read_freshness();
    }

    /// Called after a run, with what AdGuard said.
    pub fn connect_checked(&self, checked: impl Fn(&UpdateReport) + 'static) {
        self.checked.replace(Some(Box::new(checked)));
    }

    fn read_version(self: &Rc<Self>) {
        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.version(),
            move |version: Result<String, adguard_core::Error>| match version {
                Ok(banner) => this.cli_version.set_subtitle(short_version(&banner)),
                // The row says what it could not do rather than sitting on a
                // placeholder that reads as "not installed".
                Err(err) => this.cli_version.set_subtitle(&err.to_string()),
            },
        );
    }

    /// Read when new filter data last arrived, from the two catalogues.
    ///
    /// Two SQLite reads and no network — which is the whole argument for this
    /// row existing instead of a check on launch. A launch check would spend a
    /// download to learn something AdGuard's own daemon learned within the last
    /// few hours, and, being silent, would tell the user none of it.
    ///
    /// The newer of the two sets. They are separate catalogues on separate
    /// schedules, but the question this answers — *is AdGuard still getting
    /// data?* — is about the install rather than about either database, and two
    /// rows would ask the user to do the comparison themselves.
    fn read_freshness(self: &Rc<Self>) {
        let this = self.clone();
        worker::run(
            || {
                [FilterSet::Http, FilterSet::Dns]
                    .into_iter()
                    // A catalogue that will not open is not an error here: this
                    // row is a nicety beside a button that works without it, and
                    // the Filters page already reports an unreadable database.
                    .filter_map(|set| Catalogue::open_set(set).ok())
                    .filter_map(|catalogue| catalogue.last_downloaded().ok().flatten())
                    .max()
            },
            move |newest: Option<i64>| {
                let now = glib::DateTime::now_utc().map(|now| now.to_unix()).unwrap_or(0);
                this.freshness.set_label(&match newest {
                    Some(when) => ago(now - when),
                    // No installed list carries one. Not "never" — an empty or
                    // unreadable catalogue looks the same from here.
                    None => PLACEHOLDER.to_owned(),
                });
            },
        );
    }

    /// Run `check-update`, off the main thread.
    ///
    /// The guard is the button's own sensitivity plus [`Self::busy`]: the
    /// measured range is 1.8–7.3 s, which is comfortably long enough for a
    /// second click, and two of these in flight would put two writers into the
    /// same data directory.
    fn check_now(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.check.set_sensitive(false);
        self.check.set_label(WORKING);

        let cli = self.cli.clone();
        let this = self.clone();
        worker::run(
            move || cli.check_update(),
            move |result: Result<UpdateReport, adguard_core::Error>| {
                this.busy.set(false);
                this.check.set_sensitive(true);
                this.check.set_label(UPDATE);
                match result {
                    Ok(report) => this.render(&report),
                    Err(err) => this.render_error(&err.to_string()),
                }
            },
        );
    }

    /// Draw what AdGuard said, one row per component, in its order.
    fn render(self: &Rc<Self>, report: &UpdateReport) {
        self.clear();

        for component in &report.components {
            let row = adw::ActionRow::builder().title(component.part.title()).build();
            row.set_use_markup(false);

            // AdGuard's own sentence is the value. An unanswered header has no
            // sentence, and saying so is better than an empty cell.
            let said = if component.said.is_empty() {
                "No answer"
            } else {
                &component.said
            };
            // Not wrapped. AdGuard's longest sentence here is *Failed to update
            // filters*, which fits on one line beside a title, and letting it
            // wrap breaks even `Up to date` across two — so the column of
            // verdicts stops being scannable in order to accommodate a case
            // that does not need it. A narrow window shortens the title, which
            // is our text, rather than AdGuard's.
            let value = gtk::Label::builder()
                .label(said)
                .valign(gtk::Align::Center)
                .xalign(1.0)
                .build();
            // Colour never carries a fact on its own here — the sentence beside
            // it already says the same thing — which is the rule the Status
            // page's state rows follow.
            value.add_css_class(match component.verdict {
                Verdict::Changed => "success",
                Verdict::Failed => "error",
                Verdict::Unrecognised => "warning",
                Verdict::UpToDate => "dim-label",
            });
            row.add_suffix(&value);

            if component.verdict == Verdict::Failed {
                // Measured: every failure cleared on the next run. Saying so is
                // what keeps this from reading as a broken install.
                row.set_subtitle("Temporary — try again");
                row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            }

            self.results.add(&row);
            self.rows.borrow_mut().push(row);
        }

        self.results.set_title(&format!("Last updated {}", now()));
        self.results.set_visible(true);

        // AdGuard's word about itself, when it has one. `app_notice` withholds a
        // failed *check* — that is already a row above, and repeating it here
        // would recommend a command on the strength of a check that did not
        // finish (contract §14).
        match report.app_notice() {
            Some(said) => {
                self.notice_row.set_subtitle(said);
                self.notice.set_visible(true);
            }
            None => self.notice.set_visible(false),
        }

        let failed = report.failures().count();
        self.toasts.add_toast(toast(if failed == 0 {
            "Update finished"
        } else {
            // Deliberately not a count: what failed is on the page, named, and a
            // number here would be arithmetic about rows rather than about the
            // install.
            "Update finished — some parts did not update"
        }));

        // The run may have brought data down, so the figure above the button is
        // now stale — and it is read from the catalogues rather than from the
        // report, because `1 filter(s) updated` says something happened and the
        // database says when.
        self.read_freshness();

        if let Some(checked) = self.checked.borrow().as_ref() {
            checked(report);
        }
    }

    /// The command ran and its answer could not be read, or it never ran.
    ///
    /// Shown on the page rather than only in a toast: `Error::Unparseable`
    /// carries AdGuard's actual output, and that output is the only evidence
    /// anyone has of what the CLI now says. A toast would take it away again
    /// after a few seconds.
    fn render_error(self: &Rc<Self>, message: &str) {
        self.clear();

        let row = adw::ActionRow::builder()
            .title("AdGuard could not be asked")
            .subtitle(message)
            .build();
        row.set_use_markup(false);
        row.set_subtitle_lines(6);
        row.set_subtitle_selectable(true);
        row.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));

        self.results.add(&row);
        self.rows.borrow_mut().push(row);
        self.results.set_title(&format!("Last tried {}", now()));
        self.results.set_visible(true);
        self.notice.set_visible(false);

        self.toasts.add_toast(toast(message));
    }

    fn clear(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.results.remove(&row);
        }
    }
}

/// `AdGuard CLI v1.4.13` -> `1.4.13`, and anything else through unchanged.
///
/// The row is already titled *AdGuard CLI*, so the banner repeats it, and the
/// number beside it should read like the application's own version above it
/// rather than like a sentence. A banner in any other shape is shown whole:
/// this is a presentation trim, not a parse, and losing an unfamiliar version
/// string would be worse than showing a familiar prefix.
fn short_version(banner: &str) -> &str {
    let banner = banner.trim();
    banner
        .strip_prefix("AdGuard CLI ")
        .map_or(banner, |rest| rest.strip_prefix('v').unwrap_or(rest))
}

/// A gap in seconds, as a phrase a person would use.
///
/// Coarse on purpose. The figure answers "is AdGuard still getting data", and
/// that question is settled by *minutes / hours / days* — a precise
/// "3 hours 14 minutes" would imply the number matters, and it does not.
///
/// A negative gap is a clock that has moved backwards, or a timestamp written
/// by a machine whose clock disagrees with this one. It reads as `just now`
/// rather than as a time in the future, which is the harmless direction: the
/// alternative is a row announcing that filters will be updated tomorrow.
fn ago(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    fn plural(count: i64, unit: &str) -> String {
        if count == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{count} {unit}s ago")
        }
    }

    match seconds {
        s if s < MINUTE => "just now".to_owned(),
        s if s < HOUR => plural(s / MINUTE, "minute"),
        s if s < DAY => plural(s / HOUR, "hour"),
        s => plural(s / DAY, "day"),
    }
}

/// The local time, for "last updated". Falls back to nothing readable rather
/// than to a wrong time.
fn now() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|when| when.format("at %H:%M").ok())
        .map_or_else(|| "just now".to_owned(), |when| when.to_string())
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    // Before the strings, which are consumed as they are set: filter names and
    // CLI messages contain `&`, and markup is on by default.
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

/// A row that opens a URL, with the URL as its own subtitle so it can be read
/// and copied by someone who would rather not be sent anywhere.
fn link_row(title: &str, uri: &'static str) -> adw::ActionRow {
    let row = row(title, uri);
    row.set_subtitle_selectable(true);
    row.set_activatable(true);
    row.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    row.connect_activated(move |row| {
        let launcher = gtk::UriLauncher::new(uri);
        let window = row.root().and_downcast::<gtk::Window>();
        glib::spawn_future_local(async move {
            // Nothing to report on failure: the address is in the row, in full,
            // and selectable.
            let _ = launcher.launch_future(window.as_ref()).await;
        });
    });
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_reads_as_a_phrase() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3_599), "59 minutes ago");
        assert_eq!(ago(3_600), "1 hour ago");
        assert_eq!(ago(86_399), "23 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(8 * 86_400), "8 days ago");
    }

    /// A clock that moved backwards, or a timestamp from a machine that
    /// disagrees with this one. The row must not announce a future update.
    #[test]
    fn a_negative_gap_is_not_a_time_in_the_future() {
        assert_eq!(ago(-1), "just now");
        assert_eq!(ago(-86_400), "just now");
    }

    /// The figure says when data last *changed*, and the column behind it does
    /// not move on an up-to-date run (contract §14) — so the sentence beside it
    /// has to stop "8 days ago" from reading as an install that has stopped
    /// checking. Without this the row would send users to a button that will
    /// answer `Up to date`.
    #[test]
    fn the_freshness_row_says_a_gap_is_not_a_failure_to_check() {
        let said = WHAT_A_GAP_MEANS.to_lowercase();
        assert!(said.contains("on its own"), "it has to say something else is checking");
        assert!(said.contains("not that nothing has looked"), "and deny the wrong reading");
    }

    #[test]
    fn the_cli_banner_is_trimmed_to_a_version() {
        assert_eq!(short_version("AdGuard CLI v1.4.13"), "1.4.13");
        assert_eq!(short_version("AdGuard CLI v1.4.13\n"), "1.4.13");
    }

    /// A reworded banner is shown whole rather than mangled or dropped. The trim
    /// is cosmetic, and a cosmetic rule must not be able to lose information.
    #[test]
    fn an_unfamiliar_banner_survives_whole() {
        assert_eq!(short_version("adguard-cli 2.0"), "adguard-cli 2.0");
        assert_eq!(short_version("AdGuard CLI 1.4.13"), "1.4.13");
        assert_eq!(short_version(""), "");
    }

    /// The two links here and the ones in the AppStream metadata are the same
    /// project, and nothing but this test connects them — the file is data, and
    /// a rename would leave these two pointing at a repository that had moved.
    #[test]
    fn the_project_links_match_the_appstream_metadata() {
        const METAINFO: &str =
            include_str!("../../../data/io.github.dominik-najberg.AdGuardUI.metainfo.xml");
        assert!(METAINFO.contains(HOMEPAGE), "homepage is not the one in the metainfo");
        assert!(METAINFO.contains(ISSUES), "issue tracker is not the one in the metainfo");
    }

    /// The measured trap: `check-update` downloads five of its six components
    /// and only checks the sixth, so a control described as a check would
    /// misdescribe nearly all of what it does (contract §14).
    #[test]
    fn the_control_does_not_describe_itself_as_only_a_check() {
        assert_eq!(UPDATE, "Update now");
        let described = WHAT_IT_DOES.to_lowercase();
        assert!(described.starts_with("downloads"), "the download half comes first");
        assert!(described.contains("asks whether"), "and the check half is still named");
    }

    /// This application installs nothing privileged, and the group that mentions
    /// an available update has to say why it is only mentioning it.
    #[test]
    fn the_update_notice_explains_why_it_is_not_a_button() {
        let why = WHY_NO_BUTTON.to_lowercase();
        assert!(why.contains("privileged"));
        assert!(why.contains("yours to run"));
        assert_eq!(UPDATE_COMMAND, "adguard-cli update");
    }
}
