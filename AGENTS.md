# Working on tulpa

tulpa is a macOS body for an agent on a remote server. Keep the human's controls visible and the trust boundary local. This public repository currently contains a workspace skeleton, not working organs.

## Layout

- `crates/tulpa`: app binary.
- `crates/tulpa-ears-helper`: future bundled microphone helper binary.
- `crates/tulpa-{link,hands,ears,voice,eyes,calls,alarm,presence,notify,clip,files,secrets,health,tray}`: one library per organ, each with a documented trait in `src/lib.rs`.
- `config.example.toml`: placeholders for the intended local configuration; no loader yet.
- `.github/workflows/ci.yml`: GitHub-hosted macOS checks.
- `deny.toml`: dependency advisory, license, duplicate, and source policy.

Keep organ boundaries narrow. Do not choose a wire protocol, async runtime, or platform adapter just to fill a stub. Update README statuses only when evidence supports them.

## Public means public

- Never commit private hostnames, IPs, usernames, keys, host keys, tokens, identifying filesystem paths, or employer names, including in examples, test fixtures, logs, and Git history.
- Real config belongs in `~/.config/tulpa/config.toml`. Use obvious placeholders in the example. Local config and key material are gitignored; ignore rules are not a substitute for reviewing a diff.
- Commit `Cargo.lock`. Keep dependencies minimal and justify each addition for a security review.
- Before **every push**, run `gitleaks detect --source . --log-opts="--all" --redact` over full history and require zero findings. Install with `brew install gitleaks` if missing. Never bypass a finding. Push only when explicitly requested.

## Security and human control

- One outbound SSH connection, organs on channels, no listening ports. Pin the host key and fail closed. Route directly on LAN, otherwise through a jump host.
- The transport spike decides between `russh` with a custom Secure Enclave P-256 signer and fallback `libssh-rs`. Do not claim either is proven before the spike.
- Enforce named commands, allowed roots, and listed tmux windows locally. Hands jobs need IDs, cancellation, streamed output, and parallel execution.
- Respect OS permissions and indicators for microphone, screen, camera, notifications, and Accessibility. Capture all displays with window exclusions; camera access is on request. The microphone helper belongs inside the signed app bundle.
- The tray must expose live organ states, a hotkey that pauses all senses, and a full stop. Keep a local append-only log in `~/Library/Logs/tulpa/`, excluding secrets and private payloads.
- Alarms must ring locally while disconnected. Keep raw keys behind Keychain / Secure Enclave operations; never send them to the agent.
- Security rests on local policy and server authorized-key restrictions, not secrecy of the code. Preserve clear revocation through stop, OS permissions, and the server key line.
- Builds and signing are local with a stable self-signed certificate so TCC grants survive rebuilds. No auto-update.

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
