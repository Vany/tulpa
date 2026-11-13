//! A drop zone for incoming files and sharing for outgoing files inside allowed roots.
//!
//! Implementations must resolve paths and enforce local roots before any access.
//! No filesystem transfers are implemented yet.

/// Initial capability boundary for the files organ; no implementation is provided.
pub trait Files {
    /// A typed incoming or outgoing transfer request.
    type Request;
    /// A handle to a policy-approved file transfer.
    type Transfer;
    /// A policy, path resolution, or transfer setup failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Starts a transfer after enforcing the configured filesystem roots.
    ///
    /// # Errors
    ///
    /// Returns an error when a path escapes an allowed root or transfer setup fails.
    fn transfer(&mut self, request: &Self::Request) -> Result<Self::Transfer, Self::Error>;
}
