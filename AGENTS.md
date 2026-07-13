# Agent instructions

Start with `docs/README.md`. Read `docs/IMPLEMENTATION_PLAN.md`, `docs/ARCHITECTURE.md`, and `docs/threat-model/README.md` before changing architecture or privileged code.

## Non-negotiable boundaries

- The LLM-facing gateway is not privileged.
- The broker never accepts raw shell commands, arbitrary Registry paths, arbitrary memory addresses, MSR indices, port I/O, or firmware variables.
- Every mutating capability requires a pre-state snapshot, bounded parameters, a verification probe, a TTL lease, and rollback.
- Unknown provider versions and unknown hardware fail closed.
- Only one provider may own a knob at a time.
- The watchdog must be able to restore state without the gateway or LLM.
- BIOS, voltage, firmware flashing, Secure Boot, TPM, boot settings, and kernel patching are out of scope unless the implementation plan is explicitly amended.

## Repository rules

- Keep shared wire types in `crates/contracts`.
- Keep provider lifecycle behavior in `crates/provider-sdk`.
- Put provider-specific code in one `sidecars/<provider>` package; sidecars may not import each other.
- Put non-Rust compatibility processes under `bridges/` and isolate them behind the sidecar protocol.
- Do not vendor third-party binaries without confirmed redistribution rights.
- Add tests before enabling a real write path.

## Required validation

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

`cargo deny check` requires [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) and enforces the advisory, ban, license, and source policies in `deny.toml`.
CI runs the same checks, plus JSON schema validation, on every push and pull request.
