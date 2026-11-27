#!/usr/bin/env python3
"""Exercise an encrypted OpenSSH key through Keychain and a throwaway sshd.

Run from the repository root in a GUI Terminal.app session. This test creates and
then deletes one generic-password item for a temporary key. The required command
flag acknowledges that lifecycle. No real SSH identity or server is used.
"""

import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

if sys.argv[1:] != ["--allow-temporary-keychain-item"]:
    raise SystemExit("requires --allow-temporary-keychain-item")

subprocess.run(
    ["cargo", "build", "--package", "tulpa-link", "--bin", "tulpa-link-spike", "--locked"],
    check=True,
)
root = pathlib.Path(tempfile.mkdtemp(prefix="tulpa-ssh-keychain-"))
processes = []
listener = None
keychain_added = False
identity = None
try:
    ssh = root / "home" / ".ssh"
    ssh.mkdir(parents=True)
    host_key = root / "ssh_host_ed25519_key"
    identity = root / "id_ed25519"
    passphrase = "throwaway-test-passphrase"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", str(host_key)], check=True)
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", passphrase, "-C", "", "-f", str(identity)], check=True)
    binary = pathlib.Path("target/debug/tulpa-link-spike").resolve()
    subprocess.run(["security", "add-generic-password", "-a", str(identity), "-s", "OpenSSH", "-w", passphrase, "-T", str(binary)], check=True, stdout=subprocess.DEVNULL)
    keychain_added = True

    echo = socket.socket()
    echo.bind(("127.0.0.1", 0))
    echo.listen(1)
    listener = echo
    echo_port = echo.getsockname()[1]
    restrictions = f'restrict,port-forwarding,permitopen="127.0.0.1:{echo_port}",from="127.0.0.1" '
    (root / "authorized_keys").write_text(restrictions + identity.with_suffix(".pub").read_text())

    def serve_echo():
        conn, _ = echo.accept()
        with conn:
            data = conn.recv(4096)
            conn.sendall(b"remote: " + data)

    threading.Thread(target=serve_echo, daemon=True).start()
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    ssh_port = probe.getsockname()[1]
    probe.close()
    user = subprocess.check_output(["id", "-un"], text=True).strip()
    host_public = host_key.with_suffix(".pub").read_text().strip()
    (ssh / "known_hosts").write_text(f"[127.0.0.1]:{ssh_port} {host_public}\n")
    (ssh / "config").write_text(f"Host 127.0.0.1\n  User {user}\n  IdentityFile {identity}\n  IdentitiesOnly yes\n")
    sshd_config = root / "sshd_config"
    sshd_config.write_text(f"""ListenAddress 127.0.0.1
Port {ssh_port}
HostKey {host_key}
AuthorizedKeysFile {root / 'authorized_keys'}
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
UsePAM no
AllowTcpForwarding local
PermitListen none
AllowAgentForwarding no
X11Forwarding no
PermitTTY no
PermitUserRC no
ForceCommand /usr/bin/false
PermitOpen 127.0.0.1:{echo_port}
PidFile {root / 'sshd.pid'}
LogLevel DEBUG1
""")
    sshd_log = (root / "sshd.log").open("wb")
    sshd = subprocess.Popen(["/usr/sbin/sshd", "-D", "-f", str(sshd_config), "-E", str(root / "sshd.log")], stdout=sshd_log, stderr=sshd_log)
    processes.append(sshd)
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", ssh_port), timeout=0.05).close()
            break
        except OSError:
            time.sleep(0.02)
    config = root / "config.toml"
    config.write_text(f"""[link]
user = ""
strict_host_key_checking = "ask"
mdns_service = "_tulpa-absent._tcp.local."
discovery_timeout_ms = 100
probe_timeout_ms = 100
tunnel_entry_points = [{{ host = "127.0.0.1", port = {ssh_port} }}]
target_host = "127.0.0.1"
target_port = {echo_port}
""")
    visible = subprocess.run(["security", "find-generic-password", "-a", str(identity), "-s", "OpenSSH"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if visible.returncode != 0:
        raise RuntimeError("security could not find the temporary OpenSSH item")
    env = os.environ.copy()
    env["TULPA_OPENSSH_HOME"] = str(root / "home")
    env["SSH_AUTH_SOCK"] = str(root / "absent-agent.sock")
    try:
        result = subprocess.run(["target/debug/tulpa-link-spike", "exchange", str(config)], env=env, stdout=subprocess.PIPE, timeout=60)
    except subprocess.TimeoutExpired as error:
        stdout = (error.stdout or b"").decode(errors="replace")
        server_log = (root / "sshd.log").read_text(errors="replace")
        accepted = server_log.count("Accepted publickey")
        direct = server_log.count("server_input_channel_open: ctype direct-tcpip")
        raise RuntimeError(f"exchange timed out; stdout={stdout}; accepted={accepted}; direct={direct}") from error
    if result.returncode != 0:
        raise RuntimeError(f"exchange failed with status {result.returncode}")
    if result.stdout != b"remote: tulpa spike\n":
        raise RuntimeError("unexpected echo reply")
    time.sleep(0.1)
    log = (root / "sshd.log").read_text(errors="replace")
    accepted = log.count("Accepted publickey")
    direct = log.count("server_input_channel_open: ctype direct-tcpip")
    if accepted != 1 or direct != 1:
        raise RuntimeError(f"unexpected sshd event counts: accepted={accepted}, direct={direct}")
    print("Keychain passphrase -> stock sshd exchange passed; authenticated connections=1; direct-tcpip channels=1")
finally:
    for process in reversed(processes):
        process.terminate()
    for process in reversed(processes):
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    if listener is not None:
        listener.close()
    if keychain_added and identity is not None:
        subprocess.run(["security", "delete-generic-password", "-a", str(identity), "-s", "OpenSSH"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(root)
