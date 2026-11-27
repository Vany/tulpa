use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{self, AuthResult, Handle};
use russh::keys::agent::{AgentIdentity, client::AgentClient};
use russh::keys::ssh_key::{HashAlg, PublicKey};
use russh::{ChannelMsg, Disconnect};

use crate::known_hosts::{KnownHostsHandler, StrictHostKeyChecking, default_known_hosts_path};
use crate::passphrase::{PassphraseProvider, load_private_key};
use crate::{IdentitySelection, select_identity_files};

const MAX_REPLY_BYTES: usize = 64 * 1024;

/// A remote TCP endpoint supplied by local configuration or live discovery.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
#[non_exhaustive]
pub struct Endpoint {
    /// A DNS name or IP address. No endpoint is embedded in the repository.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

impl Endpoint {
    /// Creates an endpoint from a local configuration or discovery result.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// Configuration for one authenticated direct-TCP/IP exchange.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExchangeConfig {
    /// The selected SSH server.
    pub server: Endpoint,
    /// The SSH user from local configuration or OpenSSH config.
    pub user: String,
    /// The target reached through the single `direct-tcpip` channel.
    pub target: Endpoint,
    /// The home containing `.ssh/config`, identity files, and `known_hosts`.
    pub home: PathBuf,
    /// OpenSSH's unknown-host behavior. Changed keys are always rejected.
    pub strict_host_key_checking: StrictHostKeyChecking,
    /// The fully resolved route, identity, and proxy settings.
    pub resolved: crate::ResolvedHost,
}

impl ExchangeConfig {
    /// Creates an OpenSSH-compatible exchange configuration with `ask` host checking.
    #[must_use]
    pub fn new(server: Endpoint, user: impl Into<String>, target: Endpoint, home: PathBuf) -> Self {
        let resolved = crate::ResolvedHost::direct(&server.host, server.port, user);
        Self {
            user: resolved.user.clone(),
            server,
            target,
            home,
            strict_host_key_checking: StrictHostKeyChecking::Ask,
            resolved,
        }
    }

    /// Resolves an OpenSSH alias through `ssh -G`, including proxy settings.
    ///
    /// # Errors
    ///
    /// Returns an error if `ssh -G` rejects or cannot resolve the alias.
    pub fn from_alias(
        alias: &str,
        user_override: Option<&str>,
        target: Endpoint,
        home: PathBuf,
    ) -> Result<Self, crate::AliasError> {
        let resolved = crate::resolve_host(alias, user_override, &home)?;
        let server = Endpoint::new(&resolved.hostname, resolved.port);
        Ok(Self {
            user: resolved.user.clone(),
            server,
            target,
            home,
            strict_host_key_checking: StrictHostKeyChecking::Ask,
            resolved,
        })
    }

    /// Sets the unknown-host policy from local configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not `ask`, `yes`, or `no`.
    pub fn with_strict_host_key_checking(
        mut self,
        value: &str,
    ) -> Result<Self, crate::HostKeyError> {
        self.strict_host_key_checking = value.parse()?;
        Ok(self)
    }
}

struct ConnectedRoute {
    session: Handle<KnownHostsHandler>,
    upstream: Vec<Handle<KnownHostsHandler>>,
}

async fn connect_route(
    config: &ExchangeConfig,
    host_key_prompt: crate::SharedHostKeyPrompt,
    passphrases: &mut dyn PassphraseProvider,
    depth: u8,
) -> Result<ConnectedRoute, LinkError> {
    if depth >= 8 {
        return Err(LinkError::Configuration(
            "ProxyJump chain exceeds eight hops".to_owned(),
        ));
    }
    let handler = KnownHostsHandler::new(
        config.resolved.hostname.clone(),
        config.resolved.port,
        default_known_hosts_path(&config.home),
        config.strict_host_key_checking,
        Arc::clone(&host_key_prompt),
    );
    let mut route = if let Some(proxy_jump) = &config.resolved.proxy_jump {
        let mut hops = proxy_jump.split(',');
        let first = hops
            .next()
            .ok_or_else(|| LinkError::Configuration("empty ProxyJump".to_owned()))?;
        if hops.next().is_some() {
            return Err(LinkError::Configuration(
                "multiple ProxyJump hops are unsupported".to_owned(),
            ));
        }
        let reference = crate::parse_jump_reference(first)
            .map_err(|error| LinkError::Configuration(error.to_string()))?;
        let mut jump =
            crate::resolve_host(&reference.host, reference.user.as_deref(), &config.home)
                .map_err(|error| LinkError::Configuration(error.to_string()))?;
        if let Some(port) = reference.port {
            jump.port = port;
        }
        let mut jump_config = config.clone();
        jump_config.server = Endpoint::new(&jump.hostname, jump.port);
        jump_config.user = jump.user.clone();
        jump_config.resolved = jump;
        let upstream = Box::pin(connect_route(
            &jump_config,
            host_key_prompt,
            passphrases,
            depth.saturating_add(1),
        ))
        .await?;
        let channel = upstream
            .session
            .channel_open_direct_tcpip(
                config.resolved.hostname.clone(),
                u32::from(config.resolved.port),
                "localhost",
                0,
            )
            .await
            .map_err(|error| LinkError::Channel(error.to_string()))?;
        let session = client::connect_stream(
            Arc::new(client::Config::default()),
            channel.into_stream(),
            handler,
        )
        .await
        .map_err(|error| LinkError::Connection(error.to_string()))?;
        let mut upstream_sessions = upstream.upstream;
        upstream_sessions.push(upstream.session);
        ConnectedRoute {
            session,
            upstream: upstream_sessions,
        }
    } else if let Some(proxy_command) = &config.resolved.proxy_command {
        let command = crate::expand_proxy_command(
            proxy_command,
            &config.resolved.hostname,
            config.resolved.port,
            &config.resolved.user,
        );
        let transport = crate::ProxyTransport::spawn(&command)
            .map_err(|error| LinkError::Connection(error.to_string()))?;
        let session =
            client::connect_stream(Arc::new(client::Config::default()), transport, handler)
                .await
                .map_err(|error| LinkError::Connection(error.to_string()))?;
        ConnectedRoute {
            session,
            upstream: Vec::new(),
        }
    } else {
        let session = client::connect(
            Arc::new(client::Config::default()),
            (config.resolved.hostname.as_str(), config.resolved.port),
            handler,
        )
        .await
        .map_err(|error| LinkError::Connection(error.to_string()))?;
        ConnectedRoute {
            session,
            upstream: Vec::new(),
        }
    };
    authenticate(&mut route.session, config, passphrases).await?;
    Ok(route)
}

/// Connects once, authenticates with the user's OpenSSH identities, and opens
/// exactly one `direct-tcpip` channel. No session, PTY, listener, or reverse
/// forward is used.
///
/// # Errors
///
/// Returns an error on connection, host-key verification, identity selection,
/// authentication, channel, or reply-size failure.
pub async fn exchange_line(
    config: &ExchangeConfig,
    host_key_prompt: Box<dyn crate::HostKeyPrompt + Send>,
    passphrases: &mut dyn PassphraseProvider,
    line: &[u8],
) -> Result<Vec<u8>, LinkError> {
    let host_key_prompt = std::sync::Arc::new(tokio::sync::Mutex::new(host_key_prompt));
    let mut route = connect_route(config, host_key_prompt, passphrases, 0).await?;
    let session = &mut route.session;

    let mut channel = session
        .channel_open_direct_tcpip(
            config.target.host.clone(),
            u32::from(config.target.port),
            "localhost",
            0,
        )
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    channel
        .data_bytes(line.to_vec())
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    channel
        .eof()
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;

    let mut reply = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if reply.len().saturating_add(data.len()) > MAX_REPLY_BYTES {
                    return Err(LinkError::ReplyTooLarge);
                }
                reply.extend_from_slice(&data);
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            ChannelMsg::OpenFailure(error) => {
                return Err(LinkError::Channel(format!("{error:?}")));
            }
            _ => {}
        }
    }
    channel
        .close()
        .await
        .map_err(|error| LinkError::Channel(error.to_string()))?;
    session
        .disconnect(Disconnect::ByApplication, "", "")
        .await
        .map_err(|error| LinkError::Connection(error.to_string()))?;
    Ok(reply)
}

async fn authenticate<H: russh::client::Handler>(
    session: &mut Handle<H>,
    config: &ExchangeConfig,
    passphrases: &mut dyn PassphraseProvider,
) -> Result<(), LinkError> {
    let selection = select_identity_files(&config.home, &config.resolved.alias)
        .map_err(|error| LinkError::Configuration(error.to_string()))?;
    let mut agent_keys = Vec::new();
    if let Ok(mut agent) = AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for identity in identities {
            if !agent_identity_allowed(&identity, &selection) {
                continue;
            }
            if let AgentIdentity::PublicKey { key, .. } = &identity {
                agent_keys.push(key.clone());
            }
            let result = match &identity {
                AgentIdentity::PublicKey { key, .. } => {
                    let hash_alg = rsa_hash(session, key).await;
                    session
                        .authenticate_publickey_with(
                            config.user.clone(),
                            key.clone(),
                            hash_alg,
                            &mut agent,
                        )
                        .await
                }
                AgentIdentity::Certificate { certificate, .. } => {
                    session
                        .authenticate_certificate_with(
                            config.user.clone(),
                            certificate.clone(),
                            None,
                            &mut agent,
                        )
                        .await
                }
            };
            if matches!(result, Ok(AuthResult::Success)) {
                return Ok(());
            }
        }
    }

    for identity in &selection.files {
        if identity.public_key().is_some_and(|candidate| {
            agent_keys
                .iter()
                .any(|key| key.key_data() == candidate.key_data())
        }) {
            continue;
        }
        let Ok(key) = load_private_key(identity.path(), passphrases) else {
            continue;
        };
        let key = Arc::new(key);
        let certificate_path = identity.path().with_extension("pub");
        let certificate_path = certificate_path
            .to_string_lossy()
            .strip_suffix(".pub")
            .map(|stem| format!("{stem}-cert.pub"));
        let result = if let Some(certificate_path) = certificate_path {
            match russh::keys::load_openssh_certificate(certificate_path) {
                Ok(certificate) => {
                    session
                        .authenticate_openssh_cert(config.user.clone(), key, certificate)
                        .await
                }
                Err(_) => authenticate_file_key(session, &config.user, key).await,
            }
        } else {
            authenticate_file_key(session, &config.user, key).await
        };
        if matches!(result, Ok(AuthResult::Success)) {
            return Ok(());
        }
    }
    Err(LinkError::AuthenticationRejected)
}

fn agent_identity_allowed(identity: &AgentIdentity, selection: &IdentitySelection) -> bool {
    if !selection.identities_only {
        return true;
    }
    selection
        .files
        .iter()
        .filter_map(|file| file.public_key())
        .any(|candidate| match identity {
            AgentIdentity::PublicKey { key, .. } => candidate.key_data() == key.key_data(),
            AgentIdentity::Certificate { certificate, .. } => {
                candidate.key_data() == certificate.public_key()
            }
        })
}

async fn authenticate_file_key<H: russh::client::Handler>(
    session: &mut Handle<H>,
    user: &str,
    key: Arc<russh::keys::PrivateKey>,
) -> Result<AuthResult, russh::Error> {
    let hash_alg = if key.algorithm().is_rsa() {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    session
        .authenticate_publickey(user, russh::keys::PrivateKeyWithHashAlg::new(key, hash_alg))
        .await
}

async fn rsa_hash<H: russh::client::Handler>(
    session: &Handle<H>,
    key: &PublicKey,
) -> Option<HashAlg> {
    if key.algorithm().is_rsa() {
        session
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten()
    } else {
        None
    }
}

/// A connection, authentication, channel, or exchange failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum LinkError {
    /// Invalid local connection configuration.
    Configuration(String),
    /// The SSH transport failed, including known-host rejection.
    Connection(String),
    /// None of the selected OpenSSH identities authenticated.
    AuthenticationRejected,
    /// The direct-TCP/IP channel failed.
    Channel(String),
    /// The peer exceeded the spike's bounded reply size.
    ReplyTooLarge,
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "configuration: {error}"),
            Self::Connection(error) => write!(formatter, "SSH connection: {error}"),
            Self::AuthenticationRejected => {
                formatter.write_str("SSH authentication failed for all selected identities")
            }
            Self::Channel(error) => write!(formatter, "direct-tcpip channel: {error}"),
            Self::ReplyTooLarge => formatter.write_str("direct-tcpip reply exceeded 64 KiB"),
        }
    }
}

impl Error for LinkError {}

/// Returns the home directory used for native OpenSSH files.
///
/// `TULPA_OPENSSH_HOME` is an explicit test override; otherwise `HOME` is used.
/// Keychain access is unaffected by the override.
///
/// # Errors
///
/// Returns an error when the selected variable is absent or not Unicode.
pub fn home_directory() -> Result<PathBuf, LinkError> {
    std::env::var("TULPA_OPENSSH_HOME")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .map_err(|error| LinkError::Configuration(format!("OpenSSH home is unavailable: {error}")))
}

/// Returns the current login name used when neither local nor SSH config sets one.
///
/// # Errors
///
/// Returns an error when `USER` is absent or not Unicode.
pub fn default_user() -> Result<String, LinkError> {
    std::env::var("USER")
        .map_err(|error| LinkError::Configuration(format!("USER is unavailable: {error}")))
}

/// Resolves the SSH user without changing the discovered server address.
#[must_use]
pub fn configured_user(local_user: &str, home: &Path, host: &str) -> Option<String> {
    if !local_user.is_empty() {
        return Some(local_user.to_owned());
    }
    crate::resolve_host(host, None, home)
        .ok()
        .map(|resolved| resolved.user)
        .or_else(|| default_user().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ssh_config_uses_current_login() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let expected = default_user()?;

        assert_eq!(
            configured_user("", temp.path(), "example.test"),
            Some(expected)
        );
        Ok(())
    }

    #[tokio::test]
    async fn multiple_proxy_jump_hops_are_rejected_before_connecting()
    -> Result<(), Box<dyn std::error::Error>> {
        struct NoHostPrompt;
        impl crate::HostKeyPrompt for NoHostPrompt {
            fn confirm_unknown_host(&mut self, _: &str, _: u16, _: &str) -> bool {
                false
            }
        }
        struct NoPassphrases;
        impl PassphraseProvider for NoPassphrases {
            fn keychain_passphrase(
                &mut self,
                _: &Path,
            ) -> Result<Option<zeroize::Zeroizing<String>>, crate::PassphraseError> {
                Ok(None)
            }

            fn prompt_passphrase(
                &mut self,
                _: &Path,
            ) -> Result<zeroize::Zeroizing<String>, crate::PassphraseError> {
                Err(crate::PassphraseError::new("no prompt in this test"))
            }
        }

        let mut config = ExchangeConfig::new(
            Endpoint::new("127.0.0.1", 22),
            "test-user",
            Endpoint::new("127.0.0.1", 23),
            PathBuf::new(),
        );
        config.resolved.proxy_jump = Some("first.test,second.test".to_owned());
        let mut passphrases = NoPassphrases;
        let result =
            exchange_line(&config, Box::new(NoHostPrompt), &mut passphrases, b"test").await;

        match result {
            Err(LinkError::Configuration(message)) => {
                assert_eq!(message, "multiple ProxyJump hops are unsupported");
            }
            Err(error) => return Err(format!("unexpected error: {error}").into()),
            Ok(_) => return Err("multiple hops unexpectedly connected".into()),
        }
        Ok(())
    }
}
