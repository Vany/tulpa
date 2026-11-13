//! The body's vitals: link state, organ state, CPU, thermal condition, and battery.
//!
//! Implementations report only permitted local signals over the link. No system
//! vitals are queried yet.

/// Initial capability boundary for the health organ; no implementation is provided.
pub trait Health {
    /// A typed snapshot of body and organ vitals.
    type Snapshot;
    /// A local health query failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Reads the body's available vitals.
    ///
    /// # Errors
    ///
    /// Returns an error when a required health signal cannot be queried.
    fn snapshot(&self) -> Result<Self::Snapshot, Self::Error>;
}
