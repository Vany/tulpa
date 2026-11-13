//! Calls through Zoom, Meet, or `FaceTime`, with optional transcription and hints.
//!
//! Implementations must obtain permission before recording both sides or enabling
//! live assistance. No call joining, recording, or transcription is implemented yet.

/// Initial capability boundary for the calls organ; no implementation is provided.
pub trait Calls {
    /// A typed join or answer request with explicit recording permissions.
    type Request;
    /// A handle to the call and its permitted recording or assistance streams.
    type Session;
    /// A policy, application, or call setup failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Joins or answers a call under local policy and human permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when policy rejects the request or call setup fails.
    fn join(&mut self, request: &Self::Request) -> Result<Self::Session, Self::Error>;
}
