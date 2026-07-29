//! Logic layer for AdGuard UI.
//!
//! Deliberately free of any GTK dependency so the risky part — parsing
//! `adguard-cli` output — is unit-testable without a display server.
//!
//! See `docs/cli-contract.md` for the measured CLI behaviour this encodes.

pub mod cli;
pub mod filters;
pub mod locale;
pub mod model;
pub mod paths;

pub use cli::{Cli, Error};
pub use filters::Catalogue;
pub use locale::Locale;
pub use model::{
    Filter, FilterAction, FilterCatalogue, FilterGroup, FilterSet, FilterState, ProxyStatus,
};
