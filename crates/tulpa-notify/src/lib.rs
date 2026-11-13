//! macOS notifications shown locally and read back only when permitted.
//!
//! Incoming notification access requires explicit local permission. No system
//! notifications are shown or read yet.

/// Initial capability boundary for the notify organ; no implementation is provided.
pub trait Notify {
    /// A typed notification to display to the human.
    type Notification;
    /// A permission or notification delivery failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Shows a notification through the permitted local notification service.
    ///
    /// # Errors
    ///
    /// Returns an error if notification permission or delivery is unavailable.
    fn show(&mut self, notification: &Self::Notification) -> Result<(), Self::Error>;
}
