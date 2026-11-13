# tulpa platform spike

Status: discovery is verified locally. The protected-key implementation and unit tests exist, but no real SSH login has succeeded yet. The remaining GUI setup is at the end of this document. No server address, host key, public-key line, local username, or identifying path is recorded here.

## 1. Permission grants and the bundled helper

Not attempted. There is no app bundle, microphone capture, screen capture, or camera capture yet. No permission grant is claimed.

## 2. Protected P-256 SSH login

### Implemented

- `security-framework` 3.7.0 creates or loads a P-256 key by a label from local configuration. The API exposes public-key bytes and signing, not private-key export or deletion.
- Secure Enclave creation uses `kSecAttrTokenIDSecureEnclave`, the data-protection Keychain, `AccessibleWhenUnlockedThisDeviceOnly`, and private-key-only access control.
- The fallback generates a software P-256 key in the default file-based login Keychain. Its default ACL belongs to the creating application; this fallback does not provide Secure Enclave isolation.
- `russh` 0.65.1 accepts the custom signer. `p256` 0.14.0 converts Security.framework's DER signature into the SSH ECDSA signature encoding. `libssh-rs` is not needed for API reasons.
- `PinnedHostKeyHandler` rejects an empty pin and any mismatch. Unit tests cover both cases and DER/SSH signature verification.
- The exchange implementation opens one outbound connection and one `direct-tcpip` channel. It has no session, PTY, listener, or reverse-forward request.

These are implementation and unit-test results, not a successful platform login.

### Exact platform failures observed

| Operation | Result |
| --- | --- |
| First lookup of a new key label | `-25300` (`errSecItemNotFound`); now handled as a cache miss |
| Secure Enclave `SecKeyCreateRandomKey` in the unsigned test process | `-34018` |
| File-based Keychain creation with `kSecAttrAccessControl` in that process | `-34018` |
| File-based Keychain creation with the creating-app default ACL in the non-GUI tmux process | `-25308` (`errSecInteractionNotAllowed`) |
| Ad-hoc signature claiming `keychain-access-groups` | macOS killed the process before entry |

Framework error descriptions were removed from the public error type because they contained public-key coordinates. The stable operation name and numeric error remain.

### Entitlement check

Apple's [TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains) explicitly confirms the restriction: data-protection Keychain access groups come from code-signing entitlements, those entitlements must be authorized by a provisioning profile, and Secure Enclave protection requires the data-protection Keychain.

Apple's [TN3125](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles) specifically lists `keychain-access-groups` as a restricted entitlement on macOS. The profile allowlists the entitlement and is embedded at `Contents/embedded.provisionprofile`. A self-signed root or ad-hoc signature cannot supply that authorization.

### Options and their cost

1. **Apple Development identity and provisioning profile — required for the preferred Secure Enclave path.** Xcode can use a Personal Team associated with an Apple Account at no program-membership cost. A paid Apple Developer Program team is the alternative if Xcode does not offer Keychain Sharing to the Personal Team. This requires account sign-in, network access, an app-specific profile, and eventual profile renewal. It has not yet been verified on this machine.
2. **An app-like bundle is required for that profile, but is not sufficient by itself.** A bare CLI binary has no profile location. The app wrapper supplies `Contents/embedded.provisionprofile` and a stable code identity, but still needs the Apple Development identity and authorized profile. A self-signed `.app` without that profile does not solve this failure. Apple's [restricted-entitlement wrapper example](https://developer.apple.com/documentation/xcode/signing-a-daemon-with-a-restricted-entitlement) documents this structure.
3. **Legacy file-based Keychain from the GUI login session — stopgap only.** Running the existing fallback from Terminal.app, or explicitly in the GUI user context with `launchctl asuser`, may permit the one-time **Allow** prompt instead of returning `-25308`. This is the fewest GUI steps but remains unverified and produces a software Keychain key, not a Secure Enclave key. It should not become the final transport design.

Recommendation: use options 1 and 2 together. They are both necessary for the fewest-steps path that can actually reach the Secure Enclave. Option 3 is useful only to test the SSH exchange while that setup is unavailable.

### Recommended GUI setup for vany

Xcode is already installed. No privacy toggle and no Keychain Access certificate creation are needed.

1. Open **Xcode → Settings → Accounts**, click **+**, choose **Apple Account**, and sign in. Accept the Apple Developer Agreement if prompted.
2. Choose **File → New → Project → macOS → App**. Create a temporary app project named `tulpa-provisioning` outside the public checkout.
3. Select the project, then its app target, then **Signing & Capabilities**. Enable **Automatically manage signing** and select the **Personal Team** from **Team**. Use a paid team instead only if one is already available or Keychain Sharing is unavailable to the Personal Team.
4. Click **+ Capability** and add **Keychain Sharing**. Leave the bundle identifier suitable for the future tulpa app; Xcode should create an Apple Development identity and provisioning profile automatically.
5. Choose **Product → Build** once. Do not run any capture code or change privacy settings.

The next implementation step is to put the Rust spike executable in an app-like wrapper, embed the resulting profile, sign it with the Apple Development identity, and repeat the key test. Only after that should the temporary `sshd` test be called proven.

## Server discovery

Verified with `mdns-sd` 0.21.7 without its async or logging features:

- A real ephemeral Bonjour service was advertised on the LAN, resolved, probed, selected as the LAN route, and unregistered. The ignored-by-default adapter test passed when run explicitly.
- Unit tests verify that a live LAN candidate prevents tunnel probes and that multiple configured tunnel entry points are tried until one is live.
- A real loopback TCP listener verified selection of a currently reachable tunnel endpoint over an unavailable one.
- Discovery runs on every call. Bonjour candidates are themselves probed before selection; a stale mDNS record is not treated as live.
- Local TOML parsing supports any number of tunnel endpoints. `config.example.toml` contains only unusable placeholders. The spike binary takes the external config path as an argument.
- TCP liveness is not trust. The SSH host-key pin remains mandatory after route selection.

The LAN test is intentionally excluded from routine CI because it emits multicast traffic. No real server or tunnel address was used or committed.

## 3. Menu-bar states and pause hotkey

Not attempted. No tray event loop or global hotkey is implemented, and no pause latency is claimed.

## 4. Listening sockets and private values

- After the build and discovery tests, `lsof` showed no listening TCP socket owned by tulpa, Cargo, or rustc test processes. Temporary listeners existed only inside the individual tests and were closed on exit.
- The staged discovery diff was reviewed before its local commit. No endpoint, key, host key, username, token, or identifying path was included.
- A local full-history `gitleaks detect --source . --log-opts="--all" --redact` scan covered all three commits and reported zero findings. The same scan remains a required gate before any future push.

## Checks run

The following passed on the deliverable before the discovery commit:

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny --locked check
```

`cargo-deny` 0.20.2 reported advisories, bans, licenses, and sources OK. `russh` 0.65.1 is required because earlier versions are affected by RUSTSEC-2026-0495. Its unavoidable transitive duplicate versions have narrow, exact-version exceptions in `deny.toml`; duplicate detection remains enabled.
