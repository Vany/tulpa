//! Microphone audio through VAD and streaming speech recognition.
//!
//! Implementations emit partial and final transcripts through the link and must
//! stop capture on pause. Capture will run in the bundled ears helper so the app
//! owns its microphone permission. No capture or recognition is implemented yet.

/// Initial capability boundary for the ears organ; no implementation is provided.
pub trait Ears {
    /// A typed partial or final transcript event.
    type Transcript;
    /// A permission, capture, or recognition failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Receives the next transcript event from a permitted capture session.
    ///
    /// # Errors
    ///
    /// Returns an error if microphone access or speech recognition fails.
    fn next_transcript(&mut self) -> Result<Self::Transcript, Self::Error>;
}
