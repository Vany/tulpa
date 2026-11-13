//! One outbound SSH connection carrying a channel for each organ.
//!
//! Implementations must verify pinned host keys before opening channels, use
//! a local protected signer, and choose a LAN or jump-host route at runtime.
//! No transport implementation is included yet.

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
