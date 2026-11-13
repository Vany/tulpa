//! Manual harness for the protected-key, discovery, and SSH transport spike.

use std::error::Error;
use std::io::{self, Write};
use std::path::Path;

use p256::ecdsa::signature::Verifier as _;
use tulpa_link::{LocalConfig, ProtectedP256Signer, Route, discover};
use tulpa_secrets::{KeyBackend, ProtectedP256Key};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args();
    let _program = arguments.next();
    let command = arguments.next().unwrap_or_default();
    let value = arguments.next().unwrap_or_default();
    if value.is_empty() || arguments.next().is_some() {
        return usage();
    }
    match command.as_str() {
        "key" => run_key(&value),
        "discover" => run_discovery(Path::new(&value)),
        _ => usage(),
    }
}

fn usage() -> Result<(), Box<dyn Error>> {
    writeln!(
        io::stderr().lock(),
        "usage: tulpa-link-spike key KEY_LABEL | discover CONFIG_PATH"
    )?;
    Err("invalid arguments".into())
}

fn run_key(label: &str) -> Result<(), Box<dyn Error>> {
    let key = ProtectedP256Key::create_or_load(label)?;
    let signer = ProtectedP256Signer::new(key)?;
    let backend = match signer.protected_key().backend() {
        KeyBackend::SecureEnclave => "Secure Enclave",
        KeyBackend::AppRestrictedKeychain => "app-restricted login Keychain",
        KeyBackend::Existing => "existing protected key",
        _ => "unknown protected-key backend",
    };
    writeln!(io::stderr().lock(), "backend: {backend}")?;
    if let Some(error) = signer.protected_key().secure_enclave_error() {
        writeln!(
            io::stderr().lock(),
            "Secure Enclave fallback cause: {error}"
        )?;
    }

    let message = b"tulpa protected-key self-test";
    let der = signer.protected_key().sign(message)?;
    let signature = p256::ecdsa::Signature::from_der(&der)?;
    let public_key =
        p256::ecdsa::VerifyingKey::from_sec1_bytes(&signer.protected_key().public_key_sec1()?)?;
    public_key.verify(message, &signature)?;
    writeln!(io::stderr().lock(), "protected signing self-test: passed")?;
    writeln!(io::stdout().lock(), "{}", signer.openssh_public_key()?)?;
    Ok(())
}

fn run_discovery(path: &Path) -> Result<(), Box<dyn Error>> {
    let input = std::fs::read_to_string(path)?;
    let config = LocalConfig::from_toml(&input)?;
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
