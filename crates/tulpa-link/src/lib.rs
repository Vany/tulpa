//! One outbound SSH connection carrying a channel for each organ.
//!
//! The transport spike verifies pinned host keys before authentication, uses a
//! local protected signer, and supports a single direct-TCP/IP exchange.

#[cfg(target_os = "macos")]
mod client;
mod config;
mod discovery;
#[cfg(target_os = "macos")]
mod signer;
#[cfg(target_os = "macos")]
pub use client::{
    Endpoint, ExchangeConfig, HostKeyError, LinkError, PinnedHostKeyHandler, exchange_line,
};
pub use config::{ConfigError, LinkConfig, LocalConfig};
#[cfg(target_os = "macos")]
pub use discovery::{DiscoveryConfig, DiscoveryError, Route, SelectedEndpoint, discover};
#[cfg(target_os = "macos")]
pub use signer::{ProtectedP256Signer, SignerError, public_key_from_sec1};

/// Initial capability boundary for the link organ; no implementation is provided.
pub trait Link {
    /// A typed organ identifier used to select its channel.
    type Organ;
    /// The bidirectional stream carried by an SSH channel.
    type Channel: std::io::Read + std::io::Write + Send;
    /// A transport, authentication, or host-key verification failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Opens an organ channel on the authenticated outbound connection.
    ///
    /// # Errors
    ///
    /// Returns an error if trust verification, authentication, or channel creation fails.
    fn open_channel(&mut self, organ: &Self::Organ) -> Result<Self::Channel, Self::Error>;
}
