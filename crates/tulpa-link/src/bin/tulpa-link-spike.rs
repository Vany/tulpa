//! Manual harness for the protected-key and SSH transport spike.

use std::error::Error;
use std::io::{self, Write};

use p256::ecdsa::signature::Verifier as _;
use tulpa_link::ProtectedP256Signer;
use tulpa_secrets::{KeyBackend, ProtectedP256Key};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args();
    let _program = arguments.next();
    let command = arguments.next().unwrap_or_default();
    let label = arguments.next().unwrap_or_default();
    if command != "key" || label.is_empty() || arguments.next().is_some() {
        writeln!(io::stderr().lock(), "usage: tulpa-link-spike key KEY_LABEL")?;
        return Err("invalid arguments".into());
    }

    let key = ProtectedP256Key::create_or_load(&label)?;
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
