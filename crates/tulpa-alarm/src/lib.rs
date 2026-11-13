//! Local timers and wake-ups that ring even when the SSH link is down.
//!
//! Scheduling and delivery must not depend on a live remote connection. No timer
//! storage or local ringing is implemented yet.

/// Initial capability boundary for the alarm organ; no implementation is provided.
pub trait Alarm {
    /// A typed local timer or wake-up request.
    type Request;
    /// A handle identifying the scheduled local alarm.
    type Handle;
    /// A scheduling or local delivery setup failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Schedules an alarm for local delivery independently of the link.
    ///
    /// # Errors
    ///
    /// Returns an error when the alarm cannot be scheduled locally.
    fn schedule(&mut self, request: Self::Request) -> Result<Self::Handle, Self::Error>;
}
