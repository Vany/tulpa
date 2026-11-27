use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use russh::client::Handler;
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::ssh_key::HashAlg;
use russh::keys::{PublicKeyOrCertificate, check_known_hosts_path};

/// OpenSSH's action for a host absent from `known_hosts`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum StrictHostKeyChecking {
    /// Ask whether to trust and record a previously unknown host key.
    #[default]
    Ask,
    /// Refuse an unknown host key.
    Yes,
    /// Refuse unknown keys without prompting. Changed keys are always refused.
    No,
}

impl FromStr for StrictHostKeyChecking {
    type Err = HostKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "yes" => Ok(Self::Yes),
            "no" => Ok(Self::No),
            _ => Err(HostKeyError(
                "StrictHostKeyChecking must be ask, yes, or no".to_owned(),
            )),
        }
    }
}

/// A decision request for a host key absent from native `known_hosts`.
pub trait HostKeyPrompt: Send {
    /// Returns true only when the human explicitly accepts the displayed key.
    fn confirm_unknown_host(&mut self, host: &str, port: u16, fingerprint: &str) -> bool;
}

/// A host-key prompt shared serially across all hops in one route.
pub type SharedHostKeyPrompt = std::sync::Arc<tokio::sync::Mutex<Box<dyn HostKeyPrompt + Send>>>;

/// A `russh` handler backed by a native OpenSSH `known_hosts` file.
pub struct KnownHostsHandler {
    host: String,
    port: u16,
    path: PathBuf,
    strict: StrictHostKeyChecking,
    prompt: SharedHostKeyPrompt,
}

impl fmt::Debug for KnownHostsHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KnownHostsHandler")
            .field("strict", &self.strict)
            .finish_non_exhaustive()
    }
}

impl KnownHostsHandler {
    /// Creates a handler for one server and a user-selected prompt implementation.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        path: PathBuf,
        strict: StrictHostKeyChecking,
        prompt: SharedHostKeyPrompt,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            path,
            strict,
            prompt,
        }
    }
}

impl Handler for KnownHostsHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        match check_known_hosts_path(&self.host, self.port, &key, &self.path) {
            Ok(true) => Ok(true),
            Ok(false) if self.strict == StrictHostKeyChecking::Ask => {
                let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
                let accepted = self.prompt.lock().await.confirm_unknown_host(
                    &self.host,
                    self.port,
                    &fingerprint,
                );
                if !accepted {
                    return Ok(false);
                }
                Ok(learn_known_hosts_path(&self.host, self.port, &key, &self.path).is_ok())
            }
            Ok(false) | Err(_) => Ok(false),
        }
    }
}

/// Invalid host-key policy text.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HostKeyError(String);

impl fmt::Display for HostKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for HostKeyError {}

/// Returns the native known-hosts path below an SSH home directory.
#[must_use]
pub fn default_known_hosts_path(home: &Path) -> PathBuf {
    home.join(".ssh").join("known_hosts")
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use russh::keys::ssh_key::{PublicKey, public::Ed25519PublicKey};

    use super::*;

    #[test]
    fn native_lookup_supports_hashed_hosts() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("known_hosts");
        let key = PublicKey::from(Ed25519PublicKey([7_u8; 32]));
        std::fs::write(&path, format!("example.test {}\n", key.to_openssh()?))?;
        let status = Command::new("ssh-keygen")
            .arg("-H")
            .arg("-f")
            .arg(&path)
            .status()?;
        assert!(status.success());

        assert!(check_known_hosts_path("example.test", 22, &key, &path)?);
        Ok(())
    }

    #[tokio::test]
    async fn changed_key_is_refused_without_prompt() -> Result<(), Box<dyn std::error::Error>> {
        struct RejectPrompt;
        impl HostKeyPrompt for RejectPrompt {
            fn confirm_unknown_host(&mut self, _: &str, _: u16, _: &str) -> bool {
                false
            }
        }

        let temp = tempfile::tempdir()?;
        let path = temp.path().join("known_hosts");
        let recorded = PublicKey::from(Ed25519PublicKey([7_u8; 32]));
        let changed = PublicKey::from(Ed25519PublicKey([8_u8; 32]));
        std::fs::write(&path, format!("example.test {}\n", recorded.to_openssh()?))?;
        let handler = KnownHostsHandler::new(
            "example.test",
            22,
            path,
            StrictHostKeyChecking::Ask,
            std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(RejectPrompt))),
        );

        let mut handler = handler;
        assert!(!handler.check_server_key(&changed.into()).await?);
        Ok(())
    }
}
