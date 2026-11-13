//! The future bundled microphone helper for tulpa.
//!
//! This skeleton only reports its status and exits without requesting permissions.

use std::io::{self, Write};

fn main() -> io::Result<()> {
    writeln!(
        io::stdout().lock(),
        "tulpa-ears-helper: workspace skeleton; microphone capture is inactive."
    )
}
