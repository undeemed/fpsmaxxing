# Architecture

FPSMaxxing separates reasoning, policy, privilege, hardware integration, measurement, and recovery.

The current read-only alpha implements the gateway and an in-process control-plane seam (`crates/control-plane`) holding the capability registry, bounded policy, broker lifecycle, and durable SQLite experiment journal, wired to a single mock provider; the privileged broker, watchdog, and experiment runner remain scaffolds.

## Processes

### Gateway

The unprivileged MCP server translates agent tool calls into typed capability requests. It may inspect public capability metadata and telemetry but cannot directly mutate the host. The alpha gateway serves line-delimited JSON-RPC over stdio.

### Broker

The privileged Windows service accepts authenticated local requests from the gateway, revalidates policy, journals the transaction, and supervises provider sidecars. It exposes no raw command or memory primitive.

### Watchdog

The independent watchdog owns lease deadlines and emergency rollback. It must restore state without the gateway, agent, or experiment runner. On lease expiry or a safety violation it reverts to the pre-state snapshot through the privileged broker.

### Experiment runner

The runner controls workload setup, warmup, repeated measurements, cooldown, correctness checks, and promotion decisions. Evaluator code is outside the LLM's writable surface.

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
