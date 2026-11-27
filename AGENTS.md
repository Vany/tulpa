# Working on tulpa

tulpa is a macOS body for an agent on a remote server. Keep the human's controls visible and the trust boundary local. This public repository currently contains a workspace skeleton, not working organs.

## Layout

- `crates/tulpa`: app binary.
- `crates/tulpa-ears-helper`: future bundled microphone helper binary.
- `crates/tulpa-segment`: cuts the ears' 16 kHz mono stream into whole utterances on pauses (with each one's span and end intonation); bundled into the app beside `packaging/macos/ears-capture.sh`, the ears' capture script.
- `crates/tulpa-audio-events`: CoreAudio's device events (default input, default output, the device list, whether a device runs) as callbacks, for tulpa-hear's mic and tap and the tray's device pickers; macOS only, and the one crate allowed `unsafe` (the C listener API). The only other `unsafe` is `crates/tulpa-hear/examples/switch_gap.rs`, measurement only and shipped in no binary: it makes a temporary aggregate device and sets the default input to measure the capture gap on a real switch.
- `crates/tulpa-stop`: the one stop event (Pause, stop, Quit) loops wait on instead of polling a flag, and a channel whose receiver also wakes on it; std only, shared by the tray, the hands and tulpa-hear.
- `crates/tulpa-{link,hands,ears,voice,eyes,calls,alarm,presence,notify,clip,files,secrets,health,tray}`: one library per organ, each with a documented trait in `src/lib.rs`.
- `config.example.toml`: placeholders for the intended local configuration; no loader yet.
- `.github/workflows/ci.yml`: GitHub-hosted macOS checks.
- `deny.toml`: dependency advisory, license, duplicate, and source policy.

Keep organ boundaries narrow. Do not choose a wire protocol, async runtime, or platform adapter just to fill a stub. Update README statuses only when evidence supports them.

## Public means public

- Never commit private hostnames, IPs, usernames, keys, host keys, tokens, identifying filesystem paths, or employer names, including in examples, test fixtures, logs, and Git history.
- Real config belongs in `~/.config/tulpa/config.toml`. Use obvious placeholders in the example. Local config and key material are gitignored; ignore rules are not a substitute for reviewing a diff.
- Commit `Cargo.lock`. Keep dependencies minimal and justify each addition for a security review.
- Before **every push**, run `scripts/public-check.sh` with the private word list over the commits to push ([docs/DEVELOP.md](docs/DEVELOP.md#before-a-public-push)), then `gitleaks detect --source . --log-opts="--all" --redact` over full history, and require zero findings from both. Install with `brew install gitleaks` if missing. Never bypass a finding. Push only when explicitly requested.

## Security and human control

- One outbound SSH connection, organs on channels, no listening ports. Use native `known_hosts`, reject changed keys, and ask before trusting an unknown key. Route directly on LAN, otherwise through a configured tunnel endpoint.
- The one exception to "no listening ports" is a port forward the agent asked for ([docs/USE.md](docs/USE.md#port-forwards)), and it has three limits. It listens on loopback only (`127.0.0.1`). It listens only while that forward is open: its time limit, its one connection by default, Pause and Quit end it. It is visible in the menu for as long as it is open. It may use only a port the Mac's own config lists, and the server key line must allow the same port.
- The transport uses `russh` with the user's existing OpenSSH identities and native `known_hosts` / `authorized_keys`. Resolve Host aliases through `ssh -G`, including HostName/Port/User and ProxyJump/ProxyCommand; verify and authenticate every hop. Honour `IdentityFile` / `IdentitiesOnly`, and try ssh-agent before Keychain passphrase lookup and an interactive prompt. Secure Enclave and provisioning-profile work is dropped.
- Enforce named commands, allowed roots, and listed tmux windows locally. Hands jobs need IDs, cancellation, streamed output, and parallel execution.
- Respect OS permissions and indicators for microphone, screen, camera, notifications, and Accessibility. Capture all displays with window exclusions; camera access is on request. The microphone helper belongs inside the signed app bundle.
- The tray must expose live organ states, a hotkey that pauses all senses, and a full stop. Keep a local append-only log in `~/Library/Logs/tulpa/`, excluding secrets and private payloads.
- Alarms must ring locally while disconnected. Use existing SSH agent identities where possible; Keychain passphrases and private-key contents must never be logged or sent to the agent.
- Security rests on local policy and server authorized-key restrictions, not secrecy of the code. Preserve clear revocation through stop, OS permissions, and the server key line.
- Builds and signing are local with a stable self-signed certificate so TCC grants survive rebuilds. The app never updates itself; the one unattended installer is the operator-ordered login build check (`packaging/macos/build-check.sh`), which at login and hourly compares the installed bundle's `Contents/Resources/build` marker with `refs/tags/latest-good` on the origin and runs the staged `update.sh` on a mismatch — signed bundles from the same local pipeline, no downloads from the network beyond that ref lookup.

## Rust and checks

Use stable Rust, edition 2024, inherited workspace metadata and lints. Prefer typed, narrow interfaces and standard library facilities. Document public APIs and fallible behavior. Do not hide stubs behind success, panic, or `unimplemented!`. The current binaries only identify themselves and exit.

Run from the repository root:

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny --locked check
```

Install the CI-matched checker with `cargo install cargo-deny --version 0.20.2 --locked`. Add meaningful behavior tests when implementations arrive; empty traits need compilation, not tautological tests. Exercise real adapters on macOS and report checks that could not run.

Keep CI light: GitHub-hosted macOS runners, read-only permissions, no repository secrets, no self-hosted runners, and every action pinned to its full commit SHA. CI covers fmt, clippy, tests, and cargo-deny.

## Commits

Use small, coherent commits with short imperative subjects, such as `Add the SSH signer spike`. Include implementation and its direct tests together. Review the full staged diff for private values and unrelated edits before committing. Never push without explicit authorization.
