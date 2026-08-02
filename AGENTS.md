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

- Keep shared wire types in `crates/contracts`, and keep `schemas/*.json` in sync with them; the contract tests enforce matching fields and enum strings.
- Give every field whose checked-in schema constrains it beyond `type` (a `minLength`, a `pattern`, a numeric bound) or that carries a `deserialize_with` validator its own dedicated test that binds each checked-in schema carrying that constraint, not merely one of them; for a `deserialize_with` validator, which no checked-in schema can state, the counterpart to bind is that field's declared `type` in each schema publishing it.
  A test binds a schema by opening the checked-in file and asserting the constraint there; a sibling that only asserts what the Rust type rejects binds nothing and is not the pattern to copy. `protocol_version_zero_is_rejected_like_the_schema` and `the_hypothesis_is_bounded_like_the_schema` bind that way and are also complete for the constraints they assert, each opening the schema that carries it: `schemas/sidecar.schema.json` carries the `protocol_version` minimum alone, and `schemas/experiment.schema.json` the hypothesis length bounds alone. `change_request_parameters_are_an_object_in_both` binds that way for a declared `type`, but does so for fewer checked-in schemas than carry the one it asserts, the class registered as `fpsm-unbound-carrier-parity`.
  Schema parity compares property names, `required`, and `additionalProperties` only, whether through `assert_object_parity` and `assert_same_shape` or hand-rolled in `response_variants_match_schema`, `capability_fields_match_capability_schema`, and `manifest_fields_match_sidecar_schema`, so a mismatched `type` or a dropped bound otherwise stays green; not every such field carries its test today, and that gap is registered in the deferred work list in `docs/ARCHITECTURE.md` as `fpsm-capid-guard` where writing the guard is blocked, as it is for `capability_id`, and as `fpsm-unbound-carrier-parity` where it is merely pending.
- Keep provider lifecycle behavior in `crates/provider-sdk`.
- Keep the capability registry, policy, broker lifecycle, and experiment journal in `crates/control-plane`.
- Keep the local IPC transport, framing, and peer-authentication seams behind traits in `crates/ipc` (Unix domain socket now, Windows named pipe later); the privileged broker in `apps/broker` composes them over the control plane and enforces peer auth, catalog policy, and single-owner-per-knob. The non-`Send` control plane is confined to one worker thread reached through a `Send` handle.
- Keep the independent crash and lease recovery path in `apps/watchdog`; it reads the journal owned by `crates/control-plane` and writes only its own restore-outcome records, never the schema.
- Keep the measurement model, immutable evaluator, and replayable trial records in `apps/experiment-runner`; the evaluator stays a pure function of recorded samples and fixed bounds.
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
CI runs the same checks, plus JSON schema validation, on every pull request and push to `main`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
