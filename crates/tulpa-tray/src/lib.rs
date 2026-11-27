//! A menu-bar presence exposing organ state, pause, stop, and a local activity log.
//!
//! The crate is split so everything except the presenter is platform-free and
//! unit-testable: [`state`] is the vocabulary, [`model`] is the menu state
//! machine, [`icon`] renders the state disc, [`log`] appends human-control
//! events, [`checkin`] writes the marker an installer waits for, [`config`]
//! reads local TOML, [`askdialog`] decides when the owner's Allow/Deny
//! dialog shows and goes, and [`saylog`] keeps the log window's lines — what
//! the ears heard and each phrase's fate, what Tulpa said, the lines the
//! operator types back. The [`macos`] module wires those
//! to `tray-icon`/`muda`/`tao`; other platforms build the pure core only.

pub mod askdialog;
pub mod buildcheck;
pub mod checkin;
pub mod config;
pub mod devices;
pub mod ears;
pub mod eyes;
pub mod files;
pub mod flow;
pub mod forward;
pub mod hands;
pub mod icon;
pub mod keepawake;
pub mod lifecycle;
pub mod link;
pub mod linkserve;
pub mod log;
pub mod model;
pub mod pause;
pub mod saylog;
pub mod state;
pub mod switch;
#[cfg(test)]
pub mod testutil;
pub mod voicestream;

#[cfg(target_os = "macos")]
pub mod macos;

pub use config::{ConfigError, TrayAppConfig};
pub use log::{ActivityLog, LogError};
pub use model::{MenuEntry, TrayEvent, TrayModel};
pub use state::{LinkState, ORGANS, Organ, OrganMode};
