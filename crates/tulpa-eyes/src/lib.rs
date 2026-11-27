//! Eyes across all displays, with excluded windows and camera capture on request.
//!
//! Implementations must honor local exclusions and macOS capture permissions.
//!
//! Built so far: one still from the camera on request. The app bundle's
//! camera helper takes it, the tray runs that helper for the link verb
//! `eyes.photo`, and [`photo`] keeps the JPEG in a private directory until
//! the agent fetches it. Screen capture is not built yet; [`Eyes`] remains
//! its boundary.

pub mod photo;

pub use photo::{PHOTO_MAX_AGE, PHOTO_MAX_BYTES, Photo, PhotoError, PhotoStore, is_photo_name};

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
