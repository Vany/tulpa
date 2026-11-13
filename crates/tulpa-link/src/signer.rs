use std::error::Error;
use std::fmt;

use russh::Signer;
use russh::keys::agent::AgentIdentity;
use russh::keys::ssh_encoding::Encode;
use russh::keys::ssh_key::{HashAlg, PublicKey};
use tulpa_secrets::{KeyError, ProtectedP256Key};

/// A `russh` signer backed by a non-exportable macOS P-256 key.
#[derive(Debug)]
pub struct ProtectedP256Signer {
    key: ProtectedP256Key,
    public_key: PublicKey,
}

impl ProtectedP256Signer {
    /// Creates a signer and derives its SSH public key from the protected key.
    ///
    /// # Errors
    ///
    /// Returns an error if the public key is unavailable or is not a valid P-256 point.
    pub fn new(key: ProtectedP256Key) -> Result<Self, SignerError> {
        let sec1 = key.public_key_sec1().map_err(SignerError::Key)?;
        let public_key = public_key_from_sec1(&sec1)?;
        Ok(Self { key, public_key })
    }

    /// Returns the SSH public key with no identifying comment.
    #[must_use]
    pub const fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// Returns an `ecdsa-sha2-nistp256` OpenSSH public-key line with no comment.
    ///
    /// # Errors
    ///
    /// Returns an error if the SSH encoder cannot represent the public key.
    pub fn openssh_public_key(&self) -> Result<String, SignerError> {
        self.public_key
            .to_openssh()
            .map_err(|error| SignerError::Encoding(error.to_string()))
    }

    /// Returns the protected key, including its selected backend and creation error.
    #[must_use]
    pub const fn protected_key(&self) -> &ProtectedP256Key {
        &self.key
    }
}

impl Signer for ProtectedP256Signer {
    type Error = SignerError;

    async fn auth_sign(
        &mut self,
        identity: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        if hash_alg.is_some() {
            return Err(SignerError::UnsupportedHashAlgorithm);
        }
        let AgentIdentity::PublicKey { key, .. } = identity else {
            return Err(SignerError::UnexpectedIdentity);
        };
        if key != &self.public_key {
            return Err(SignerError::UnexpectedIdentity);
        }
        let der = self.key.sign(&to_sign).map_err(SignerError::Key)?;
        append_ecdsa_signature(to_sign, &der)
    }
}

/// A protected-key signing or SSH encoding failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignerError {
    /// Security.framework rejected the protected-key operation.
    Key(KeyError),
    /// The SSH key or signature encoding was invalid.
    Encoding(String),
    /// `russh` requested a hash override that ECDSA P-256 does not use.
    UnsupportedHashAlgorithm,
    /// `russh` requested a signature for a different key or a certificate.
    UnexpectedIdentity,
    /// The `russh` session closed before the signature could be sent.
    Send(russh::SendError),
}

impl fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(error) => write!(formatter, "{error}"),
            Self::Encoding(error) => write!(formatter, "SSH encoding failed: {error}"),
            Self::UnsupportedHashAlgorithm => {
                formatter.write_str("ECDSA P-256 does not accept an RSA hash override")
            }
            Self::UnexpectedIdentity => {
                formatter.write_str("russh requested a signature for an unexpected identity")
            }
            Self::Send(error) => write!(formatter, "SSH session send failed: {error}"),
        }
    }
}

impl Error for SignerError {}

impl From<russh::SendError> for SignerError {
    fn from(error: russh::SendError) -> Self {
        Self::Send(error)
    }
}

/// Converts a SEC1 P-256 point to the SSH key type expected by `russh`.
///
/// # Errors
///
/// Returns an error when the point is not a valid P-256 public key.
pub fn public_key_from_sec1(sec1: &[u8]) -> Result<PublicKey, SignerError> {
    let point = russh::keys::ssh_key::public::EcdsaPublicKey::from_sec1_bytes(sec1)
        .map_err(|error| SignerError::Encoding(error.to_string()))?;
    if !matches!(
        point,
        russh::keys::ssh_key::public::EcdsaPublicKey::NistP256(_)
    ) {
        return Err(SignerError::Encoding(
            "the protected key is not P-256".to_owned(),
        ));
    }
    Ok(point.into())
}

fn append_ecdsa_signature(mut to_sign: Vec<u8>, der: &[u8]) -> Result<Vec<u8>, SignerError> {
    let signature = p256::ecdsa::Signature::from_der(der)
        .map_err(|error| SignerError::Encoding(error.to_string()))?;
    let signature = russh::keys::ssh_key::Signature::try_from(signature)
        .map_err(|error| SignerError::Encoding(error.to_string()))?;
    signature
        .encode(&mut to_sign)
        .map_err(|error| SignerError::Encoding(error.to_string()))?;
    Ok(to_sign)
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::signature::{Signer as _, Verifier as _};

    use super::*;

    #[test]
    fn der_signature_becomes_russh_signature_without_losing_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = b"SSH userauth payload".to_vec();
        let signing_key = p256::ecdsa::SigningKey::from_slice(&[7_u8; 32])?;
        let p256_signature: p256::ecdsa::Signature = signing_key.sign(&message);
        let der = p256_signature.to_der();
        let original_len = message.len();

        let encoded = append_ecdsa_signature(message.clone(), der.as_bytes())?;

        let prefix = encoded.get(..original_len).ok_or("missing SSH payload")?;
        let signature_bytes = encoded.get(original_len..).ok_or("missing SSH signature")?;
        assert_eq!(prefix, message.as_slice());
        let ssh_signature = russh::keys::ssh_key::Signature::try_from(signature_bytes)?;
        let decoded = p256::ecdsa::Signature::try_from(&ssh_signature)?;
        signing_key.verifying_key().verify(&message, &decoded)?;
        Ok(())
    }
}
