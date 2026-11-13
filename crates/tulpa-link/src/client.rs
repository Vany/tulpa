use std::error::Error;
use std::fmt;
use std::sync::Arc;

use russh::client::{self, AuthResult, Handler};
use russh::keys::PublicKeyOrCertificate;
use russh::keys::ssh_key::PublicKey;
use russh::{ChannelMsg, Disconnect};

use crate::signer::{ProtectedP256Signer, SignerError};
use tulpa_secrets::ProtectedP256Key;

const MAX_REPLY_BYTES: usize = 64 * 1024;

/// A remote TCP endpoint supplied by local configuration or live discovery.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
#[non_exhaustive]
pub struct Endpoint {
    /// A DNS name or IP address. No endpoint is embedded in the repository.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

impl Endpoint {
    /// Creates an endpoint from a local configuration or discovery result.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// Configuration for one authenticated direct-TCP/IP exchange.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExchangeConfig {
    /// The selected SSH server.
    pub server: Endpoint,
    /// The SSH user from local configuration.
    pub user: String,
    /// An OpenSSH host public key. Empty or mismatched values are rejected.
    pub pinned_host_key: String,
    /// The target reached through the single `direct-tcpip` channel.
    pub target: Endpoint,
}

impl ExchangeConfig {
    /// Creates the configuration for one direct-TCP/IP exchange.
    #[must_use]
    pub fn new(
        server: Endpoint,
        user: impl Into<String>,
        pinned_host_key: impl Into<String>,
        target: Endpoint,
    ) -> Self {
        Self {
            server,
            user: user.into(),
            pinned_host_key: pinned_host_key.into(),
            target,
        }
    }
}

/// A `russh` client handler that accepts only the configured host public key.
#[derive(Clone, Debug)]
pub struct PinnedHostKeyHandler {
    pinned: Option<PublicKey>,
}

impl PinnedHostKeyHandler {
    /// Parses a pinned OpenSSH host key. An empty value produces a handler that
    /// rejects every server rather than accepting the first key seen.
    ///
    /// # Errors
    ///
    /// Returns an error if a non-empty value is not an OpenSSH public key.
    pub fn new(pinned_host_key: &str) -> Result<Self, HostKeyError> {
        let pinned_host_key = pinned_host_key.trim();
        let pinned = if pinned_host_key.is_empty() {
            None
        } else {
            Some(
                PublicKey::from_openssh(pinned_host_key)
                    .map_err(|error| HostKeyError(error.to_string()))?,
            )
        };
        Ok(Self { pinned })
    }

    /// Returns whether `server_key` exactly matches the configured pin.
    #[must_use]
    pub fn accepts(&self, server_key: &PublicKey) -> bool {
        self.pinned.as_ref() == Some(server_key)
    }
}

impl Handler for PinnedHostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(self.accepts(&server_public_key.public_key()))
    }
}

/// An invalid pinned host key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyError(String);

impl fmt::Display for HostKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid pinned host key: {}", self.0)
    }
}

impl Error for HostKeyError {}

/// Connects once, authenticates with a protected P-256 key, and opens exactly one
/// `direct-tcpip` channel. No session, PTY, listener, or reverse forward is used.
///
/// # Errors
///
/// Returns an error on connection, host-key verification, signing,
/// authentication, channel, or reply-size failure.
pub async fn exchange_line(
    config: &ExchangeConfig,
    key: ProtectedP256Key,
    line: &[u8],
) -> Result<Vec<u8>, LinkError> {
    let handler = PinnedHostKeyHandler::new(&config.pinned_host_key)
        .map_err(|error| LinkError::Configuration(error.to_string()))?;
    let mut session = client::connect(
        Arc::new(client::Config::default()),
        (config.server.host.as_str(), config.server.port),
        handler,
    )
    .await
    .map_err(|error| LinkError::Connection(error.to_string()))?;

    let mut signer = ProtectedP256Signer::new(key).map_err(LinkError::Signing)?;
    let authentication = session
        .authenticate_publickey_with(
            config.user.clone(),
            signer.public_key().clone(),
            None,
            &mut signer,
        )
        .await
        .map_err(LinkError::Signing)?;
    if !matches!(authentication, AuthResult::Success) {
        return Err(LinkError::AuthenticationRejected);
    }

    let mut channel = session
        .channel_open_direct_tcpip(
            config.target.host.clone(),
            u32::from(config.target.port),
            "localhost",
            0,
        )
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    channel
        .data_bytes(line.to_vec())
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    channel
        .eof()
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;

    let mut reply = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if reply.len().saturating_add(data.len()) > MAX_REPLY_BYTES {
                    return Err(LinkError::ReplyTooLarge);
                }
                reply.extend_from_slice(&data);
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            ChannelMsg::OpenFailure(error) => {
                return Err(LinkError::Channel(format!("{error:?}")));
            }
            _ => {}
        }
    }
    channel
        .close()
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    session
        .disconnect(Disconnect::ByApplication, "", "")
        .await
        .map_err(|error| LinkError::Connection(error.to_string()))?;
    Ok(reply)
}

/// A connection, authentication, channel, or exchange failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum LinkError {
    /// Invalid local connection configuration.
    Configuration(String),
    /// The SSH transport failed, including host-key rejection.
    Connection(String),
    /// Protected-key signing or signature encoding failed.
    Signing(SignerError),
    /// The server did not accept the public-key authentication.
    AuthenticationRejected,
    /// The direct-TCP/IP channel failed.
    Channel(String),
    /// The peer exceeded the spike's bounded reply size.
    ReplyTooLarge,
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "configuration: {error}"),
            Self::Connection(error) => write!(formatter, "SSH connection: {error}"),
            Self::Signing(error) => write!(formatter, "SSH signing: {error}"),
            Self::AuthenticationRejected => {
                formatter.write_str("SSH public-key authentication was rejected")
            }
            Self::Channel(error) => write!(formatter, "direct-tcpip channel: {error}"),
            Self::ReplyTooLarge => formatter.write_str("direct-tcpip reply exceeded 64 KiB"),
        }
    }
}

impl Error for LinkError {}

#[cfg(test)]
mod tests {
    use russh::keys::ssh_key::public::EcdsaPublicKey;

    use super::*;

    fn test_public_key(byte: u8) -> Result<PublicKey, Box<dyn std::error::Error>> {
        let signing_key = p256::ecdsa::SigningKey::from_slice(&[byte; 32])?;
        let point = signing_key.verifying_key().to_sec1_point(false);
        Ok(EcdsaPublicKey::from_sec1_bytes(point.as_bytes())?.into())
    }

    #[test]
    fn empty_host_key_rejects_every_server_key() -> Result<(), Box<dyn std::error::Error>> {
        let handler = PinnedHostKeyHandler::new("")?;
        assert!(!handler.accepts(&test_public_key(7)?));
        Ok(())
    }

    #[test]
    fn host_key_pin_requires_an_exact_match() -> Result<(), Box<dyn std::error::Error>> {
        let pinned = test_public_key(7)?;
        let encoded = pinned.to_openssh()?;
        let handler = PinnedHostKeyHandler::new(&encoded)?;
        assert!(handler.accepts(&pinned));
        assert!(!handler.accepts(&test_public_key(8)?));
        Ok(())
    }
}
