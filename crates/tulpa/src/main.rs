//! The macOS body for a remote AI agent.
//!
//! With `[tray] enabled = true` in the local config the app runs the menu-bar
//! presenter; otherwise it reports its skeleton status and exits. A tray that
//! is enabled on a non-macOS build is refused loudly instead of faking it.

use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_CONFIG_PATH: &str = "~/.config/tulpa/config.toml";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _printed: io::Result<()> = writeln!(stderr, "tulpa: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config_path = config_path_from_args()?;
    let input = match fs::read_to_string(&config_path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut stdout = io::stdout().lock();
            let _printed: io::Result<()> = writeln!(
                stdout,
                "tulpa: no config at {}; no organs are active.",
                config_path.display()
            );
            return Ok(());
        }
        Err(error) => Err(error)?,
    };

    let config = tulpa_tray::TrayAppConfig::from_toml(&input)?;
    if !config.tray.enabled {
        let mut stdout = io::stdout().lock();
        let _printed: io::Result<()> = writeln!(
            stdout,
            "tulpa: menu bar tray is disabled in local config; no organs are active."
        );
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        tulpa_tray::macos::run(&config, &config_path)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the tray organ requires macOS; refusing to pretend otherwise",
        ))?
    }
}

fn config_path_from_args() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = std::env::args_os();
    let _program = args.next();
    match args.next() {
        Some(path) if args.next().is_none() => Ok(PathBuf::from(path)),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: tulpa [config.toml]",
        ))?,
        None => Ok(tulpa_tray::config::expand_tilde(DEFAULT_CONFIG_PATH)),
    }
}
