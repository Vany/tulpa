//! A menu-bar presence exposing organ state, pause, stop, and a local activity log.
//!
//! Implementations must keep human controls visible and pause all senses on the
//! pause hotkey. No menu-bar UI, hotkey, or log writer is implemented yet.

/// Initial capability boundary for the tray organ; no implementation is provided.
pub trait Tray {
    /// A typed view of live organ states and available human controls.
    type State;
    /// A menu-bar presentation failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Updates the human-visible view of the body.
    ///
    /// # Errors
    ///
    /// Returns an error when the menu-bar view cannot be updated.
    fn update(&mut self, state: &Self::State) -> Result<(), Self::Error>;
}
