use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use russh::keys::ssh_key::{HashAlg, PublicKey};

const DEFAULT_IDENTITY_FILES: &[&str] = &[
    "id_rsa",
    "id_ecdsa",
    "id_ecdsa_sk",
    "id_ed25519",
    "id_ed25519_sk",
    "id_xmss",
];

/// An identity file selected by OpenSSH configuration or the default list.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IdentityFile {
    path: PathBuf,
    public_key: Option<PublicKey>,
}

impl IdentityFile {
    /// Loads a candidate path and, when available without a passphrase, its public key.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let public_key = read_public_key(&path);
        Self { path, public_key }
    }

    /// The path used for the Keychain account and any interactive prompt.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The public key from the adjacent `.pub` file or an unencrypted private key.
    #[must_use]
    pub const fn public_key(&self) -> Option<&PublicKey> {
        self.public_key.as_ref()
    }

    /// The SHA-256 fingerprint in the same text form as `ssh-keygen -lf`.
    #[must_use]
    pub fn fingerprint(&self) -> Option<String> {
        self.public_key
            .as_ref()
            .map(|key| key.fingerprint(HashAlg::Sha256).to_string())
    }
}

/// The identity files selected for one SSH destination.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IdentitySelection {
    /// Candidate files in OpenSSH configuration order.
    pub files: Vec<IdentityFile>,
    /// Whether `IdentitiesOnly yes` excludes the default identity list.
    pub identities_only: bool,
}

/// Resolves `~/.ssh/config` for `host` and returns existing identity files.
///
/// Configured `IdentityFile` entries retain their order. Without any, the
/// standard OpenSSH default paths are used unless `IdentitiesOnly yes` is set.
///
/// # Errors
///
/// Returns an error if an existing SSH config cannot be read or parsed.
pub fn select_identity_files(
    home: &Path,
    host: &str,
) -> Result<IdentitySelection, IdentityConfigError> {
    let resolved = crate::resolve_host(host, None, home).map_err(config_error)?;
    let paths = if resolved.identity_files.is_empty() && !resolved.identities_only {
        DEFAULT_IDENTITY_FILES
            .iter()
            .map(|name| home.join(".ssh").join(name))
            .collect()
    } else {
        resolved.identity_files
    };
    let identities_only = resolved.identities_only;
    let files = paths
        .into_iter()
        .filter(|path| path.is_file())
        .map(IdentityFile::new)
        .collect();
    Ok(IdentitySelection {
        files,
        identities_only,
    })
}

fn read_public_key(path: &Path) -> Option<PublicKey> {
    let public_path = path.with_extension("pub");
    if let Ok(encoded) = std::fs::read_to_string(public_path)
        && let Ok(key) = PublicKey::from_openssh(&encoded)
    {
        return Some(key);
    }
    russh::keys::load_secret_key(path, None)
        .ok()
        .map(|key| key.public_key().clone())
}

fn config_error(error: impl fmt::Display) -> IdentityConfigError {
    IdentityConfigError(error.to_string())
}

/// An OpenSSH config read or parse failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityConfigError(String);

impl fmt::Display for IdentityConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "OpenSSH identity configuration: {}", self.0)
    }
}

impl Error for IdentityConfigError {}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn configured_identity_order_and_identities_only_are_honored()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh)?;
        let first = ssh.join("first");
        let second = ssh.join("second");
        std::fs::write(&first, b"not a private key")?;
        std::fs::write(&second, b"not a private key")?;
        std::fs::write(
            ssh.join("config"),
            format!(
                "Host example\n  IdentityFile {}\n  IdentityFile {}\n  IdentitiesOnly yes\n",
                first.display(),
                second.display()
            ),
        )?;

        let selection = select_identity_files(temp.path(), "example")?;

        assert!(selection.identities_only);
        let paths: Vec<_> = selection.files.iter().map(IdentityFile::path).collect();
        assert_eq!(paths, [first.as_path(), second.as_path()]);
        Ok(())
    }

    #[test]
    fn fingerprint_matches_ssh_keygen() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("id_ed25519");
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", ""])
            .arg("-f")
            .arg(&path)
            .status()?;
        assert!(status.success());
        let identity = IdentityFile::new(path.clone());
        let output = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(path.with_extension("pub"))
            .output()?;
        let text = String::from_utf8(output.stdout)?;
        let expected = text.split_whitespace().nth(1);

        assert_eq!(identity.fingerprint().as_deref(), expected);
        Ok(())
    }
}
