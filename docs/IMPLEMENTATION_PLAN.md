# FPSMaxxing implementation plan

## Goal

Build a Windows-first, Rust-first performance-control gateway that lets an LLM inspect, propose, measure, and roll back safe system tuning experiments through typed capabilities rather than administrator shell access.

The first prototype targets one workstation. Third-party tools remain user-installed and are detected at runtime. BIOS writes, voltage changes, arbitrary Registry edits, raw hardware access, and firmware flashing are excluded.

## Architecture

```text
LLM / outer research loop
  │ typed MCP tools
  ▼
Unprivileged gateway
  │ validated request
  ▼
Capability registry + policy engine
  │ authenticated local IPC
  ▼
Privileged broker ──► provider sidecars ──► supported vendor/service APIs
  │
  ├──► durable experiment journal
  └──► independent watchdog and rollback
```

The LLM proposes declarative experiments. It never receives administrator credentials, a generic shell, arbitrary Registry access, or raw device primitives.

## Language and repository decisions

- Rust 2024 edition on the pinned stable toolchain
- Cargo workspace and `xtask` orchestration
- Tokio for async services and Windows named pipes
- Official Rust MCP SDK for the northbound agent interface
- Microsoft Rust crates for focused Windows Registry and service APIs
- Serde and Schemars for wire contracts and JSON Schema
- SQLite journal through Rusqlite
- One isolated C# bridge for LibreHardwareMonitor
- No Node, Turborepo, Bazel, or Tauri in v1

Source is organized by deployable role and provider. Release artifacts are assembled by OS and target triple.

## Provider contract

Every provider implements the same lifecycle:

1. `discover` — version, health, target, capabilities, valid ranges, conflicts.
2. `snapshot` — capture the exact state required for reversal.
3. `preview` — produce a typed, human-readable change description.
4. `apply` — perform one policy-approved bounded change.
5. `verify` — read back provider state and observe the real system effect.
6. `rollback` — restore the snapshot without consulting the LLM.

Every mutation has a TTL lease. If the gateway, workload, or agent dies, the watchdog restores the prior state.

## Integration order

| Order | Provider | Initial boundary | Promotion evidence |
| --- | --- | --- | --- |
| 1 | LibreHardwareMonitor bridge | Read-only telemetry | Stable normalized samples under workload |
| 1 | PresentMon | Frame and latency metrics | Correlated frame and hardware timeline |
| 2 | Process Lasso | Switch reviewed profiles | Apply, verify, and restore baseline profile |
| 2 | Windows power APIs | Clone and activate project-owned schemes | User's original scheme remains untouched |
| 3 | Registry provider | Curated catalog only | Exact type, view, value, effect, and rollback verified |
| 3 | NVML / AMD SMI | Supported clocks and power limits | Limits queried; default reset and watchdog tested |
| 3 | Fan Control | Switch complete JSON profiles | Fallback profile activates after process loss |

## One-hour AI implementation sprint

The following times are autonomous coding budgets assuming dependencies, administrator access, and target hardware are already available. Benchmark duration, thermal soak, reboots, signing, licensing, and human approvals are validation time and remain evidence-gated.

| Phase | Budget | Output | Exit gate |
| --- | ---: | --- | --- |
| Workspace and contract | 5 min | Cargo workspace, schemas, provider traits, CI | Mock provider passes full lifecycle |
| Read-only alpha | 10 min | MCP gateway, journal, telemetry adapters | Reproducible baseline report |
| Safe actuators | 15 min | Broker, IPC ACLs, Process Lasso, power, Registry leases | Gateway termination restores state |
| Hardware adapters | 10 min | GPU and fan profile providers, watchdog | Fault injection restores defaults |
| Experiment engine | 10 min | Baseline/candidate trials and decision gate | Trial replays without chat history |
| Hardening | 10 min | Reboot recovery, tamper tests, installer and diagnostics | Uninstall restores the baseline |

## Experiment loop

```text
observe
  → propose a typed hypothesis
  → intersect with policy and provider limits
  → snapshot
  → apply one change with a TTL
  → warm up
  → run repeated baseline/candidate measurements
  → enforce correctness, temperature, power, and error constraints
  → keep or roll back
```

Use the LLM for hypothesis generation and explanation. Use deterministic search and statistics for numeric optimization.

## Safety invariants

- No generic shell or raw Registry path reaches the broker.
- Unknown hardware and provider versions fail closed.
- Every write has a snapshot, TTL, verification probe, and rollback method.
- Only one provider owns a setting at a time.
- Critical thermal limits remain owned by hardware, firmware, and drivers.
- The watchdog is independent of the gateway and experiment runner.
- Persistent BIOS, security, voltage, and firmware operations require a future design amendment.

## MVP acceptance criteria

- Installed providers and capabilities are discovered.
- A complete baseline snapshot is journaled.
- Process Lasso profile apply and restore are demonstrated.
- One power, Registry, GPU, and fan transaction passes its contract tests.
- Crash, timeout, telemetry loss, and reboot rollback are demonstrated.
- One measured experiment is promoted or rejected using the immutable evaluator.
- Uninstall removes project-created services, profiles, rules, and Registry values.
