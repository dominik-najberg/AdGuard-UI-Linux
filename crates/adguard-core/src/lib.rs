//! Logic layer for AdGuard UI.
//!
//! Deliberately free of any GTK dependency so the risky part — parsing
//! `adguard-cli` output — is unit-testable without a display server.
//!
//! See `docs/cli-contract.md` for the measured CLI behaviour this encodes.

pub mod autostart;
pub mod browser;
pub mod cli;
pub mod config;
pub mod filters;
pub mod helper;
pub mod locale;
pub mod model;
pub mod orphan;
pub mod paths;
pub mod release;
pub mod trust;
pub mod userscripts;
pub mod window_state;
pub mod zip;

pub use autostart::Autostart;
pub use browser::BrowserIntegration;
pub use cli::{Activation, Applied, Cli, Error};
pub use config::{AddressPlan, AuthState, Config, DnsListenPort, Watch};
pub use helper::RootHelper;
pub use orphan::Daemon;
pub use release::{Release, Standing};
pub use trust::CaTrust;
pub use window_state::{Geometry, WindowState};
pub use filters::Catalogue;
pub use locale::Locale;
pub use model::{
    ComponentUpdate, Consent, Filter, FilterAction, FilterCatalogue, FilterGroup, FilterSet,
    FilterState, Kind, License, ProxyStatus, Recommended, Setting, SettingGroup, Toggle,
    UpdatePart, UpdateReport, Userscript, Verdict, ADVANCED, ANNOYANCE_TERMS, FILTER_SETTINGS,
    RECOMMENDED, SETUP, STEALTH,
};
