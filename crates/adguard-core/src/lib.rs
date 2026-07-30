//! Logic layer for AdGuard UI.
//!
//! Deliberately free of any GTK dependency so the risky part — parsing
//! `adguard-cli` output — is unit-testable without a display server.
//!
//! See `docs/cli-contract.md` for the measured CLI behaviour this encodes.

pub mod cli;
pub mod config;
pub mod filters;
pub mod locale;
pub mod model;
pub mod paths;

pub use cli::{Applied, Cli, Error};
pub use config::{AddressPlan, AuthState, Config};
pub use filters::Catalogue;
pub use locale::Locale;
pub use model::{
    Filter, FilterAction, FilterCatalogue, FilterGroup, FilterSet, FilterState, Kind, ProxyStatus,
    Setting, SettingGroup, Toggle, ADVANCED,
};
