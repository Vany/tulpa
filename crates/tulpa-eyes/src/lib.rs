//! Eyes across all displays, with excluded windows and camera capture on request.
//!
//! Implementations must honor local exclusions and macOS capture permissions.
//! No screen or camera capture is implemented yet.

/// Initial capability boundary for the eyes organ; no implementation is provided.
pub trait Eyes {
    /// A typed display or explicitly requested camera capture.
    type Request;
    /// Captured frames with their display or camera metadata.
    type Capture;
    /// A permission or capture failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Captures only sources permitted by the request and local policy.
    ///
    /// # Errors
    ///
    /// Returns an error when permission is missing or capture fails.
    fn capture(&mut self, request: &Self::Request) -> Result<Self::Capture, Self::Error>;
}
