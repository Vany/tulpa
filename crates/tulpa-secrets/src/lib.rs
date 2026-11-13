//! Keychain-guarded secret operations without exporting raw keys to the agent.
//!
//! The macOS protected-key implementation exposes only public-key bytes and
//! signing operations. Private-key export and deletion are deliberately absent.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{KeyBackend, KeyError, PlatformError, ProtectedP256Key};

/// Initial capability boundary for the secrets organ; no implementation is provided.
pub trait Secrets {
    /// A typed signing request referring to a local protected key.
    type Request;
    /// A signature that contains no private key material.
    type Signature;
    /// A permission, key lookup, or signing failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Signs through a protected local key without exporting it.
    ///
    /// # Errors
    ///
    /// Returns an error when access is denied, the key is absent, or signing fails.
    fn sign(&self, request: &Self::Request) -> Result<Self::Signature, Self::Error>;
}
