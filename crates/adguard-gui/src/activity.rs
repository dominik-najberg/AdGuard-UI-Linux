//! The Activity page: what the proxy has been doing, counted.
//!
//! The first item of [issue #21]. Everything shown here comes from
//! `adguard_core::activity`, which keeps counts and never requests — the
//! decision, and what it leaves out, are that module's header. This page's job
//! is to show the counts and to say plainly what is kept, where, and how to get
//! rid of it.
//!
//! # When it reads
//!
//! The counts are brought up to date by [`keep_up`], which reads whatever
//! AdGuard has logged since last time: once at launch and every
//! [`INTERVAL`] after, in the background, whether or not the window is open.
//! That is not a page polling for display — nothing is redrawn — but the one
//! way to count a log that AdGuard deletes after a few days. Opening the page,
//! changing the range and pressing refresh each read the log again first, so
//! what is shown is never older than the click.
//!
//! [issue #21]: https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/21

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adguard_core::activity::{self, Bucket, Span, Store, Summary};
use adguard_core::{access, Catalogue, FilterSet, Locale};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::{style, toast, worker};

/// How often [`keep_up`] reads the log.
///
/// AdGuard keeps ten generations of ~10 MiB, and the shortest measured lasted
/// two hours (contract §9), so the window is at least twenty hours at the
/// heaviest traffic seen. Ten minutes is far inside that, and a read with
/// nothing new costs a quarter of a millisecond.
const INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Said once, under the page title.
const WHAT_IT_IS: &str = "What AdGuard has done with this computer's traffic, from its own \
                          access log. Updated in the background while AdGuard UI runs.";

/// Said beside the Clear button. Every clause is something the store really
/// does or really does not do — `activity.rs` has the test for each.
const WHAT_IS_KEPT: &str = "Kept on this computer only, and readable only by you: counts per \
                            hour, and per day for each site, rule and app. Never the address \
                            of a page, what was searched, or when in a day a site was visited.";

/// The client name AdGuard gives its own requests (contract §9).
const INTERNAL_CLIENT: &str = "internal_proxy_client";

/// Start reading the log in the background: now, and every [`INTERVAL`].
///
/// Held by nothing: the timer lives as long as the main loop, which is as long
/// as the application.
pub fn keep_up() {
    ingest();
    glib::timeout_add_local(INTERVAL, || {
        ingest();
        glib::ControlFlow::Continue
    });
}

/// One background read. A failure is printed rather than shown: nobody asked
/// for this one, and the page reports its own when it is opened.
fn ingest() {
    worker::run(
        || -> Result<(), String> {
            let Some(live) = access::path() else { return Ok(()) };
            Store::locate()
                .and_then(|mut store| store.ingest(&live))
                .map(|_| ())
                .map_err(|err| err.to_string())
        },
        |result| {
            if let Err(err) = result {
                eprintln!("adguard-ui: could not count AdGuard's activity: {err}");
            }
        },
    );
}

/// What one reading brings back to the main thread.
struct Loaded {
    summary: Summary,
    /// Filter list names by id, for the rules list. Empty when the catalogue
    /// could not be read, in which case a rule shows its id instead.
    names: HashMap<i64, String>,
}

pub struct ActivityPage {
    toasts: adw::ToastOverlay,
    page: adw::PreferencesPage,
    header: adw::PreferencesGroup,
    range: adw::ToggleGroup,
    /// Requests, blocked, modified, traffic.
    figures: [gtk::Label; 4],
    chart: gtk::DrawingArea,
    /// What the chart draws. Shared with its draw function.
    bars: Rc<RefCell<Vec<Bucket>>>,
    span: Rc<Cell<Span>>,
    axis: (gtk::Label, gtk::Label),
    /// A line under the chart for what the four figures leave out.
    footnote: gtk::Label,
    /// Shown only when lines of the log could not be read.
    unread: adw::ActionRow,
    /// The four top lists. Rebuilt whole on every reading.
    lists: RefCell<Vec<adw::PreferencesGroup>>,
    privacy: adw::PreferencesGroup,
    busy: Cell<bool>,
    /// A reading was asked for while one was running; run it when that ends,
    /// so a range picked mid-read is not lost.
    again: Cell<bool>,
}

impl ActivityPage {
    pub fn new(toasts: adw::ToastOverlay) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        // Not shrinkable: in a group's header the suffix is squeezed first,
        // and a shrinkable group ellipsised all three labels to "To…".
        let range = adw::ToggleGroup::builder()
            .valign(gtk::Align::Center)
            .can_shrink(false)
            .build();
        for (name, label) in [("today", "Today"), ("week", "7 Days"), ("month", "30 Days")] {
            range.add(adw::Toggle::builder().name(name).label(label).build());
        }
        range.set_active_name(Some("today"));
        let header = adw::PreferencesGroup::builder()
            .title("Activity")
            .description(WHAT_IT_IS)
            .header_suffix(&range)
            .build();

        let figures = [stat_value(), stat_value(), stat_value(), stat_value()];
        let stats = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .build();
        stats.add_css_class("card");
        stats.add_css_class(style::STATS);
        for (value, caption) in figures.iter().zip(["Requests", "Blocked", "Modified", "Traffic"]) {
            stats.append(&stat_tile(value, caption));
        }
        header.add(&stats);

        let bars: Rc<RefCell<Vec<Bucket>>> = Rc::new(RefCell::new(Vec::new()));
        let span = Rc::new(Cell::new(Span::Today));
        let chart = gtk::DrawingArea::builder()
            .content_height(140)
            .hexpand(true)
            .has_tooltip(true)
            .build();
        chart.set_accessible_role(gtk::AccessibleRole::Img);
        chart.set_draw_func({
            let bars = bars.clone();
            move |area, cr, width, height| draw(area, cr, width, height, &bars.borrow())
        });
        chart.connect_query_tooltip({
            let bars = bars.clone();
            let span = span.clone();
            move |area, x, _, _, tooltip| {
                let bars = bars.borrow();
                let width = area.width().max(1) as f64;
                let index = (x as f64 / width * bars.len() as f64) as usize;
                let Some(bucket) = bars.get(index) else { return false };
                tooltip.set_text(Some(&format!(
                    "{}: {} requests, {} blocked",
                    when(bucket.start, span.get()),
                    grouped(bucket.counts.total()),
                    grouped(bucket.counts.blocked),
                )));
                true
            }
        });

        let axis = (axis_label(gtk::Align::Start), axis_label(gtk::Align::End));
        let axis_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .build();
        axis_row.append(&axis.0);
        axis_row.append(&axis.1);

        let footnote = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .build();
        footnote.add_css_class("dim-label");
        footnote.add_css_class("caption");

        let legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(16)
            .build();
        legend.append(&swatch(false, "All requests"));
        legend.append(&swatch(true, "Blocked"));

        let chart_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        chart_box.append(&legend);
        chart_box.append(&chart);
        chart_box.append(&axis_row);
        chart_box.append(&footnote);
        let chart_card = gtk::Box::builder().margin_top(12).build();
        chart_card.add_css_class("card");
        chart_card.append(&chart_box);
        header.add(&chart_card);

        let unread = adw::ActionRow::builder()
            .title("Some of AdGuard's log could not be read")
            .subtitle(
                "Lines in a shape this version does not recognise are left out rather than \
                 guessed at, so the figures may be low. An AdGuard CLI update that changed its \
                 log is the usual cause.",
            )
            .subtitle_lines(0)
            .visible(false)
            .margin_top(12)
            .build();
        let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warning.add_css_class("warning");
        unread.add_prefix(&warning);
        unread.add_css_class("card");
        header.add(&unread);
        page.add(&header);

        let clear = gtk::Button::builder()
            .label("Clear History…")
            .valign(gtk::Align::Center)
            .build();
        clear.add_css_class("destructive-action");
        let privacy = adw::PreferencesGroup::builder()
            .title("History")
            .header_suffix(&clear)
            .build();
        page.add(&privacy);

        let this = Rc::new(Self {
            toasts,
            page,
            header,
            range,
            figures,
            chart,
            bars,
            span,
            axis,
            footnote,
            unread,
            lists: RefCell::new(Vec::new()),
            privacy,
            busy: Cell::new(false),
            again: Cell::new(false),
        });

        this.range.connect_active_name_notify({
            let this = Rc::downgrade(&this);
            move |range| {
                let Some(this) = this.upgrade() else { return };
                this.span.set(match range.active_name().as_deref() {
                    Some("week") => Span::Week,
                    Some("month") => Span::Month,
                    _ => Span::Today,
                });
                this.reload();
            }
        });
        clear.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                let Some(this) = this.upgrade() else { return };
                glib::spawn_future_local(async move { this.clear().await });
            }
        });
        this.describe_privacy();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    /// Read the log, then the counts for the chosen range. Called when the page
    /// is selected, when the range changes and by the refresh button.
    pub fn reload(self: &Rc<Self>) {
        if self.busy.replace(true) {
            self.again.set(true);
            return;
        }
        let span = self.span.get();
        let this = self.clone();
        worker::run(
            move || load(span),
            move |result: Result<Loaded, String>| {
                this.busy.set(false);
                match result {
                    Ok(loaded) => this.render(&loaded),
                    Err(err) => this.header.set_description(Some(&format!(
                        "The activity history could not be read: {err}"
                    ))),
                }
                if this.again.take() {
                    this.reload();
                }
            },
        );
    }

    fn render(&self, loaded: &Loaded) {
        let summary = &loaded.summary;
        let totals = &summary.totals;
        let total = totals.total();

        self.figures[0].set_label(&grouped(total));
        self.figures[1].set_label(&match total {
            0 => "0".to_owned(),
            _ => format!(
                "{} ({:.0}%)",
                grouped(totals.blocked),
                totals.blocked as f64 * 100.0 / total as f64
            ),
        });
        self.figures[2].set_label(&grouped(totals.modified));
        self.figures[3].set_label(&glib::format_size(summary.bytes));

        self.header.set_description(Some(&match summary.recorded_since {
            None => "Nothing counted yet. AdGuard writes a line for each request it handles, \
                     and they are counted here as they arrive."
                .to_owned(),
            // The history began part-way through the range, so the range is not
            // what it says: say from when.
            Some(first) if first > summary.since => format!(
                "{WHAT_IT_IS} Counted from {}, the oldest AdGuard still had when counting began.",
                when(first, Span::Month),
            ),
            Some(_) => WHAT_IT_IS.to_owned(),
        }));

        let peak = summary.buckets.iter().max_by_key(|bucket| bucket.counts.total());
        let description = match peak {
            Some(peak) if peak.counts.total() > 0 => format!(
                "Requests per {}, blocked shaded. The busiest was {}, with {}.",
                if summary.span == Span::Today { "hour" } else { "day" },
                when(peak.start, summary.span),
                grouped(peak.counts.total()),
            ),
            _ => "No requests counted in this range.".to_owned(),
        };
        self.chart.update_property(&[gtk::accessible::Property::Label(&description)]);
        self.bars.replace(summary.buckets.clone());
        self.chart.queue_draw();
        if let (Some(first), Some(last)) = (summary.buckets.first(), summary.buckets.last()) {
            self.axis.0.set_label(&when(first.start, summary.span));
            self.axis.1.set_label(&match summary.span {
                Span::Today => when(last.start, summary.span),
                _ => "Today".to_owned(),
            });
        }

        let mut notes = Vec::new();
        if totals.allowed > 0 {
            notes.push(format!("{} let through by exception rules", grouped(totals.allowed)));
        }
        if totals.uninspected > 0 {
            // `-` in the action column, half of all QUIC lines (contract §9).
            // What AdGuard means by it is not measured, so it is named for what
            // the log shows rather than explained.
            notes.push(format!(
                "{} logged with no action, mostly QUIC",
                grouped(totals.uninspected)
            ));
        }
        self.footnote.set_label(&notes.join(" · "));
        self.footnote.set_visible(!notes.is_empty());
        self.unread.set_visible(summary.unread > 0);

        for group in self.lists.take() {
            self.page.remove(&group);
        }
        let groups = vec![
            ranked_group(
                "Most Blocked Sites",
                summary.blocked_hosts.iter().map(|host| (host.name.clone(), None, host.requests)),
            ),
            ranked_group(
                "Most Requested Sites",
                summary.hosts.iter().map(|host| (host.name.clone(), None, host.requests)),
            ),
            ranked_group(
                "Rules That Matched Most",
                summary.rules.iter().map(|rule| {
                    let list = loaded
                        .names
                        .get(&rule.filter)
                        .cloned()
                        .unwrap_or_else(|| format!("Filter list {}", rule.filter));
                    (rule.text.clone(), Some(format!("{list} · {}", verb(rule.action))), rule.requests)
                }),
            ),
            ranked_group(
                "Apps",
                summary.clients.iter().map(|client| {
                    let name = match client.name.as_str() {
                        INTERNAL_CLIENT => "AdGuard itself".to_owned(),
                        name => name.to_owned(),
                    };
                    (name, Some(format!("{} blocked", grouped(client.blocked))), client.requests)
                }),
            ),
        ];
        // Above the history group, which stays last.
        self.page.remove(&self.privacy);
        for group in &groups {
            self.page.add(group);
        }
        self.page.add(&self.privacy);
        self.lists.replace(groups);
    }

    fn describe_privacy(&self) {
        let path = Store::path()
            .map(|path| crate::abbreviate(&path))
            .unwrap_or_else(|| "your state folder".to_owned());
        self.privacy.set_description(Some(&format!(
            "{WHAT_IS_KEPT} Stored in {path}; counts older than {} days are deleted.",
            activity::RETENTION_DAYS
        )));
    }

    async fn clear(self: Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Clear activity history?"),
            Some(
                "Every count on this page will be deleted from this computer. AdGuard's own \
                 log is not touched, and counting carries on from now.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("clear", "Clear");
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        // Cancel is the default and the escape route: the other answer cannot
        // be undone.
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        if dialog.choose_future(Some(&self.page)).await != "clear" {
            return;
        }

        let this = self.clone();
        worker::run(
            || Store::locate().and_then(|mut store| store.clear()).map_err(|err| err.to_string()),
            move |result: Result<(), String>| {
                match result {
                    Ok(()) => this.toasts.add_toast(toast("Activity history cleared")),
                    Err(err) => this.toasts.add_toast(toast(&format!("Could not clear: {err}"))),
                }
                this.reload();
            },
        );
    }
}

/// The worker half of [`ActivityPage::reload`].
fn load(span: Span) -> Result<Loaded, String> {
    let mut store = Store::locate().map_err(|err| err.to_string())?;
    if let Some(live) = access::path() {
        store.ingest(&live).map_err(|err| err.to_string())?;
    }
    let summary = store.summary(span).map_err(|err| err.to_string())?;

    // Names for the lists the rules came from. A catalogue that cannot be read
    // costs the names and nothing else.
    let mut names = HashMap::new();
    if let Ok(catalogue) = Catalogue::open_set(FilterSet::Http) {
        let locale = Locale::from_env();
        for filter in catalogue.filters(&locale).unwrap_or_default() {
            names.insert(filter.id, filter.name);
        }
        if let Ok(Some(user)) = catalogue.user_rules(&locale) {
            names.insert(user.id, "Your rules".to_owned());
        }
    }
    Ok(Loaded { summary, names })
}

/// A top list: one row per entry, the count on the right.
fn ranked_group(
    title: &str,
    entries: impl Iterator<Item = (String, Option<String>, u64)>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let mut any = false;
    for (name, subtitle, requests) in entries {
        any = true;
        let row = adw::ActionRow::new();
        // Host names, rule text and program names all come from the log, and
        // rule text is full of characters markup would read.
        row.set_use_markup(false);
        row.set_title(&name);
        row.set_title_lines(2);
        row.set_title_selectable(true);
        if let Some(subtitle) = subtitle {
            row.set_subtitle(&subtitle);
        }
        let count = gtk::Label::new(Some(&grouped(requests)));
        count.add_css_class("dim-label");
        count.add_css_class("numeric");
        row.add_suffix(&count);
        group.add(&row);
    }
    if !any {
        let row = adw::ActionRow::builder().title("Nothing in this range").build();
        row.add_css_class("dim-label");
        group.add(&row);
    }
    group
}

/// What a rule did, as a word for its row.
fn verb(action: activity::Action) -> &'static str {
    match action {
        activity::Action::Blocked => "blocked",
        activity::Action::Modified => "modified",
        activity::Action::Allowed => "allowed",
        activity::Action::Passed | activity::Action::Uninspected => "matched",
    }
}

/// Draw the bars: every request in the foreground colour, faint, with the
/// blocked share over it in the accent colour.
///
/// Both colours come from the theme, as `style.rs` requires of everything: the
/// widget's own foreground follows dark mode and high contrast, and the accent
/// follows the user's choice of it.
fn draw(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32, bars: &[Bucket]) {
    if bars.is_empty() {
        return;
    }
    let peak = bars.iter().map(|bar| bar.counts.total()).max().unwrap_or(0).max(1) as f64;
    let (width, height) = (width as f64, height as f64);
    let slot = width / bars.len() as f64;
    let gap = (slot * 0.2).min(4.0);
    let foreground = area.color();
    let accent = adw::StyleManager::default().accent_color_rgba();

    // A baseline, so an empty range still shows where the bars would stand.
    cr.set_source_rgba(
        foreground.red().into(),
        foreground.green().into(),
        foreground.blue().into(),
        0.25,
    );
    cr.rectangle(0.0, height - 1.0, width, 1.0);
    let _ = cr.fill();

    for (index, bar) in bars.iter().enumerate() {
        let x = index as f64 * slot + gap / 2.0;
        let all = bar.counts.total() as f64 / peak * height;
        let blocked = bar.counts.blocked as f64 / peak * height;
        cr.set_source_rgba(
            foreground.red().into(),
            foreground.green().into(),
            foreground.blue().into(),
            0.18,
        );
        cr.rectangle(x, height - all, slot - gap, all);
        let _ = cr.fill();
        cr.set_source_rgba(accent.red().into(), accent.green().into(), accent.blue().into(), 1.0);
        cr.rectangle(x, height - blocked, slot - gap, blocked);
        let _ = cr.fill();
    }
}

/// A legend entry: a square in the colour [`draw`] uses, and what it means.
fn swatch(blocked: bool, label: &str) -> gtk::Box {
    let square = gtk::DrawingArea::builder()
        .content_width(10)
        .content_height(10)
        .valign(gtk::Align::Center)
        .build();
    square.set_accessible_role(gtk::AccessibleRole::Presentation);
    square.set_draw_func(move |area, cr, width, height| {
        let colour = if blocked {
            adw::StyleManager::default().accent_color_rgba()
        } else {
            area.color()
        };
        let alpha = if blocked { 1.0 } else { 0.18 };
        cr.set_source_rgba(colour.red().into(), colour.green().into(), colour.blue().into(), alpha);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = cr.fill();
    });
    let text = gtk::Label::new(Some(label));
    text.add_css_class("caption");
    let entry = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    entry.append(&square);
    entry.append(&text);
    entry
}

fn axis_label(align: gtk::Align) -> gtk::Label {
    let label = gtk::Label::builder().halign(align).build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.add_css_class("numeric");
    label
}

fn stat_value() -> gtk::Label {
    let label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .label("—")
        .build();
    label.add_css_class(style::STAT_VALUE);
    label.add_css_class("numeric");
    label
}

fn stat_tile(value: &gtk::Label, caption: &str) -> gtk::Box {
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    tile.add_css_class(style::STAT);
    let caption = gtk::Label::builder()
        .label(caption)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .build();
    caption.add_css_class("dim-label");
    caption.add_css_class("caption");
    tile.append(value);
    tile.append(&caption);
    tile
}

/// A bar's start as the axis and tooltip say it: an hour for today, a date
/// otherwise.
fn when(start: i64, span: Span) -> String {
    let format = match span {
        Span::Today => "%H:%M",
        _ => "%a %-d %b",
    };
    glib::DateTime::from_unix_local(start)
        .ok()
        .and_then(|at| at.format(format).ok())
        .map_or_else(|| start.to_string(), |text| text.to_string())
}

/// `236433` as `236,433`.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(236_433), "236,433");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    /// The disclosure beside the Clear button names what is never kept, and
    /// the store's own tests hold it to that.
    #[test]
    fn the_disclosure_names_what_is_left_out() {
        for left_out in ["address of a page", "searched", "when in a day", "this computer only"] {
            assert!(WHAT_IS_KEPT.contains(left_out), "{left_out}");
        }
    }
}
