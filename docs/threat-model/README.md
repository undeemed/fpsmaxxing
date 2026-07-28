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

## Interim state of the local IPC boundary

The Linux broker in `apps/broker` implements the authenticated local IPC boundary; these parts of the required mitigations are not there yet.

- The peer ACL is an interim same-uid check (`SO_PEERCRED`). It refuses every other local user, but it does not separate an unprivileged gateway from a privileged broker: once the broker runs as a service account, the gateway it is meant to serve would be refused. The split-privilege ACL arrives with the Windows named-pipe SID authorizer, tracked as `fpsm-broker-splitacl`.
- The verified peer uid and pid are checked before any request is read and then dropped: they are not journaled against a lifecycle, and the client-supplied owner label is not authenticated against them. Both wait on the same follow-up, because under a same-uid ACL every authorized peer is one identity.
- Policy is enforced in the broker for the requests it serves, but the gateway still runs its own in-process control plane rather than calling the broker, so no shipped path crosses this boundary yet.

`docs/ARCHITECTURE.md` records the filesystem, single-instance, and fail-fast reasoning behind the boundary as built.
