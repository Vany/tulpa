//! Helpers the unit tests of more than one module share.

use std::path::PathBuf;

use crate::event::Event;

pub(crate) fn temp_state(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tulpa-hear-{tag}-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("me")).ok();
    dir
}

pub(crate) fn speakers() -> Event {
    Event::Device {
        name: "Speakers".to_owned(),
        rate: 48_000,
        channels: 2,
    }
}
