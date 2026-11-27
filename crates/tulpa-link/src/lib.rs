//! One outbound SSH connection carrying a channel for each organ.
//!
//! The transport spike uses native OpenSSH identities and known-host files,
//! chooses a live route, and supports a single direct-TCP/IP exchange.

mod alias;
mod client;
mod config;
mod discovery;
mod identity;
mod known_hosts;
mod passphrase;
mod transport;
pub use alias::{
    AliasError, JumpReference, ResolvedHost, expand_proxy_command, parse_jump_reference,
    resolve_host,
};
pub use client::{
    Endpoint, ExchangeConfig, LinkError, configured_user, default_user, exchange_line,
    home_directory,
};
pub use config::{ConfigError, LinkConfig, LocalConfig};
pub use discovery::{DiscoveryConfig, DiscoveryError, Route, SelectedEndpoint, discover};
pub use identity::{IdentityConfigError, IdentityFile, IdentitySelection, select_identity_files};
pub use known_hosts::{
    HostKeyError, HostKeyPrompt, KnownHostsHandler, SharedHostKeyPrompt, StrictHostKeyChecking,
    default_known_hosts_path,
};
pub use passphrase::{PassphraseError, PassphraseProvider, load_private_key};
pub use transport::{ProxyTransport, TransportError};

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
