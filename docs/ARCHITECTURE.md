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
Typed request and response messages live in `crates/contracts` with `schemas/broker-request.schema.json` and `schemas/broker-response.schema.json` kept in sync; a malformed frame is rejected with a typed error without taking the broker down.
A frame is a big-endian `u32` body length followed by that many bytes of JSON, bounded at one mebibyte so a hostile peer cannot force an unbounded allocation; the framing is transport-agnostic, so a named-pipe transport would reuse it unchanged.
At most 32 connections are served at once, and one that stalls for 30 seconds in either direction is closed, so a peer cannot pin a task, a descriptor, or a frame buffer; a peer refused by the ACL gets a far shorter deadline to take its rejection frame, because it has not authenticated and must not be able to hold a connection slot for the full idle budget.
A response is a tagged union of its outcome and that outcome's payload, so a tag without its payload never crosses the boundary and no consumer has to unwrap one.
Any text a client chose is truncated before it is quoted back in an error message, so a rejected request is always answered with a typed error rather than a response too large for the frame limit.

The same-uid ACL is the deliberate interim policy for the current single-user, Linux-safe mock path, where the broker and its only client run as the same desktop user.
It is not the shipping policy for the privilege split described above: once the broker runs as a service account, an unprivileged gateway would be refused by construction.
The real privilege-split ACL arrives with the Windows named-pipe SID implementation of the `PeerAuthorizer` trait, tracked separately; because every caller reaches the ACL through that trait, no call site changes when it lands.
The trusted uid is read from the broker's own effective credentials rather than from the socket file's owner, so replacing the socket path cannot redirect the ACL.
In this interim every authorized peer is the same identity, so journaling the verified peer uid and pid against each lifecycle, and authenticating the client-supplied owner label against them, only carry their weight once split-privilege ACLs arrive; both are tracked as follow-up work `fpsm-broker-splitacl`.
A peer refused by the ACL is told only that it is not authorized: the refusal names neither uid, because the caller it reaches has not authenticated, and the uid pair is traced locally instead.
The broker always establishes one owner-only directory of its own under `$XDG_RUNTIME_DIR` (or `/run`), and unless an explicit path is given it keeps its socket and its journal there rather than beside the inherited working directory, so no other user can race the socket path or read the audit journal.
`XDG_RUNTIME_DIR` is inherited from whoever started the broker, so it is honored only when it is absolute and only for an unprivileged broker; a broker running as root always uses `/run`.
That directory and every directory above it must be owned by the broker or root and unwritable by anyone else, or the broker fails closed: a writable parent is what would let another user swap a vetted directory for a symlink between the check and the bind.
Every resolved path is held to that same bar, whether a flag, an environment variable, or the default named it: it must be absolute, and the whole chain above it is vetted, so an inherited variable cannot buy a caller the placement `XDG_RUNTIME_DIR` is filtered to deny them.
The directory that directly holds the socket or the journal is held higher still: no other user may reach it at all, sticky or not.
Sticky only stops another user renaming or removing the broker's entry, not creating that entry first in a shared directory like `/tmp` and keeping ownership of the file the broker then writes every `apply-intent` record into.
Traversal alone is enough to reach the socket, whose own mode cannot be pinned, so a directory like `/run` that merely lets everyone through is refused as well.
Above that directory, neither is the threat and swapping a directory is, which is exactly what sticky prevents, so the not-writable-by-others bar with its sticky exemption stands where it is sound.
The socket file's own mode is not pinned, because the only ways to do so are a symlink-following `chmod` or a window in the process-global umask; confidentiality rests on the owner-only directory and the `SO_PEERCRED` gate instead.
What keeps single-owner-per-knob true across processes is an exclusive advisory lock rather than the bind: the broker locks a fixed `0600` file in its private directory before the journal is opened and before the socket is bound, so a second broker refuses to start instead of driving the same knobs through an ownership ledger of its own, and it refuses before it has touched either.
The bind cannot do that job, because it cannot be made atomic: stat, probe, unlink, and bind are four steps, and two brokers that both found the same socket file stale would leave the first serving an unlinked inode - still answering its connected clients - while the second owned the path, with nothing logged anywhere.
The kernel releases the lock with the last descriptor for it, so a broker that crashed leaves its successor nothing to clean up, and the socket file it left behind is simply rebound.
The transport unlinks on drop only the entry it bound itself - matched by device and inode - so an exiting instance can never strand a live successor.
The lock is placed by uid rather than derived from the socket or the journal, and the private directory holding it is established even when both of those are overridden, so neither `--socket` nor `--journal` nor the environment variables behind them buy a second instance.
Keying it to a path would not hold: only the path left unset falls back to the default, so two brokers given different journals would take two locks and then share one socket.
What they contend for is the machine's knobs, not a file, and a supervisor-level single-instance unit can layer on top later.
For the privileged deployment the guarantee is unconditional: a root broker ignores `XDG_RUNTIME_DIR`, so its private directory is always the fixed `/run/fpsmaxxing` and there is exactly one lock file to contend for.
An unprivileged broker keeps that directory wherever the inherited `XDG_RUNTIME_DIR` puts it, so the lock moves with the variable: one user starting two brokers under two different values for it takes two locks, and both start.
Single-instance is therefore best-effort off the privileged path, which is the dev and test path it serves.
That concession gives up no boundary.
A same-uid caller who can vary `XDG_RUNTIME_DIR` is already inside the trust domain the same-uid ACL admits, and could drive the same knobs through the running broker without starting a second one; the shipping broker runs as root, where the variable is refused and the guarantee is airtight.
The audit journal is different: it holds every `apply-intent` record and outlives the process, so it is created mode `0600` before it is opened, which also restricts the rollback journal and write-ahead log `SQLite` creates beside it.
The broker reads only broker-specific overrides (`FPSMAXXING_BROKER_SOCKET`, `FPSMAXXING_BROKER_JOURNAL_PATH`) and never the `FPSMAXXING_JOURNAL_PATH` the unprivileged gateway, CLI, and watchdog use, so an operator who exported that variable cannot move the privileged audit journal.
Both are read as raw `OsString` values, so a path that is not UTF-8 relocates the socket or journal as configured rather than being silently dropped back to the default.
The command line is read as `OsString` too, but it is matched against flag names rather than used verbatim, so an argument that is not UTF-8 is a typed fatal parse error naming it - not the mid-iteration panic `env::args` would raise, and not a silent fallback either.
[Broker operations and deployment](BROKER_OPERATIONS.md) turns these rules into the procedure for running the broker, down to the systemd `RuntimeDirectory` settings a unit needs.

The broker fails fast rather than degrading.
Losing the control-plane worker thread - including to a panic mid-lifecycle, which may leave provider state applied and un-rolled-back - stops the serve loop and exits the process non-zero instead of answering later requests with an internal fault.
Deploy it under a supervisor that restarts on failure, and let the watchdog own recovery of any state left behind.

#### Deferred broker work

- Gateway wiring: nothing shipped connects to the broker yet, because the gateway still opens its own in-process control plane, so `BrokerClient` in `crates/ipc` is exercised only by the broker's end-to-end tests. Promoting the gateway onto that client also has to settle what becomes of the gateway's own journal once the privileged audit journal is the record of every apply.
- `fpsm-broker-splitacl`: the real split-privilege ACL, replacing the interim same-uid policy described above, plus journaling the verified peer uid and pid against each lifecycle and authenticating the client-supplied owner label against them.
- Client reconnect on `Closed`: the broker closes a connection idle for 30 seconds, and `BrokerClient` holds one long-lived stream with no keepalive or reconnect, so a caller whose requests are further apart than that gets `ClientError::Closed`. Whether the client reconnects transparently, the server distinguishes a healthy idle peer from a stalled one, or callers connect per request is undecided.
- `broker-dispatch-unbounded`: neither `BrokerHandle::dispatch` nor `BrokerClient::request` bounds the wait on the single control-plane worker, so a provider that blocks inside `apply` or `verify` stalls every peer rather than failing one. Deferred to the pass that adds graceful shutdown, which has to answer the same question: what a request already in flight is owed when the broker stops serving.
- `fpsm-lease-ceiling-parity`: the missing `maximum` on `$defs.ChangeRequest.lease_seconds` in `schemas/broker-request.schema.json`, which declares only a minimum while `MAX_LEASE_SECONDS` and the change request lease in `schemas/experiment.schema.json` both carry the ceiling, and the gateway's `tools/list` input schema, which states the ceiling independently as the `MAX_LEASE_SECONDS` doc comment intends but which no test binds to that constant in agreement. Raising `MAX_LEASE_SECONDS` on its own is caught: within `lease_seconds_is_bounded_like_the_schema` the assertion comparing the checked-in experiment literal against the constant fails the moment the constant moves, while the assertion comparing the generated value against that same constant moves with it and stays green. Once the experiment schema has been brought along, nothing is left to fail and the gateway advertises a stale ceiling with the suite green. The deferred work is the binding test, not a reference from the gateway to the constant. The control-plane policy check rejects an over-ceiling lease, so this is drift in the published contract rather than in enforcement.
- `fpsm-unbound-carrier-parity`: the class of constraint whose dedicated test binds fewer checked-in schemas than carry it, leaving the unbound carriers free to drift with the suite green. The quantifier ranges over each checked-in schema carrying the triggering constraint, read per constraint rather than per field, so one field can be compliant for one of its constraints and not for another, and a carried constraint no test binds at all is inside the class whether a lone schema carries it or several do. What separates this class from `fpsm-capid-guard` is that the Rust field here carries a counterpart for the triggering constraint itself, read per constraint as above: `schemars` emits the minimum for the non-zero lease, and the declared `type` where the attribute names the field a JSON object map. So a guard that proves parity is writable today and the work is pending rather than blocked. A plain `String` also emits a declared `type`, but nothing that a `pattern` or a `minLength` can be held against, which is why `capability_id` stays with the blocked class. The lease floor is the worked example: the change request lease declares the same minimum under `$defs.ChangeRequest` in `schemas/broker-request.schema.json` and under `$defs.change_request` in `schemas/experiment.schema.json`, and no test binds that minimum in either - `lease_seconds_is_bounded_like_the_schema` asserts only the ceiling, and `lease_seconds_zero_is_rejected` asserts only what the Rust type rejects, which binds nothing. That same field is compliant for its ceiling, which every checked-in schema carrying it binds. `ChangeRequest::parameters` is a second instance: `change_request_parameters_are_an_object_in_both` binds the broker request schema and the generated schema, while `schemas/experiment.schema.json` publishes the same field's declared `type` and nothing opens it. A lone carrier falls in the same way: `DecisionBounds::min_samples` declares its minimum in `schemas/experiment.schema.json` alone, and `decision_bounds_are_bounded_like_the_schema` skips that keyword while binding the improvement floor and the exclusive minimums beside it out of the same envelope, so the hole is per keyword in a table that demonstrably does floors.
- `fpsm-capid-guard`: the dedicated guard `AGENTS.md` requires cannot prove parity today for the class of field whose checked-in schema carries a constraint with no Rust-side counterpart to compare against, because the Rust field is a plain type with no constraint attribute and the generated schema therefore states nothing a guard could hold the checked-in constraint against. A guard that opens the checked-in file and asserts the constraint is writable today, but it pins that file against itself and proves no parity. `ChangeRequest::capability_id` is the worked example, and it carries a second blocker of its own: its checked-in schemas constrain it differently from each other, a `pattern` in one and a `minLength` in the other. Closing the class needs each such Rust type to carry its constraint, and for `capability_id` the disagreeing schemas settled onto one, before a guard can prove parity; it is a prerequisite chain, not a missing test.

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
