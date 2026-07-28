# Threat model

## Protected assets

- Host availability and bootability
- User data and workload correctness
- Registry, power, fan, clock, and process-policy state
- Privileged broker credentials and IPC endpoints
- Experiment history and rollback snapshots

## Primary threats

- Prompt injection causing a privileged action
- Sidecar impersonation or protocol confusion
- Parameter smuggling outside provider limits
- Conflicting tools racing over the same setting
- Telemetry loss hiding unsafe temperatures or errors
- Process termination leaving persistent changes behind
- Malicious or incompatible third-party binaries
- Reboot before a candidate configuration is blessed
- A proposed experiment declaring decision thresholds that disarm its own promotion gate
- Rewritten experiment history crediting a change with measurements it never produced

## Required mitigations

- Typed allowlisted capabilities
- Local IPC authentication and ACLs
- Provider version and binary verification
- Policy enforcement in both gateway and broker
- Durable pre-state journal and TTL leases
- Independent watchdog and last-known-good baseline
- Fail-closed behavior on unknown state or missing telemetry
- A policy-owned decision envelope a proposed experiment may tighten but never loosen
- Append-only trial records re-evaluated and re-gated against current policy on replay
- A signed or hash-chained journal before recorded measurement content itself is trusted
- Hardware-in-the-loop fault tests before enabling writes
