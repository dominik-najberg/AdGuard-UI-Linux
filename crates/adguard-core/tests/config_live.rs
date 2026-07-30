//! Integration tests against the real `proxy.yaml`.
//!
//! The unit tests in `config.rs` run against a trimmed sample, which proves the
//! accessors but not that they point at keys AdGuard actually writes. These
//! read the machine's own config — the file we do not control and which could
//! be reshaped by any CLI update.
//!
//! They **skip** rather than fail when AdGuard CLI is not installed, so the
//! suite still passes on a machine (or CI runner) without it.

use adguard_core::config::key;
use adguard_core::{Config, Kind, Toggle, ADVANCED, STEALTH};

fn load() -> Option<Config> {
    let path = adguard_core::paths::config_file()?;
    if !path.is_file() {
        eprintln!("skipping: {} not present", path.display());
        return None;
    }
    Some(Config::read(&path).expect("the machine's proxy.yaml should parse"))
}

/// Every Protection switch must resolve to a real boolean in the real file. A
/// `None` here means the key was renamed upstream and that row would silently
/// render as "unavailable" — exactly the kind of quiet breakage this catches.
#[test]
fn every_protection_toggle_resolves() {
    let Some(config) = load() else { return };

    for toggle in Toggle::ALL {
        assert!(
            config.toggle(toggle).is_some(),
            "{} ({}) did not resolve to a boolean in {}",
            toggle.title(),
            toggle.key(),
            config.path().display(),
        );
    }
}

/// The other keys this crate reads by path.
#[test]
fn supporting_keys_resolve() {
    let Some(config) = load() else { return };

    assert!(
        matches!(config.proxy_mode(), Some("manual" | "auto")),
        "unexpected proxy_mode: {:?}",
        config.proxy_mode(),
    );
    assert!(
        config.int_at(key::DNS_LISTEN_PORT).is_some(),
        "{} did not resolve to an integer",
        key::DNS_LISTEN_PORT,
    );
    assert!(
        config.str_at(key::LISTEN_ADDRESS).is_some(),
        "{} did not resolve to a string",
        key::LISTEN_ADDRESS,
    );
    assert!(
        config.listen_auth_enabled().is_some(),
        "{} did not resolve to a boolean",
        key::LISTEN_AUTH_ENABLED,
    );
}

/// Every Advanced setting must resolve in the real file **as the type its
/// control needs**. A key that reads `None` renders as an insensitive
/// "unavailable" row, which is honest but useless — and since the whole page is
/// driven off this table, a single upstream rename would empty a group without
/// anything else noticing.
#[test]
fn every_advanced_setting_resolves_with_the_right_type() {
    let Some(config) = load() else { return };

    for group in ADVANCED.iter().chain(STEALTH.iter()) {
        for setting in group.settings {
            let resolved = match setting.kind {
                Kind::Switch => config.bool_at(setting.key).is_some(),
                Kind::Number { .. } => config.int_at(setting.key).is_some(),
                // A credential or a host legitimately holds the empty string;
                // `str_at` still returns `Some` for it, which is the point.
                Kind::Text { .. } => config.str_at(setting.key).is_some(),
                Kind::Choice { options } => config.choice_at(setting.key, options).is_some(),
            };
            assert!(
                resolved,
                "{} ({}) did not resolve as {:?} in {}",
                setting.title,
                setting.key,
                setting.kind,
                config.path().display(),
            );
        }
    }
}

/// The numbers in the real file should be ones the page can actually write.
///
/// Not a claim about our code so much as a check that the shipped defaults sit
/// inside the ranges chosen for them — if they do not, a fresh install would
/// open the Advanced page with read-only rows and no explanation beyond "edit
/// the file yourself".
#[test]
fn the_shipped_numbers_are_inside_the_ranges_this_page_offers() {
    let Some(config) = load() else { return };

    for group in &ADVANCED {
        for setting in group.settings {
            if !matches!(setting.kind, Kind::Number { .. }) {
                continue;
            }
            let Some(value) = config.int_at(setting.key) else {
                continue; // covered by the test above
            };
            assert!(
                setting.permits_number(value),
                "{} is {value} in the real config, outside the range {:?} this page \
                 will write — the row would open read-only",
                setting.key,
                setting.kind,
            );
        }
    }
}

/// The list-valued keys `config get` refuses ("This field is not a separate
/// setting") still read fine from the file. `filters` carries the `flm://`
/// marker plus the user's own rules file.
#[test]
fn list_valued_keys_read_from_the_file() {
    let Some(config) = load() else { return };

    let filters = config.list_at("filters").expect("`filters` should be a list");
    assert!(
        filters.contains(&"flm://"),
        "`filters` lost its flm:// marker: {filters:?}",
    );

    assert!(
        config.list_at("dns_filtering.filters").is_some(),
        "`dns_filtering.filters` should be a list",
    );
}

/// The safety invariant behind `listen_address_plan`: if this machine is
/// exposed beyond loopback, authentication must be on. Not an assertion about
/// our code so much as a check that the config it will act on is sane.
#[test]
fn exposure_implies_authentication() {
    let Some(config) = load() else { return };

    if config.listens_beyond_loopback() {
        assert_eq!(
            config.listen_auth_enabled(),
            Some(true),
            "{} is not loopback but listen_auth is off",
            config.str_at(key::LISTEN_ADDRESS).unwrap_or("?"),
        );
    }
}

/// The address in the real file must be a form `is_loopback` actually
/// understands, not merely one it can return an answer for.
///
/// It returns `false` for anything unparseable, which is the safe default but
/// is indistinguishable from a confident "this is exposed". So assert the
/// value parses: a future config that writes, say, `0.0.0.0:3128` or a
/// hostname would otherwise be silently classified as exposed forever, and the
/// UI would nag about authentication that is not needed.
#[test]
fn the_real_listen_address_is_a_form_we_understand() {
    let Some(config) = load() else { return };
    let address = config
        .str_at(key::LISTEN_ADDRESS)
        .expect("listen_address should be a string");
    let bare = address.trim().trim_matches(['\'', '"']);

    assert!(
        bare.eq_ignore_ascii_case("localhost") || bare.parse::<std::net::IpAddr>().is_ok(),
        "listen_address {address:?} is neither an IP nor localhost — is_loopback \
         would call it exposed without being able to tell",
    );
}
