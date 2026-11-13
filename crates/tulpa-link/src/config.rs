use std::error::Error;
use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::{DiscoveryConfig, Endpoint};

const DEFAULT_MDNS_SERVICE: &str = "_tulpa-ssh._tcp.local.";
const DEFAULT_BROWSE_TIMEOUT_MS: u64 = 1_500;
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 500;

/// The subset of local TOML configuration used by the transport spike.
#[derive(Deserialize)]
#[non_exhaustive]
pub struct LocalConfig {
    /// SSH and discovery settings. Other organ sections are left to their crates.
    pub link: LinkConfig,
}

impl fmt::Debug for LocalConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalConfig { link: redacted }")
    }
}

impl LocalConfig {
    /// Parses local TOML without reading any repository or default file.
    ///
    /// # Errors
    ///
    /// Returns an error when the TOML or a field's type is invalid.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        toml::from_str(input).map_err(|error| ConfigError(error.to_string()))
    }
}

/// Local link configuration. Values are never serialized or logged by this type.
#[derive(Deserialize)]
#[non_exhaustive]
pub struct LinkConfig {
    /// Whether the product should start the link; explicit spike commands ignore it.
    #[serde(default)]
    pub enabled: bool,
    /// SSH user, supplied only by local configuration.
    #[serde(default)]
    pub user: String,
    /// OpenSSH host public key used as a fail-closed pin.
    #[serde(default)]
    pub pinned_host_key: String,
    /// Stable label for the protected P-256 key.
    #[serde(default)]
    pub signing_key_label: String,
    /// Bonjour service type used on the LAN.
    #[serde(default = "default_mdns_service")]
    pub mdns_service: String,
    /// Maximum Bonjour browse time in milliseconds.
    #[serde(default = "default_browse_timeout_ms")]
    pub discovery_timeout_ms: u64,
    /// Maximum time for each endpoint probe in milliseconds.
    #[serde(default = "default_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
    /// All possible tunnel entry points; selection happens on every run.
    #[serde(default)]
    pub tunnel_entry_points: Vec<Endpoint>,
    /// Host reached through the authenticated direct-TCP/IP channel.
    #[serde(default)]
    pub target_host: String,
    /// Port reached through the authenticated direct-TCP/IP channel.
    #[serde(default)]
    pub target_port: u16,
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinkConfig")
            .field("enabled", &self.enabled)
            .field("tunnel_entry_point_count", &self.tunnel_entry_points.len())
            .finish_non_exhaustive()
    }
}

impl LinkConfig {
    /// Builds the runtime discovery configuration from local values.
    #[must_use]
    pub fn discovery(&self) -> DiscoveryConfig {
        DiscoveryConfig {
            mdns_service: self.mdns_service.clone(),
            tunnel_entry_points: self.tunnel_entry_points.clone(),
            browse_timeout: Duration::from_millis(self.discovery_timeout_ms),
            probe_timeout: Duration::from_millis(self.probe_timeout_ms),
        }
    }
}

/// A local TOML parsing failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid local configuration: {}", self.0)
    }
}

impl Error for ConfigError {}

fn default_mdns_service() -> String {
    DEFAULT_MDNS_SERVICE.to_owned()
}

const fn default_browse_timeout_ms() -> u64 {
    DEFAULT_BROWSE_TIMEOUT_MS
}

const fn default_probe_timeout_ms() -> u64 {
    DEFAULT_PROBE_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_many_tunnel_endpoints_without_embedding_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = LocalConfig::from_toml(
            r#"
            [link]
            mdns_service = "_test-tulpa._tcp.local."
            tunnel_entry_points = [
                { host = "FIRST_PLACEHOLDER", port = 2001 },
                { host = "SECOND_PLACEHOLDER", port = 2002 },
            ]
            "#,
        )?;

        assert_eq!(config.link.tunnel_entry_points.len(), 2);
        assert_eq!(
            config
                .link
                .tunnel_entry_points
                .get(1)
                .map(|entry| entry.port),
            Some(2002)
        );
        assert_eq!(config.link.discovery_timeout_ms, 1_500);
        Ok(())
    }
}
