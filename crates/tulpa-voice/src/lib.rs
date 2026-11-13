//! Interruptible playback of speech audio arriving over the link.
//!
//! Implementations must release queued audio when interrupted. This crate does
//! not open an audio device or play sound yet.

/// Initial capability boundary for the voice organ; no implementation is provided.
pub trait Voice {
    /// A typed audio chunk including its playback format.
    type Audio;
    /// An audio format or playback-device failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Queues an incoming audio chunk for playback.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is unsupported or playback is unavailable.
    fn play(&mut self, audio: &Self::Audio) -> Result<(), Self::Error>;

    /// Interrupts current playback and discards queued speech.
    ///
    /// # Errors
    ///
    /// Returns an error if the playback device cannot be stopped.
    fn interrupt(&mut self) -> Result<(), Self::Error>;
}
