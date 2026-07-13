# ADR 0001: Rust-first implementation

- Status: accepted
- Date: 2026-07-12

## Decision

Use Rust for every trusted process and shared library. Use a C# sidecar only where the selected hardware library is natively .NET.

## Rationale

Rust provides a strong fit for long-running services, explicit error handling, cross-platform native binaries, constrained unsafe code, and shared types across IPC, policy, and providers. A Cargo workspace is sufficient for the initial monorepo. Additional build systems would duplicate Cargo's dependency and task graph before the repository is large enough to benefit.

## Consequences

- Unsafe Rust is forbidden workspace-wide by default.
- Windows APIs use focused Microsoft Rust crates where possible.
- Sidecar protocols remain language-neutral JSON Schema contracts.
- A future UI may use Tauri, but no TypeScript toolchain is introduced before a UI exists.
