//! Policy-bound hands: `read`, `list`, `git.status|diff|log` and `run <name>`
//! inside the roots the local policy lists (design §2, §10), and the data
//! stream's `xfer.put` / `xfer.get` for big files beside the control line.
//!
//! Everything that leaves goes through one gate, in this order: paused →
//! refused; verb posture (off/ask/on) → a local click for `ask`; real path of
//! the opened descriptor inside a root and outside the deny list; git without
//! repo-supplied code; `run` with a cleared environment; the scrub; the 256 KB
//! cap; one activity line without the payload. Pausing kills a running child.

pub mod deny;
pub mod engine;
pub mod exec;
pub mod fsx;
pub mod policy;
pub mod scrub;
pub mod xfer;

pub use deny::is_denied;
pub use engine::{AskAnswer, Asker, Engine, Journal, WorkHook, shelf_get};
pub use policy::{NamedCommand, Policy, PolicyError, Posture};
pub use scrub::scrub_secrets;
pub use xfer::{Closer, DataConn, DataDial, HalfWrite, Shelf, Ticket, Xfer};

/// What one reply may carry, after the scrub (design §2).
pub const REPLY_CAP_BYTES: usize = 256 * 1024;
