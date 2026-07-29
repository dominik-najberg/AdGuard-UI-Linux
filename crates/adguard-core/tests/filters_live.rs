//! Integration tests against the real filter databases.
//!
//! These guard the SQL in `filters.rs` against AdGuard's actual schema — a
//! schema we do not control and which could change on any CLI update.
//!
//! They **skip** rather than fail when AdGuard CLI is not installed, so the
//! suite still passes on a machine (or CI runner) without it.

use adguard_core::filters::Catalogue;
use adguard_core::locale::Locale;
use adguard_core::model::FilterSet;
use adguard_core::paths;

use std::path::PathBuf;

/// Returns the DB path, or `None` if this machine has no AdGuard install.
fn catalogue_path(which: Option<PathBuf>) -> Option<PathBuf> {
    let path = which?;
    if path.is_file() {
        Some(path)
    } else {
        eprintln!("skipping: {} not present", path.display());
        None
    }
}

fn open(which: Option<PathBuf>) -> Option<Catalogue> {
    let path = catalogue_path(which)?;
    Some(Catalogue::open(&path).expect("should open read-only"))
}

fn check_catalogue(path: PathBuf, label: &str) {
    let catalogue = Catalogue::open(&path).expect("should open read-only");
    let locale = Locale::english();

    let groups = catalogue.groups(&locale).expect("should read filter_group");
    let filters = catalogue.filters(&locale).expect("should read filter");

    assert!(!groups.is_empty(), "{label}: no filter groups");
    assert!(!filters.is_empty(), "{label}: no filters");

    // The user-rules pseudo-filter must not leak into the subscribable list:
    // it sits in a group_id of 0 that does not exist in filter_group.
    assert!(
        !filters.iter().any(|f| f.is_user_rules()),
        "{label}: user-rules pseudo-filter leaked into filters()"
    );

    for filter in &filters {
        assert!(
            groups.iter().any(|g| g.id == filter.group_id),
            "{label}: filter {} references unknown group {}",
            filter.id,
            filter.group_id
        );
        assert!(
            !filter.title.is_empty(),
            "{label}: filter {} has an empty title",
            filter.id
        );
        // Every row must arrive with something renderable, whether or not the
        // locale had a translation for it.
        assert!(
            !filter.name.is_empty(),
            "{label}: filter {} has an empty display name",
            filter.id
        );
        // Holds for every real filter, but NOT for the pseudo-filter above,
        // which is why it must be excluded before asserting this.
        if filter.enabled {
            assert!(
                filter.installed,
                "{label}: filter {} is enabled but not installed",
                filter.id
            );
        }
    }

    eprintln!(
        "{label}: {} groups, {} filters, {} installed",
        groups.len(),
        filters.len(),
        filters.iter().filter(|f| f.installed).count()
    );
}

#[test]
fn reads_http_filter_catalogue() {
    let Some(path) = catalogue_path(paths::filters_db()) else {
        return;
    };
    check_catalogue(path, "agflm_standard");
}

#[test]
fn reads_dns_filter_catalogue() {
    let Some(path) = catalogue_path(paths::dns_filters_db()) else {
        return;
    };
    check_catalogue(path, "agflm_dns");
}

/// The "Custom filters" group is real and present, unlike group 0. The UI
/// needs it to host user-installed lists.
#[test]
fn custom_filters_group_exists() {
    let Some(catalogue) = open(paths::filters_db()) else {
        return;
    };
    let groups = catalogue
        .groups(&Locale::english())
        .expect("should read filter_group");

    assert!(
        groups.iter().any(|g| g.is_custom()),
        "no custom-filters group; groups were {:?}",
        groups.iter().map(|g| (&g.name, g.id)).collect::<Vec<_>>()
    );
}

/// The user-rules pseudo-filter is reachable on its own, and is the documented
/// exception to the enabled-implies-installed invariant.
#[test]
fn user_rules_pseudo_filter_is_reachable() {
    for (which, label) in [
        (paths::filters_db(), "agflm_standard"),
        (paths::dns_filters_db(), "agflm_dns"),
    ] {
        let Some(catalogue) = open(which) else {
            continue;
        };
        let user_rules = catalogue
            .user_rules(&Locale::english())
            .expect("lookup should not error");

        let Some(filter) = user_rules else {
            panic!("{label}: user-rules pseudo-filter missing");
        };
        assert!(filter.is_user_rules());
        assert!(!filter.name.is_empty(), "{label}: empty display name");
        eprintln!(
            "{label}: user rules = {:?} (enabled {}, installed {})",
            filter.name, filter.enabled, filter.installed
        );
    }
}

/// Localised names back the filter UI, so the joins must resolve against the
/// real `filter_localisation` table (thousands of rows keyed by `lang`).
#[test]
fn localises_filter_names() {
    let Some(catalogue) = open(paths::filters_db()) else {
        return;
    };

    let english = catalogue.filters(&Locale::english()).expect("read");
    let polish = catalogue.filters(&Locale::parse("pl_PL.UTF-8")).expect("read");

    assert_eq!(english.len(), polish.len(), "locale changed the row count");

    // `pl` has no region-specific rows, so this also proves the fallback from
    // `pl_PL` to `pl` works — without it, every name would stay English.
    let translated = english
        .iter()
        .zip(&polish)
        .filter(|(en, pl)| en.name != pl.name)
        .count();
    assert!(
        translated > 0,
        "no name differed under pl_PL; the localisation join is not matching"
    );
    eprintln!("{translated}/{} filter names differ under pl_PL", english.len());

    // Every row still renders, including the one filter with no `en` row.
    assert!(polish.iter().all(|f| !f.name.is_empty()));
}

/// A locale nobody translated into must degrade to English, not to blanks.
#[test]
fn unknown_locale_falls_back_to_english() {
    let Some(catalogue) = open(paths::filters_db()) else {
        return;
    };
    let english = catalogue.filters(&Locale::english()).expect("read");
    let nonsense = catalogue
        .filters(&Locale::parse("zz_ZZ"))
        .expect("a missing locale is not an error");

    let names: Vec<&str> = nonsense.iter().map(|f| f.name.as_str()).collect();
    let titles: Vec<&str> = english.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(names, titles, "fallback should be the English title column");
}

/// Group headings are localised too — `filter_group_localisation`, a table
/// distinct from the per-filter one.
#[test]
fn localises_group_names() {
    let Some(catalogue) = open(paths::filters_db()) else {
        return;
    };
    let english = catalogue.groups(&Locale::english()).expect("read");
    let polish = catalogue.groups(&Locale::parse("pl")).expect("read");

    assert_eq!(english.len(), polish.len());
    assert!(
        english.iter().zip(&polish).any(|(en, pl)| en.name != pl.name),
        "no group heading differed under pl: {:?}",
        polish.iter().map(|g| &g.name).collect::<Vec<_>>()
    );
    assert!(polish.iter().all(|g| !g.name.is_empty()));
}

/// What the Filters page actually consumes: one read, grouped for rendering.
#[test]
fn assembled_catalogue_is_renderable() {
    for set in [FilterSet::Http, FilterSet::Dns] {
        if catalogue_path(set.db_path()).is_none() {
            continue;
        }
        let catalogue = Catalogue::open_set(set).expect("should open read-only");
        let read = catalogue.read(&Locale::from_env()).expect("should read");

        assert!(read.user_rules.is_some(), "{set:?}: no user-rules row");

        let grouped = read.grouped();
        assert!(!grouped.is_empty(), "{set:?}: nothing to render");
        for (group, filters) in &grouped {
            assert!(
                !filters.is_empty(),
                "{set:?}: group {:?} would render as an empty heading",
                group.name
            );
        }

        // Grouping must not lose or duplicate anything.
        let rendered: usize = grouped.iter().map(|(_, f)| f.len()).sum();
        assert_eq!(
            rendered,
            read.filters.len(),
            "{set:?}: {} filters read but {rendered} rendered",
            read.filters.len()
        );

        eprintln!(
            "{set:?}: {} groups rendered, {rendered} filters",
            grouped.len()
        );
    }
}

/// The single-filter re-read used to verify a toggle.
#[test]
fn reads_one_filter_state() {
    let Some(catalogue) = open(paths::filters_db()) else {
        return;
    };
    let filters = catalogue.filters(&Locale::english()).expect("read");
    let first = filters.first().expect("at least one filter");

    let state = catalogue
        .state(first.id)
        .expect("lookup should not error")
        .expect("filter should exist");
    assert_eq!(state.enabled, first.enabled);
    assert_eq!(state.installed, first.installed);

    assert!(
        catalogue.state(424_242).expect("not an error").is_none(),
        "an unknown id should be absent, not an error"
    );
}

/// The databases are the live daemon's. Opening read-only must not create
/// side-car files (a read-write open would produce -wal/-shm).
#[test]
fn opening_does_not_create_wal_files() {
    let Some(path) = catalogue_path(paths::filters_db()) else {
        return;
    };
    let wal = path.with_extension("db-wal");
    let shm = path.with_extension("db-shm");
    let wal_before = wal.exists();
    let shm_before = shm.exists();

    {
        let catalogue = Catalogue::open(&path).expect("should open read-only");
        let _ = catalogue.read(&Locale::from_env()).expect("should read");
    }

    assert_eq!(wal.exists(), wal_before, "read-only open created {wal:?}");
    assert_eq!(shm.exists(), shm_before, "read-only open created {shm:?}");
}
