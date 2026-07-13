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

## Required mitigations

- Typed allowlisted capabilities
- Local IPC authentication and ACLs
- Provider version and binary verification
- Policy enforcement in both gateway and broker
- Durable pre-state journal and TTL leases
- Independent watchdog and last-known-good baseline
- Fail-closed behavior on unknown state or missing telemetry
- Hardware-in-the-loop fault tests before enabling writes
