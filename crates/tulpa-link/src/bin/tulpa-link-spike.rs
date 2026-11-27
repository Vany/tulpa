//! Manual harness for OpenSSH identities, discovery, and the transport spike.

use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tulpa_link::{
    Endpoint, ExchangeConfig, HostKeyPrompt, LocalConfig, PassphraseError, PassphraseProvider,
    Route, configured_user, discover, exchange_line, home_directory, select_identity_files,
};
use zeroize::Zeroizing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [command, path] if command == "discover" => run_discovery(Path::new(path)),
        [command, path] if command == "exchange" => run_exchange(Path::new(path)).await,
        [command, host] if command == "identities" => run_identities(host, None),
        [command, host, home] if command == "identities" => {
            run_identities(host, Some(PathBuf::from(home)))
        }
        _ => usage(),
    }
}

fn usage() -> Result<(), Box<dyn Error>> {
    writeln!(
        io::stderr().lock(),
        "usage: tulpa-link-spike discover CONFIG_PATH | exchange CONFIG_PATH | identities HOST [HOME]"
    )?;
    Err("invalid arguments".into())
}

fn run_identities(host: &str, home: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let home = home.map_or_else(home_directory, Ok)?;
    let selection = select_identity_files(&home, host)?;
    for identity in &selection.files {
        if let Some(fingerprint) = identity.fingerprint() {
            writeln!(
                io::stdout().lock(),
                "{} {}",
                identity.path().display(),
                fingerprint
            )?;
        }
    }
    Ok(())
}

fn run_discovery(path: &Path) -> Result<(), Box<dyn Error>> {
    let config = read_config(path)?;
    let selected = discover(&config.link.discovery())?;
    let route = match selected.route {
        Route::LanBonjour => "LAN Bonjour",
        Route::ConfiguredTunnel => "configured tunnel",
        _ => "unknown route",
    };
    writeln!(
        io::stdout().lock(),
        "route: {route}; selected {}:{}",
        selected.endpoint.host,
        selected.endpoint.port
    )?;
    Ok(())
}

async fn run_exchange(path: &Path) -> Result<(), Box<dyn Error>> {
    let config = read_config(path)?;
    let home = home_directory()?;
    let target = Endpoint::new(&config.link.target_host, config.link.target_port);
    let exchange = if config.link.host.is_empty() {
        let selected = discover(&config.link.discovery())?;
        let user = configured_user(&config.link.user, &home, &selected.endpoint.host)
            .ok_or("no SSH user configured")?;
        ExchangeConfig::new(selected.endpoint, user, target, home)
    } else {
        let user_override = (!config.link.user.is_empty()).then_some(config.link.user.as_str());
        ExchangeConfig::from_alias(&config.link.host, user_override, target, home)?
    }
    .with_strict_host_key_checking(&config.link.strict_host_key_checking)?;
    let host_prompt = CliHostPrompt;
    let mut passphrases = CliPassphrases;
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        exchange_line(
            &exchange,
            Box::new(host_prompt),
            &mut passphrases,
            b"tulpa spike\n",
        ),
    )
    .await??;
    io::stdout().lock().write_all(&reply)?;
    Ok(())
}

fn read_config(path: &Path) -> Result<LocalConfig, Box<dyn Error>> {
    let input = std::fs::read_to_string(path)?;
    Ok(LocalConfig::from_toml(&input)?)
}

struct CliHostPrompt;

impl HostKeyPrompt for CliHostPrompt {
    fn confirm_unknown_host(&mut self, host: &str, port: u16, fingerprint: &str) -> bool {
        let mut stderr = io::stderr().lock();
        if writeln!(
            stderr,
            "Unknown SSH host {host}:{port}. SHA-256 fingerprint: {fingerprint}.\nTrust and add it to known_hosts? Type yes or no."
        )
        .is_err()
        {
            return false;
        }
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .is_ok_and(|count| count > 0 && answer.trim().eq_ignore_ascii_case("yes"))
    }
}

struct CliPassphrases;

impl PassphraseProvider for CliPassphrases {
    fn keychain_passphrase(
        &mut self,
        key_path: &Path,
    ) -> Result<Option<Zeroizing<String>>, PassphraseError> {
        // The Keychain lookup exists only on macOS; elsewhere the provider
        // falls through to the interactive prompt.
        #[cfg(target_os = "macos")]
        {
            tulpa_secrets::openssh_passphrase(key_path)
                .map_err(|error| PassphraseError::new(error.to_string()))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = key_path;
            Ok(None)
        }
    }

    fn prompt_passphrase(&mut self, key_path: &Path) -> Result<Zeroizing<String>, PassphraseError> {
        rpassword::prompt_password(format!("Enter passphrase for {}: ", key_path.display()))
            .map(Zeroizing::new)
            .map_err(|error| PassphraseError::new(error.to_string()))
    }
}
