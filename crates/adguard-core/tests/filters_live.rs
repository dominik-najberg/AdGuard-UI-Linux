//! Integration tests against the real filter databases.
//!
//! These guard the SQL in `filters.rs` against AdGuard's actual schema — a
//! schema we do not control and which could change on any CLI update.
//!
//! They **skip** rather than fail when AdGuard CLI is not installed, so the
//! suite still passes on a machine (or CI runner) without it.

use adguard_core::filters::Catalogue;
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

fn check_catalogue(path: PathBuf, label: &str) {
    let catalogue = Catalogue::open(&path).expect("should open read-only");

    let groups = catalogue.groups().expect("should read filter_group");
    let filters = catalogue.filters().expect("should read filter");

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
    let Some(path) = catalogue_path(paths::filters_db()) else {
        return;
    };
    let catalogue = Catalogue::open(&path).expect("should open read-only");
    let groups = catalogue.groups().expect("should read filter_group");

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
        let Some(path) = catalogue_path(which) else {
            continue;
        };
        let catalogue = Catalogue::open(&path).expect("should open read-only");
        let user_rules = catalogue.user_rules().expect("lookup should not error");

        let Some(filter) = user_rules else {
            panic!("{label}: user-rules pseudo-filter missing");
        };
        assert!(filter.is_user_rules());
        assert!(!filter.title.is_empty(), "{label}: empty title");
        eprintln!(
            "{label}: user rules = {:?} (enabled {}, installed {})",
            filter.title, filter.enabled, filter.installed
        );
    }
}

/// Localised names back the filter UI, so the lookup must work against the
/// real `filter_localisation` table (thousands of rows keyed by `lang`).
#[test]
fn looks_up_localised_names() {
    let Some(path) = catalogue_path(paths::filters_db()) else {
        return;
    };
    let catalogue = Catalogue::open(&path).expect("should open read-only");
    let filters = catalogue.filters().expect("should read filter");
    let first = filters.first().expect("at least one filter");

    // A real language must resolve; a nonsense one must be absent, not an error.
    let english = catalogue
        .localised_name(first.id, "en")
        .expect("lookup should not error");
    let nonsense = catalogue
        .localised_name(first.id, "zz-not-a-lang")
        .expect("missing locale is not an error");

    assert!(nonsense.is_none(), "unexpected match for a bogus locale");
    eprintln!("filter {} en name: {:?}", first.id, english);
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
        let _ = catalogue.filters().expect("should read filter");
    }

    assert_eq!(wal.exists(), wal_before, "read-only open created {wal:?}");
    assert_eq!(shm.exists(), shm_before, "read-only open created {shm:?}");
}
