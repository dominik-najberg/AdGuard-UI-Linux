//! The app's one stylesheet.
//!
//! Everything here is layout — padding, corner radius, a font size — or a tint
//! taken from a colour libadwaita already defines. Nothing sets a literal
//! colour, and that is the rule this file is meant to keep: `@success_color`
//! and friends are redefined by the platform stylesheet under a dark theme, by
//! the user's accent colour, and by high contrast, so a tint written as
//! `alpha(@success_color, 0.1)` follows all three where `#2ec27e` would follow
//! none. `alpha()` is GTK's own CSS function, so the blend happens after the
//! theme has had its say.
//!
//! Widget-level styling stays out of here: libadwaita's own classes — `.card`,
//! `.title-2`, `.dim-label`, `.numeric`, `.success` — already say what the
//! Status page needs, and a class defined here is one the next GTK release
//! cannot improve underneath us. What is left is the handful of things it has
//! no class for: a tinted hero panel, a pill-shaped state badge, and the
//! padding for a row of figures.

use gtk::gdk;
use gtk4 as gtk;

/// Added to the hero panel and to the shield beside it, by state.
pub const HERO: &str = "hero";
pub const HERO_ON: &str = "hero-on";
pub const HERO_OFF: &str = "hero-off";
pub const HERO_UNKNOWN: &str = "hero-unknown";

/// The small upper-case word above the hero's title.
pub const BADGE: &str = "state-badge";
pub const BADGE_ON: &str = "state-badge-on";
pub const BADGE_OFF: &str = "state-badge-off";
pub const BADGE_UNKNOWN: &str = "state-badge-unknown";

/// The row of figures under the hero, and one figure in it.
pub const STATS: &str = "stat-row";
pub const STAT: &str = "stat-tile";
pub const STAT_VALUE: &str = "stat-value";

const CSS: &str = "
.hero {
  padding: 24px;
}

/* Tints, not fills: 8% of a theme colour over the card background reads as
   `this is the state` at a glance while leaving the text at the contrast the
   theme chose for it. A saturated panel would need its own foreground colour
   for every theme, which is the trap this avoids. */
.hero-on {
  background-color: alpha(@success_color, 0.08);
}

.hero-off {
  background-color: alpha(@warning_color, 0.08);
}

.hero-unknown {
  background-color: alpha(@error_color, 0.08);
}

.state-badge {
  padding: 1px 9px;
  border-radius: 9999px;
  font-size: 0.78em;
  font-weight: bold;
  /* The badge is one short word, and letter-spacing is what keeps an
     upper-case one from reading as a shout. */
  letter-spacing: 0.06em;
}

.state-badge-on {
  background-color: alpha(@success_color, 0.15);
  color: @success_color;
}

.state-badge-off {
  background-color: alpha(@warning_color, 0.15);
  color: @warning_color;
}

.state-badge-unknown {
  background-color: alpha(@error_color, 0.15);
  color: @error_color;
}

.stat-row {
  padding: 4px 0;
}

.stat-tile {
  padding: 14px 20px;
}

.stat-value {
  font-size: 1.4em;
  font-weight: bold;
}
";

/// Install the stylesheet for the whole display.
///
/// `APPLICATION` priority puts it above the platform stylesheet — so the
/// classes here win — and below `USER`, so a user's own `gtk.css` still has the
/// last word. Called once, before any window is built.
///
/// Silently does nothing when there is no display to style, which is only
/// reachable from a headless run; the pages render without these classes, so
/// there is nothing worth reporting.
pub fn install() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    // `load_from_string` would read better, but it is GTK 4.12 and this crate
    // builds against the `v4_10` feature (see the workspace manifest).
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
