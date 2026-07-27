# Architecture

FPSMaxxing separates reasoning, policy, privilege, hardware integration, measurement, and recovery.

The current read-only alpha implements the gateway, an in-process control-plane seam (`crates/control-plane`) holding the capability registry, bounded policy, broker lifecycle, and durable SQLite experiment journal, and a deterministic experiment runner (`apps/experiment-runner`) that gates measured trials through an immutable evaluator, all wired to a single mock provider.
The privileged broker exposes that control plane over an authenticated local IPC boundary on the Linux-safe socket path (`apps/broker`, `crates/ipc`), and the independent watchdog restore path is implemented against the journal on the Linux-safe mock path (`apps/watchdog`).
The gateway does not route through the broker yet - it still opens an in-process control plane of its own - so the two paths run side by side over separate journals.

## Processes

### Gateway

The unprivileged MCP server translates agent tool calls into typed capability requests. It may inspect public capability metadata and telemetry but cannot directly mutate the host. The alpha gateway serves line-delimited JSON-RPC over stdio.

### Broker

The privileged Windows service accepts authenticated local requests from the gateway, revalidates policy, journals the transaction, and supervises provider sidecars. It exposes no raw command or memory primitive.

`apps/broker` implements this on the Linux-safe path.
It owns the control plane on a dedicated worker thread and serves exactly two operations - capability discovery and a bounded provider lifecycle - to authenticated local peers over the transport seam in `crates/ipc` (a Unix domain socket now, a Windows named pipe later).
Three fail-closed checks guard the boundary: peer authentication before any request is read (a Linux `SO_PEERCRED` same-uid ACL, shaped so a Windows SID ACL can satisfy the same trait), a capability-catalog check that rejects raw shell, arbitrary Registry paths, and out-of-catalog ids, and single-owner-per-knob enforcement that refuses a second concurrent owner of a setting.
Typed request and response messages live in `crates/contracts` with `schemas/*.json` kept in sync; a malformed frame is rejected with a typed error without taking the broker down.

The same-uid ACL is the deliberate interim policy for the current single-user, Linux-safe mock path, where the broker and its only client run as the same desktop user.
It is not the shipping policy for the privilege split described above: once the broker runs as a service account, an unprivileged gateway would be refused by construction.
The real privilege-split ACL arrives with the Windows named-pipe SID implementation of the `PeerAuthorizer` trait, tracked separately; because every caller reaches the ACL through that trait, no call site changes when it lands.
The trusted uid is read from the broker's own effective credentials rather than from the socket file's owner, so replacing the socket path cannot redirect the ACL.

The broker fails fast rather than degrading.
Losing the control-plane worker thread - including to a panic mid-lifecycle, which may leave provider state applied and un-rolled-back - stops the serve loop and exits the process non-zero instead of answering later requests with an internal fault.
Deploy it under a supervisor that restarts on failure, and let the watchdog own recovery of any state left behind.

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
Sample counts and the hypothesis text are bounded by the spec schema and rechecked by the runner before any measurement runs, the target must name the one capability the measurement model describes and the accepted provider advertises so unknown hardware fails closed before anything is measured or journaled, the free-form parameter object that capability is invoked with is held to the keys it actually takes so nothing unread is journaled, the candidate and the provider's own baseline are both held to the knob ceiling the broker policy enforces later, the target's TTL lease is held to the ceiling the broker enforces on every change request, and the spec's decision bounds are intersected with a policy-owned envelope - declared once in `crates/contracts`, mirrored in `schemas/experiment.schema.json`, and re-checked on replay - so a spec can tighten its own safety gate but never loosen it.
Replay applies that whole gate again to the journaled record rather than the bounds alone, so a row whose spec, capability, hypothesis length, declared sample counts, target parameters, or TTL lease was rewritten after the fact is reported as outside policy even when re-evaluating it reproduces the recorded verdict; the gate applied is the current one, so tightening a policy constant deliberately flags archived rows recorded under the looser ceiling (ADR 0002).
The two recorded knob values are held more weakly than that list reads: the candidate value is cross-checked only against the copy of itself the spec carries in `parameters.value`, and the baseline value is held to the knob ceiling with no second term at all.
The recorded samples themselves are not re-derived and nothing ties them to the values they were taken at, and the hypothesis is held to its length rather than its content, so three rewrites pass replay - the measurements together with the verdict they imply, the hypothesis text within its bounds, and a coherent relabelling of the knob values that edits `parameters.value` and the recorded candidate value together to another in-bounds value or moves the baseline to another value under the ceiling.
Re-deriving the samples would in fact work today, because the stand-in model is a pure function of values the record already carries, and it is deliberately not done: real telemetry is not reproducible, which is why samples are journaled verbatim in the first place, so a gate built on the model's reproducibility would have to be deleted the moment that model is replaced.
Measurement-content integrity needs a signed or hash-chained journal instead, which the alpha defers rather than approximates.
Because mock capabilities are leased and the broker lifecycle always rolls back, the verdict gates whether the candidate is applied at all rather than whether it persists; durable keep-or-rollback awaits the privileged broker.
The candidate is also measured before it is applied, which only the pure stand-in model permits: real `PresentMon` or hardware telemetry cannot observe a candidate that was never written, so swapping it in moves the candidate measurement inside the apply-and-lease window and runs the gate after it.

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
