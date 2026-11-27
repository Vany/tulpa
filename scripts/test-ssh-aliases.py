#!/usr/bin/env python3
"""Exercise OpenSSH aliases and proxy routes against throwaway stock sshds.

Run from the repository root. No real SSH configuration, identity, or server is
read. All files and listeners are temporary and removed after the test.
"""

import os
import pathlib
import shutil
import socket
import subprocess
import tempfile
import threading
import time

subprocess.run(
    ["cargo", "build", "--package", "tulpa-link", "--bin", "tulpa-link-spike", "--locked"],
    check=True,
)
root = pathlib.Path(tempfile.mkdtemp(prefix="tulpa-ssh-aliases-"))
processes = []
listener = None
try:
    home = root / "home"
    ssh = home / ".ssh"
    ssh.mkdir(parents=True)
    identity = root / "id_ed25519"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", str(identity)], check=True)
    user = subprocess.check_output(["id", "-un"], text=True).strip()

    echo = socket.socket()
    echo.bind(("127.0.0.1", 0))
    echo.listen(8)
    listener = echo
    echo_port = echo.getsockname()[1]

    def serve_echo():
        while True:
            try:
                conn, _ = echo.accept()
            except OSError:
                return
            threading.Thread(target=lambda connection: serve_connection(connection), args=(conn,), daemon=True).start()

    def serve_connection(conn):
        with conn:
            data = conn.recv(4096)
            conn.sendall(b"remote: " + data)

    threading.Thread(target=serve_echo, daemon=True).start()

    def start_sshd(name):
        host_key = root / f"{name}_host_key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", str(host_key)], check=True)
        authorized = root / f"{name}_authorized_keys"
        allowed_port = target_port if name == "jump" else echo_port
        restrictions = f'restrict,port-forwarding,permitopen="127.0.0.1:{allowed_port}",from="127.0.0.1" '
        authorized.write_text(restrictions + identity.with_suffix(".pub").read_text())
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
        probe.close()
        config = root / f"{name}_sshd_config"
        config.write_text(f"""ListenAddress 127.0.0.1
Port {port}
HostKey {host_key}
AuthorizedKeysFile {authorized}
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
PermitOpen any
PidFile {root / f'{name}.pid'}
LogLevel DEBUG1
""")
        log_path = root / f"{name}.log"
        log = log_path.open("wb")
        process = subprocess.Popen(["/usr/sbin/sshd", "-D", "-f", str(config), "-E", str(log_path)], stdout=log, stderr=log)
        processes.append(process)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.05).close()
                return port, log_path
            except OSError:
                time.sleep(0.02)
        raise RuntimeError(f"{name} sshd did not start")

    target_port, target_log = start_sshd("target")
    jump_port, jump_log = start_sshd("jump")
    known_hosts = ssh / "known_hosts"
    known_hosts.write_text(
        f"[127.0.0.1]:{target_port} {(root / 'target_host_key.pub').read_text().strip()}\n"
        f"[127.0.0.1]:{jump_port} {(root / 'jump_host_key.pub').read_text().strip()}\n"
    )

    env = os.environ.copy()
    env["TULPA_OPENSSH_HOME"] = str(home)
    env["SSH_AUTH_SOCK"] = str(root / "absent-agent.sock")

    def run_alias(alias, extra=""):
        (ssh / "config").write_text(
            f"Host {alias}\n  HostName 127.0.0.1\n  Port {target_port}\n  User {user}\n"
            f"  IdentityFile {identity}\n  IdentitiesOnly yes\n{extra}\n"
            f"Host jump\n  HostName 127.0.0.1\n  Port {jump_port}\n  User {user}\n"
            f"  IdentityFile {identity}\n  IdentitiesOnly yes\n"
        )
        local = root / f"{alias}.toml"
        local.write_text(f"""[link]
host = "{alias}"
user = ""
strict_host_key_checking = "ask"
target_host = "127.0.0.1"
target_port = {echo_port}
""")
        result = subprocess.run(["target/debug/tulpa-link-spike", "exchange", str(local)], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        if result.returncode != 0:
            raise RuntimeError(f"alias {alias} failed: {result.stderr.decode(errors='replace')}")
        if result.stdout != b"remote: tulpa spike\n":
            raise RuntimeError(f"alias {alias} returned an unexpected reply")
        print(f"alias {alias}: passed")

    run_alias("direct")
    run_alias("jumped", "  ProxyJump jump\n")
    run_alias("proxied", "  ProxyCommand /usr/bin/nc %h %p\n")

    target_text = target_log.read_text(errors="replace")
    jump_text = jump_log.read_text(errors="replace")
    accepted_target = target_text.count("Accepted publickey")
    accepted_jump = jump_text.count("Accepted publickey")
    direct_target = target_text.count("server_input_channel_open: ctype direct-tcpip")
    direct_jump = jump_text.count("server_input_channel_open: ctype direct-tcpip")
    expected = (3, 1, 3, 1)
    actual = (accepted_target, accepted_jump, direct_target, direct_jump)
    if actual != expected:
        raise RuntimeError(f"unexpected route counters: {actual}, expected {expected}")
    print("HostName/Port, ProxyJump, and ProxyCommand routes passed with expected per-hop counts")
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
    shutil.rmtree(root)
