//! Policy-bound hands for parallel shell, filesystem, git, tmux, and GUI jobs.
//!
//! Implementations must enforce named commands, allowed roots, and listed tmux
//! windows locally. Each returned job must carry its ID, cancellation, and output
//! stream. This crate does not execute jobs yet.

/// Initial capability boundary for the hands organ; no implementation is provided.
pub trait Hands {
    /// A typed request that the implementation checks against local policy.
    type Request;
    /// A handle to an independently cancellable job with streamed output.
    type Job;
    /// A policy rejection or job submission failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Submits a job without waiting for other jobs to finish.
    ///
    /// # Errors
    ///
    /// Returns an error when policy rejects the request or the job cannot start.
    fn submit(&self, request: Self::Request) -> Result<Self::Job, Self::Error>;
}
