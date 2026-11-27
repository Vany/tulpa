//! Interoperability between stock OpenSSH and a throwaway `russh` node.

use std::process::Command;
use std::sync::Arc;

use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Handler, Server, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, server};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[derive(Clone)]
struct Node {
    authorized: PublicKey,
}

impl Server for Node {
    type Handler = Self;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
        self.clone()
    }
}

impl Handler for Node {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if key.public_key == &self.authorized {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<server::Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if command != b"probe" {
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        session.data(channel, b"openssh node authenticated\n".to_vec())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[tokio::test]
async fn stock_ssh_authenticates_with_native_authorized_keys()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let host_key = temp.path().join("ssh_host_ed25519_key");
    let identity = temp.path().join("id_ed25519");
    for key in [&host_key, &identity] {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "", "-f"])
            .arg(key)
            .status()?;
        assert!(status.success());
    }
    let authorized =
        PublicKey::from_openssh(&std::fs::read_to_string(identity.with_extension("pub"))?)?;
    let host_key = russh::keys::load_secret_key(host_key, None)?;
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        auth_rejection_time: std::time::Duration::ZERO,
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let mut node = Node { authorized };
    let server_task = tokio::spawn(async move { node.run_on_socket(config, &listener).await });

    let known_hosts = temp.path().join("known_hosts");
    let ssh_log = temp.path().join("ssh.log");
    let mut child = tokio::process::Command::new("ssh")
        .args([
            "-F",
            "/dev/null",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "LogLevel=DEBUG",
            "-E",
        ])
        .arg(&ssh_log)
        .arg("-i")
        .arg(identity)
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-p")
        .arg(port.to_string())
        .arg("test@127.0.0.1")
        .arg("probe")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let status = if let Ok(status) =
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await
    {
        status?
    } else {
        child.kill().await?;
        child.wait().await?;
        return Err(format!("ssh timed out: {}", std::fs::read_to_string(&ssh_log)?).into());
    };
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or("missing ssh stdout")?
        .read_to_end(&mut output)
        .await?;
    server_task.abort();

    assert!(
        status.success(),
        "ssh failed: {}",
        std::fs::read_to_string(&ssh_log)?
    );
    assert_eq!(output, b"openssh node authenticated\n");
    Ok(())
}
