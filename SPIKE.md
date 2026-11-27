# tulpa platform spike

Status: the selected SSH design is native OpenSSH compatibility. Discovery, both protocol directions, restricted-server operation, and the real Keychain-backed encrypted-key exchange are verified with throwaway credentials. No server address, host key, public-key line, local username, passphrase, or identifying path is recorded here.

## 1. Permission grants and the bundled helper

Not attempted. There is no app bundle, microphone capture, screen capture, or camera capture yet. No permission grant is claimed.

## 2. OpenSSH identities, trust, and real SSH interoperability

The Secure Enclave design is dropped. No provisioning profile, Personal Team, or `keychain-access-groups` entitlement is needed or used.

### Identity selection

- `IdentityFile` entries from `~/.ssh/config` retain their configured order; `IdentitiesOnly yes` is honored, including agent filtering.
- Without configured entries, existing files from OpenSSH's standard identity list are considered. Missing files are skipped.
- Fingerprints come from native public-key material and use the same SHA-256 text form as `ssh-keygen -lf`. A real `ssh-keygen` comparison test passes.
- Agent identities are offered first through `SSH_AUTH_SOCK`, including certificates. Files already attempted through the agent are not decrypted again.
- Standard OpenSSH RSA keys are supported through the shared `ssh-key` dependency. Hardware-backed agent keys use the agent's signing operation; no separate hardware-token test has run.

### Encrypted-key passphrase order

1. Use an already loaded ssh-agent identity, with no passphrase read.
2. For an encrypted file, read generic password service `OpenSSH`, account equal to the key path, from the login Keychain through Security.framework.
3. If that is absent, inaccessible, or does not decrypt the key, use the interactive prompt last.

Keychain reads do not create or update items. Passphrases have a zeroizing handoff and are never logged or cached by tulpa. Unit tests with an encrypted throwaway key prove Keychain-before-prompt order and wrong-value fallback.

### Native trust

- The client reads native `known_hosts`; hashed host entries pass a real `ssh-keygen -H` test.
- A changed key is rejected without prompting.
- With `StrictHostKeyChecking ask`, an unknown key requires explicit confirmation before it is appended in native format. `yes` and `no` reject unknown keys without a tulpa prompt.
- The server-side interoperability fixture reads native `authorized_keys` and accepts only that key.

### Real protocol results

All credentials, sockets, and files were temporary and are removed after each test.

| Check | Result |
| --- | --- |
| tulpa fingerprint versus `ssh-keygen -lf` | Pass |
| Agent-loaded key from tulpa to stock `sshd`, then one line through `direct-tcpip` | Pass; exactly one authenticated connection and one direct channel |
| Same exchange with a restricted authorized key, local-only forwarding, and no server-side listening | Pass; no session channel or remote forward occurred |
| Stock `ssh` to a throwaway `russh` node using native `authorized_keys` | Pass; expected command response received |
| Encrypted file using a real Keychain-stored passphrase from tulpa to stock `sshd` | Pass; exactly one authenticated connection and one direct channel |

The client implementation opens no session or PTY during the tulpa-to-`sshd` exchange. The reverse test deliberately implements one minimal command response so stock `ssh` can complete a real login.

### Keychain GUI test

Creating the throwaway generic-password item in the non-GUI tmux process initially returned `-25308` (`errSecInteractionNotAllowed`). `launchctl asuser` returned `Operation not permitted`. Running the visible test through Terminal.app in the GUI login session then passed with the restricted server policy above.

The test uses `TULPA_OPENSSH_HOME` for its temporary OpenSSH files while leaving `HOME` unchanged for the real GUI-session Keychain. It never reads a real SSH config. Its temporary Keychain item is deleted after the test.

To repeat the isolated Keychain test, open **Terminal.app** in the GUI login session and run:

```sh
cd /path/to/tulpa
python3 scripts/test-ssh-keychain.py --allow-temporary-keychain-item
```

Click **Allow** if macOS presents the expected Keychain prompt. The script builds the current binary, creates a throwaway encrypted key and its OpenSSH Keychain entry, starts temporary agent-free `sshd` and echo targets, checks one authenticated direct-channel exchange, and deletes only that temporary Keychain item. No privacy toggle is required.

### Debug-install review gates

- **Key protection and export refusal:** the current design creates no Keychain private key. It reads an existing OpenSSH identity file and only looks up its passphrase. That existing file-based identity cannot be described as non-extractable. If a future Keychain-key path is introduced, it must use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` and non-exportable key attributes. Before installation, create only a throwaway key in a temporary keychain and verify that `security export -k /path/to/test.keychain-db -t keys -f openssl -o /path/to/export.pem` is **refused**. This check is a gate for such a future path, not a claim about the current OpenSSH file identity.
- **Restricted server operation:** the client depends only on outbound SSH plus one `direct-tcpip` channel. It must work with `restrict,port-forwarding,permitopen=…,from=…` on the authorized key, `AllowTcpForwarding local`, and `PermitListen none`. Session, PTY, exec, remote forwarding, and agent forwarding are not required. The restricted-stock-`sshd` smoke test passed with one direct channel and no session or remote forward.
- **Debug prompts:** each ad-hoc rebuild can have a new code identity and prompt again. Click **Allow** only for a binary built locally from the reviewed source. Never click **Always Allow**. No system permission or TCC reset is part of the test.

### Parked Secure Enclave experiment

The abandoned experiment produced `-34018` for Secure Enclave and access-control creation without entitlements, and `-25308` for legacy Keychain creation in the non-GUI process. Apple documentation confirmed that `keychain-access-groups` requires an authorized provisioning profile and an app-like wrapper. Those findings explain the old failure but are not requirements for the selected OpenSSH design. The custom P-256 signer and direct `p256` dependency have been removed.

## Server discovery

Verified with `mdns-sd` 0.21.7 without its async or logging features:

- A real ephemeral Bonjour service was advertised, resolved, probed, selected as the LAN route, and unregistered. The ignored-by-default adapter test passed when run explicitly.
- Unit tests verify that a live LAN candidate prevents tunnel probes and that multiple configured tunnel entry points are tried until one is live.
- A real loopback TCP listener verified selection of a currently reachable tunnel endpoint over an unavailable one.
- Discovery runs on every call. Bonjour candidates are themselves probed before selection; stale records are not treated as live.
- Local TOML parsing supports any number of tunnel endpoints. `config.example.toml` contains only unusable placeholders. The spike binary takes the external config path as an argument.
- TCP liveness is not trust. Native host verification still runs after route selection.

The LAN test is excluded from routine CI because it emits multicast traffic. No real server or tunnel address was used or committed.

## Host aliases and proxy routes

- A non-empty local `host` value is treated as an OpenSSH Host alias and bypasses discovery. Real `ssh -G` resolves HostName, Port, User, IdentityFile, IdentitiesOnly, ProxyJump, and ProxyCommand, preserving Host/Match/Include behavior.
- Direct aliases use the resolved hostname and port. ProxyJump opens a direct channel from the authenticated jump session and runs the target SSH session over it with `russh::client::connect_stream`. Upstream sessions stay alive until the target exchange ends.
- ProxyCommand is used only when no ProxyJump is set. `/bin/sh` runs the command after `%h`, `%p`, and `%r` expansion, and its stdin/stdout form the SSH transport. The child is terminated with the route.
- Every hop checks the native `known_hosts` file and authenticates with its own resolved user and identities. Host-key prompts are serialized across hops, and recursive routes are bounded to eight hops.
- Multiple comma-separated ProxyJump hops are rejected as unsupported before any connection; tulpa never silently selects only the first hop. A unit test covers this refusal.
- Live throwaway tests passed for HostName/Port, ProxyJump, and ProxyCommand using `nc`. The ProxyJump case authenticated once at each hop and opened the expected direct channels at each server.
- The test sets `TULPA_OPENSSH_HOME` to a temporary directory and gives `ssh -G` an explicit temporary config path. No real SSH config or server was read or contacted.

## 3. Menu-bar states and pause hotkey

Implemented for the tray organ; no global hotkey yet and no pause latency is claimed.

- `tulpa-tray` splits into a platform-free core and a macOS presenter. The core is the menu state machine (`TrayModel`: read-only organ lines, a checked "Pause all" toggle, and "Quit"), an icon renderer (state-tinted disc with a purple mark, two bars while paused), an append-only UTC activity log, and TOML config (`[tray] enabled`, `[link] host`, per-organ `mode = "on" | "ask" | "off"` with `enabled` as fallback). Unit tests cover labels, menu shape, pause/quit events, tint distinctions, pixel-level icon states, log append semantics, and config defaults.
- The presenter (`macos.rs`, `cfg(target_os = "macos")`) runs a `tao` event loop with a `tray-icon` status item and a `muda` menu built from the model. "Pause all" logs the toggle and re-renders menu and icon; "Quit" logs and exits. Status clicks only open the menu. No RPC: pause and stop are local by construction.
- The app binary loads the config path (first argument, default `~/.config/tulpa/config.toml`), prints skeleton status when the tray is disabled, runs the presenter on macOS, and refuses loudly on other platforms instead of faking a tray.
- GUI crates (`tray-icon`, `muda`, `tao`) are macOS-target dependencies, so non-macOS builds compile and test the core only. `cargo-deny` resolves every target, which admits the GTK/GLib Linux counterparts of those crates into the lock; `deny.toml` carries the resulting license and duplicate-version exceptions.

Verified on Linux (aarch64, rust 1.99.0): `cargo build` and `cargo test` for `tulpa-tray` and `tulpa`, 19 tests green. Not verified here: `cargo fmt`, `cargo clippy -D warnings`, `cargo deny check`, doctests (no `rustdoc` component), `gitleaks` — these run on the macOS build host before signing and smoke.

## 4. Listening sockets and private values

- The application code has no listener. Loopback listeners exist only in the temporary interoperability and discovery tests and are closed on test exit.
- No real SSH identity or server is used by the smoke fixtures. Generated private keys and temporary Keychain items are outside the checkout and are cleaned up.
- Diffs are reviewed before every local commit. Full-history `gitleaks` is rerun after the commits; this report does not claim a scan of itself before it exists.

## Checks run

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny --locked check
gitleaks detect --source . --log-opts="--all" --redact
```

The report is updated only after these checks run on the final change. `cargo-deny` 0.20.2 remains the CI-matched checker. `russh` is temporarily pinned to upstream commit `b541663a88381aab9f6af1af7bbadb02e155058a`, which carries `ssh-key` 0.7.0-rc.12 and patched `rsa` 0.10.0. This avoids both the older agent-server advisory and the RSA timing advisory in released rc.11. Duplicate detection stays enabled with narrow exact-version exceptions for unavoidable transitive stacks.
