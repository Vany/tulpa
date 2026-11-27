use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One host fully resolved by `ssh -G`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ResolvedHost {
    /// The original name used to select Host and Match rules.
    pub alias: String,
    /// The resolved `HostName`.
    pub hostname: String,
    /// The resolved SSH port.
    pub port: u16,
    /// The resolved login name.
    pub user: String,
    /// Every resolved `IdentityFile`, in output order.
    pub identity_files: Vec<PathBuf>,
    /// The resolved `IdentitiesOnly` value.
    pub identities_only: bool,
    /// The unresolved `ProxyJump` reference, if configured.
    pub proxy_jump: Option<String>,
    /// The `ProxyCommand` template, if configured.
    pub proxy_command: Option<String>,
}

impl ResolvedHost {
    /// Creates a direct endpoint without running `ssh -G`.
    #[must_use]
    pub fn direct(hostname: impl Into<String>, port: u16, user: impl Into<String>) -> Self {
        let hostname = hostname.into();
        Self {
            alias: hostname.clone(),
            hostname,
            port,
            user: user.into(),
            identity_files: Vec::new(),
            identities_only: false,
            proxy_jump: None,
            proxy_command: None,
        }
    }
}

/// Resolves `alias` with the real `ssh -G`, including Host/Match/Include rules.
/// A non-empty `user_override` is passed as `-l`, like the OpenSSH CLI.
///
/// # Errors
///
/// Returns an error if `ssh` cannot run, rejects the configuration, or its
/// output lacks a usable hostname, port, or user.
pub fn resolve_host(
    alias: &str,
    user_override: Option<&str>,
    home: &Path,
) -> Result<ResolvedHost, AliasError> {
    let mut command = Command::new("ssh");
    command.env("HOME", home).arg("-G");
    if let Some(user) = user_override.filter(|user| !user.is_empty()) {
        command.arg("-l").arg(user);
    }
    let config = home.join(".ssh").join("config");
    if config.is_file() {
        command.arg("-F").arg(config);
    }
    let output = command
        .arg(alias)
        .output()
        .map_err(|error| AliasError::new(format!("could not run ssh -G: {error}")))?;
    if !output.status.success() {
        return Err(AliasError::new(format!(
            "ssh -G rejected the host: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_ssh_g(alias, user_override, &output.stdout)
}

fn parse_ssh_g(
    alias: &str,
    user_override: Option<&str>,
    output: &[u8],
) -> Result<ResolvedHost, AliasError> {
    let text = std::str::from_utf8(output)
        .map_err(|error| AliasError::new(format!("ssh -G output was not UTF-8: {error}")))?;
    let mut resolved = ResolvedHost {
        alias: alias.to_owned(),
        hostname: alias.to_owned(),
        port: 22,
        user: String::new(),
        identity_files: Vec::new(),
        identities_only: false,
        proxy_jump: None,
        proxy_command: None,
    };
    for line in text.lines() {
        let Some((name, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        match name {
            "hostname" => value.clone_into(&mut resolved.hostname),
            "port" => {
                resolved.port = value
                    .parse()
                    .map_err(|error| AliasError::new(format!("invalid SSH port: {error}")))?;
            }
            "user" => value.clone_into(&mut resolved.user),
            "identityfile" => resolved.identity_files.push(PathBuf::from(value)),
            "identitiesonly" => resolved.identities_only = value.eq_ignore_ascii_case("yes"),
            "proxyjump" => resolved.proxy_jump = nonempty(value),
            "proxycommand" => resolved.proxy_command = nonempty(value),
            _ => {}
        }
    }
    if let Some(user) = user_override.filter(|user| !user.is_empty()) {
        user.clone_into(&mut resolved.user);
    }
    if resolved.user.is_empty() {
        resolved.user = std::env::var("USER")
            .map_err(|error| AliasError::new(format!("no resolved SSH user: {error}")))?;
    }
    if resolved.hostname.is_empty() || resolved.port == 0 {
        return Err(AliasError::new("ssh -G returned an empty hostname or port"));
    }
    Ok(resolved)
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty() && !value.eq_ignore_ascii_case("none")).then(|| value.to_owned())
}

/// A parsed `[user@]host[:port]` `ProxyJump` reference.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct JumpReference {
    /// The host or alias to resolve with another `ssh -G` call.
    pub host: String,
    /// An optional user supplied in the reference.
    pub user: Option<String>,
    /// An optional port supplied in the reference.
    pub port: Option<u16>,
}

/// Parses one `ProxyJump` reference, including bracketed IPv6 addresses.
///
/// # Errors
///
/// Returns an error for an empty host or malformed port.
pub fn parse_jump_reference(value: &str) -> Result<JumpReference, AliasError> {
    let (user, host_port) = value
        .rsplit_once('@')
        .map_or((None, value), |(user, host)| (Some(user.to_owned()), host));
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .ok_or_else(|| AliasError::new("unterminated bracketed jump host"))?;
        let port = suffix
            .strip_prefix(':')
            .map(str::parse::<u16>)
            .transpose()
            .map_err(|error| AliasError::new(format!("invalid jump port: {error}")))?;
        (host, port)
    } else if host_port.matches(':').count() == 1 {
        let (host, port) = host_port
            .rsplit_once(':')
            .ok_or_else(|| AliasError::new("missing jump host"))?;
        let port = port
            .parse()
            .map_err(|error| AliasError::new(format!("invalid jump port: {error}")))?;
        (host, Some(port))
    } else {
        (host_port, None)
    };
    if host.is_empty() {
        return Err(AliasError::new("empty ProxyJump host"));
    }
    Ok(JumpReference {
        host: host.to_owned(),
        user,
        port,
    })
}

/// Expands OpenSSH `ProxyCommand`'s `%h`, `%p`, and `%r` placeholders.
#[must_use]
pub fn expand_proxy_command(template: &str, host: &str, port: u16, user: &str) -> String {
    let mut output = String::with_capacity(template.len());
    let mut characters = template.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('%') | None => output.push('%'),
            Some('h') => output.push_str(host),
            Some('p') => output.push_str(&port.to_string()),
            Some('r') => output.push_str(user),
            Some(other) => {
                output.push('%');
                output.push(other);
            }
        }
    }
    output
}

/// An alias-resolution or `ProxyJump` parsing failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AliasError(String);

impl AliasError {
    /// Creates an error containing no secret values.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AliasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AliasError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_g_resolves_all_alias_fields() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let ssh = temp.path().join(".ssh");
        std::fs::create_dir(&ssh)?;
        let identity = ssh.join("identity");
        std::fs::write(&identity, b"not a key")?;
        std::fs::write(
            ssh.join("config"),
            format!(
                "Host alias\n  HostName 127.0.0.1\n  Port 2200\n  User test-user\n  IdentityFile {}\n  IdentitiesOnly yes\n  ProxyJump jump\n",
                identity.display()
            ),
        )?;

        let resolved = resolve_host("alias", None, temp.path())?;

        assert_eq!(resolved.hostname, "127.0.0.1");
        assert_eq!(resolved.port, 2200);
        assert_eq!(resolved.user, "test-user");
        assert_eq!(resolved.identity_files, [identity]);
        assert!(resolved.identities_only);
        assert_eq!(resolved.proxy_jump.as_deref(), Some("jump"));
        Ok(())
    }

    #[test]
    fn proxy_command_expands_openssh_placeholders() {
        assert_eq!(
            expand_proxy_command("nc %h %p # %r %%", "example.test", 22, "test-user"),
            "nc example.test 22 # test-user %"
        );
    }

    #[test]
    fn jump_reference_accepts_user_host_and_port() -> Result<(), Box<dyn std::error::Error>> {
        let reference = parse_jump_reference("user@host:2222")?;
        assert_eq!(reference.host, "host");
        assert_eq!(reference.user.as_deref(), Some("user"));
        assert_eq!(reference.port, Some(2222));
        Ok(())
    }
}
