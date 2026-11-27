use std::error::Error;
use std::fmt;
use std::path::Path;

use security_framework::os::macos::passwords::find_generic_password;
use zeroize::Zeroizing;

/// Reads the passphrase stored by `ssh-add --apple-use-keychain` for `key_path`.
///
/// OpenSSH stores this as a generic password with service `OpenSSH` and the key
/// path as the account. This function only reads it. The caller must be running
/// in the user's GUI login session when Keychain interaction is required.
///
/// # Errors
///
/// Returns an error for Keychain failures other than a missing item, or when
/// the stored passphrase is not UTF-8. No error contains the passphrase.
pub fn openssh_passphrase(key_path: &Path) -> Result<Option<Zeroizing<String>>, KeychainError> {
    let account = key_path.to_string_lossy();
    match find_generic_password(None, "OpenSSH", &account) {
        Ok((password, _item)) => {
            let bytes = Zeroizing::new(password.as_ref().to_vec());
            let passphrase = std::str::from_utf8(&bytes)
                .map_err(|_| KeychainError::InvalidPassphrase)?
                .to_owned();
            Ok(Some(Zeroizing::new(passphrase)))
        }
        Err(error) if error.code() == -25_300 => Ok(None),
        Err(error) => Err(KeychainError::Lookup { code: error.code() }),
    }
}

/// A read-only OpenSSH Keychain lookup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KeychainError {
    /// The Keychain lookup failed with the contained `OSStatus` code.
    Lookup {
        /// The numeric Keychain error.
        code: i32,
    },
    /// The stored passphrase was not valid UTF-8.
    InvalidPassphrase,
}

impl fmt::Display for KeychainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lookup { code } => {
                write!(
                    formatter,
                    "OpenSSH Keychain lookup failed with OSStatus {code}"
                )
            }
            Self::InvalidPassphrase => {
                formatter.write_str("the stored OpenSSH passphrase is not UTF-8")
            }
        }
    }
}

impl Error for KeychainError {}
