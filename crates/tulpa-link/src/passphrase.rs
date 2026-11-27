use std::error::Error;
use std::fmt;
use std::path::Path;

use russh::keys::PrivateKey;
use zeroize::Zeroizing;

/// Passphrase sources for an encrypted OpenSSH identity file.
pub trait PassphraseProvider: Send {
    /// Reads the existing OpenSSH Keychain entry, without creating or updating it.
    ///
    /// # Errors
    ///
    /// Returns an error when Keychain access fails. The caller then tries the prompt.
    fn keychain_passphrase(
        &mut self,
        key_path: &Path,
    ) -> Result<Option<Zeroizing<String>>, PassphraseError>;

    /// Requests a passphrase interactively as the last resort.
    ///
    /// # Errors
    ///
    /// Returns an error when the user cancels or no interactive terminal is available.
    fn prompt_passphrase(&mut self, key_path: &Path) -> Result<Zeroizing<String>, PassphraseError>;
}

/// Loads an OpenSSH private key. An encrypted key tries Keychain before an
/// interactive prompt. Passphrases are neither cached nor logged.
///
/// # Errors
///
/// Returns an error for an invalid key, an unreadable file, or failure of all
/// applicable passphrase sources.
pub fn load_private_key(
    path: &Path,
    provider: &mut dyn PassphraseProvider,
) -> Result<PrivateKey, PassphraseError> {
    match russh::keys::load_secret_key(path, None) {
        Ok(key) => Ok(key),
        Err(russh::keys::Error::KeyIsEncrypted) => {
            if let Ok(Some(passphrase)) = provider.keychain_passphrase(path)
                && let Ok(key) = russh::keys::load_secret_key(path, Some(&passphrase))
            {
                return Ok(key);
            }
            let passphrase = provider.prompt_passphrase(path)?;
            russh::keys::load_secret_key(path, Some(&passphrase))
                .map_err(|error| PassphraseError(error.to_string()))
        }
        Err(error) => Err(PassphraseError(error.to_string())),
    }
}

/// A private-key read, decryption, or interactive-prompt failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PassphraseError(String);

impl PassphraseError {
    /// Creates an error from a message that must not contain a passphrase.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for PassphraseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "OpenSSH identity: {}", self.0)
    }
}

impl Error for PassphraseError {}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    struct TestProvider {
        keychain: Option<&'static str>,
        calls: Vec<&'static str>,
    }

    impl PassphraseProvider for TestProvider {
        fn keychain_passphrase(
            &mut self,
            _: &Path,
        ) -> Result<Option<Zeroizing<String>>, PassphraseError> {
            self.calls.push("keychain");
            Ok(self.keychain.map(|value| Zeroizing::new(value.to_owned())))
        }

        fn prompt_passphrase(&mut self, _: &Path) -> Result<Zeroizing<String>, PassphraseError> {
            self.calls.push("prompt");
            Ok(Zeroizing::new("correct test passphrase".to_owned()))
        }
    }

    #[test]
    fn keychain_precedes_prompt_and_a_wrong_value_falls_through()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("id_ed25519");
        let status = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "correct test passphrase",
                "-C",
                "",
            ])
            .arg("-f")
            .arg(&path)
            .status()?;
        assert!(status.success());
        let mut provider = TestProvider {
            keychain: Some("incorrect test passphrase"),
            calls: Vec::new(),
        };

        let key = load_private_key(&path, &mut provider)?;

        assert_eq!(key.algorithm().as_str(), "ssh-ed25519");
        assert_eq!(provider.calls, ["keychain", "prompt"]);
        Ok(())
    }

    #[test]
    fn correct_keychain_passphrase_avoids_prompt() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("id_ed25519");
        let status = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "correct test passphrase",
                "-C",
                "",
            ])
            .arg("-f")
            .arg(&path)
            .status()?;
        assert!(status.success());
        let mut provider = TestProvider {
            keychain: Some("correct test passphrase"),
            calls: Vec::new(),
        };

        load_private_key(&path, &mut provider)?;

        assert_eq!(provider.calls, ["keychain"]);
        Ok(())
    }

    #[test]
    fn unencrypted_rsa_identity_needs_no_passphrase() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("id_rsa");
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "rsa", "-b", "2048", "-N", "", "-C", ""])
            .arg("-f")
            .arg(&path)
            .status()?;
        assert!(status.success());
        let mut provider = TestProvider {
            keychain: None,
            calls: Vec::new(),
        };

        let key = load_private_key(&path, &mut provider)?;

        assert!(key.algorithm().is_rsa());
        assert!(provider.calls.is_empty());
        Ok(())
    }
}
