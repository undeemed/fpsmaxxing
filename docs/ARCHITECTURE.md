# Architecture

FPSMaxxing separates reasoning, policy, privilege, hardware integration, measurement, and recovery.

The current read-only alpha implements the gateway, an in-process control-plane seam (`crates/control-plane`) holding the capability registry, bounded policy, broker lifecycle, and durable SQLite experiment journal, and a deterministic experiment runner (`apps/experiment-runner`) that gates measured trials through an immutable evaluator, all wired to a single mock provider.
The independent watchdog restore path is implemented against that journal on the Linux-safe mock path (`apps/watchdog`); the privileged broker remains a scaffold.

## Processes

### Gateway

The unprivileged MCP server translates agent tool calls into typed capability requests. It may inspect public capability metadata and telemetry but cannot directly mutate the host. The alpha gateway serves line-delimited JSON-RPC over stdio.

### Broker

The privileged Windows service accepts authenticated local requests from the gateway, revalidates policy, journals the transaction, and supervises provider sidecars. It exposes no raw command or memory primitive.

### Watchdog

The independent watchdog owns lease deadlines and emergency rollback. It must restore state without the gateway, agent, or experiment runner. On lease expiry or a safety violation it reverts to the pre-state snapshot through the privileged broker.

`apps/watchdog` implements this on the Linux-safe mock path.
It reads the durable journal owned by the control plane and scans for experiments that recorded a write-ahead `apply-intent` with no terminal `completed` or `failed` record: either a crash between the mutation and its rollback, or an elapsed TTL lease.
Each selected experiment is rolled back through its provider using the journaled snapshot, and the restore is verified by re-reading provider state against that snapshot.
The watchdog writes nothing to the journal except its own correlation-ID-linked `watchdog-restore` record and the terminal record that closes an experiment it has restored, so it never depends on the components it recovers from and never mutates the schema.
Restores are idempotent: a closed experiment is never rolled back twice, and a failed rollback is left unclosed for a later pass to retry.
A steady-state poll reclaims only expired leases, while a crash-recovery pass (`--recover-all`) reclaims every unclosed experiment; a single `--once` pass is the unit a later Windows-service timer would drive.

### Experiment runner

The runner controls workload setup, warmup, repeated measurements, cooldown, correctness checks, and promotion decisions. Evaluator code is outside the LLM's writable surface.

In the safe alpha (`apps/experiment-runner`) the runner measures a baseline and a candidate against a deterministic model, then a pure immutable evaluator returns a promote or reject verdict from the recorded samples and fixed bounds alone - no clock, no LLM, no I/O. Every trial is journaled as a self-describing, versioned record so it can be replayed and re-evaluated from the journal without the original conversation.
The record is appended once, after the broker lifecycle it authorized has finished and carrying either that lifecycle's outcome or the error the broker returned, so a promotion the broker refuses still leaves the measurements that authorized it and no API can rewrite a recorded verdict.
Sample counts are bounded by the spec schema and rechecked by the runner before any measurement runs, the target must name the one capability the measurement model describes and the accepted provider advertises so unknown hardware fails closed before anything is measured or journaled, the candidate and the provider's own baseline are both held to the knob ceiling the broker policy enforces later, and the spec's decision bounds are intersected with a policy-owned envelope - declared once in `crates/contracts`, mirrored in `schemas/experiment.schema.json`, and re-checked on replay - so a spec can tighten its own safety gate but never loosen it.
Replay applies that whole gate again to the journaled record rather than the bounds alone, so a row rewritten after the fact is reported as outside policy even when re-evaluating it reproduces the recorded verdict.
Because mock capabilities are leased and the broker lifecycle always rolls back, the verdict gates whether the candidate is applied at all rather than whether it persists; durable keep-or-rollback awaits the privileged broker.

### Provider sidecars

Each sidecar integrates exactly one service or vendor API. Sidecars advertise semantic capabilities and implement snapshot, preview, apply, verify, and rollback. They do not make cross-provider decisions.

## Dependency rule

Dependencies point inward:

```text
apps and sidecars → shared crates
shared crates     ↛ apps or sidecars
sidecar A         ↛ sidecar B
```

Non-Rust compatibility code lives under `bridges/` and speaks the same versioned sidecar protocol.
