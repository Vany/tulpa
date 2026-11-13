//! The macOS body for a remote AI agent.
//!
//! This skeleton only reports its status and exits without requesting permissions.

use std::io::{self, Write};

fn main() -> io::Result<()> {
    writeln!(
        io::stdout().lock(),
        "tulpa: workspace skeleton; no organs are active."
    )
}
