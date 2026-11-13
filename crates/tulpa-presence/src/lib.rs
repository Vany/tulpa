//! Human presence from idle time, screen lock, active display, and proximity.
//!
//! Implementations must respect permissions for Bluetooth and Wi-Fi signals.
//! No presence sensors are read yet.

/// Initial capability boundary for the presence organ; no implementation is provided.
pub trait Presence {
    /// A typed snapshot of available presence signals.
    type Snapshot;
    /// A permission or sensor query failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Reads the currently permitted presence signals.
    ///
    /// # Errors
    ///
    /// Returns an error when a permitted sensor cannot be queried.
    fn snapshot(&self) -> Result<Self::Snapshot, Self::Error>;
}
