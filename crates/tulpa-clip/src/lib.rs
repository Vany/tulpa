//! Clipboard exchange under explicit local policy.
//!
//! Implementations must gate both reads and writes on human controls. This crate
//! does not access the system clipboard yet.

/// Initial capability boundary for the clip organ; no implementation is provided.
pub trait Clipboard {
    /// Typed clipboard contents with an explicit format.
    type Contents;
    /// A policy or clipboard access failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Reads clipboard contents allowed by local policy.
    ///
    /// # Errors
    ///
    /// Returns an error when policy rejects access or the clipboard is unavailable.
    fn read(&self) -> Result<Self::Contents, Self::Error>;

    /// Writes clipboard contents allowed by local policy.
    ///
    /// # Errors
    ///
    /// Returns an error when policy rejects access or the clipboard is unavailable.
    fn write(&mut self, contents: &Self::Contents) -> Result<(), Self::Error>;
}
