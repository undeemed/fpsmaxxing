# ADR 0002: Alpha experiment journal scope

- Status: accepted
- Date: 2026-07-20

## Decision

The alpha journal keeps a single `experiment_journal` table of stage records.
Each lifecycle allocates its correlation ID atomically with the snapshot record, commits a write-ahead `apply-intent` record holding the full change request before the provider mutates state, and closes with exactly one terminal `completed` or `failed` record carrying a structured error kind and the failing stage.
When a restore failure supersedes an earlier apply or verify failure, the `failed` record embeds the suppressed failure so the audit trail never loses the primary error.
Full two-phase intent/result journaling for every stage is deferred until the seam is promoted to the privileged broker.

The same journal database also keeps an `experiment_trials` table of self-describing trial records, added when the deterministic experiment engine landed.
This supersedes the original deferral of a dedicated experiments table.
A trial record is a different kind of row from a lifecycle stage record: it holds the spec, the recorded baseline and candidate samples, and the immutable evaluator's verdict, so a trial re-evaluates from the journal alone without chat history or a re-run workload.
Storing that under the stage schema would have meant either overloading `payload` with a shape `doctor` cannot interpret or restructuring the stage table, so a second table was the smaller change.
Trial rows follow the same write-ahead principle as the lifecycle journal: the runner records the measured trial before invoking the lifecycle and amends that row with the outcome, so a promotion the broker refuses still leaves the measurements that authorized it.

## Rationale

The write-ahead apply intent makes a crash between mutation and journaling distinguishable from an apply that never started, and the terminal record guarantees that a surviving process never leaves an experiment with only partial stage rows.
A per-stage two-phase protocol would duplicate that machinery for the mock-only alpha before the broker owns the transaction log.
Trial records carry a `schema_version` so a future field addition is a version bump a reader can refuse rather than a silent misread of journaled history.

## Consequences

- `doctor` reports experiments with an `apply-intent` record but no terminal outcome as dangling; it does not yet inspect `experiment_trials`.
- Correlation IDs are allocated inside an immediate transaction with a busy timeout, so concurrent gateways sharing one journal file cannot mint duplicate IDs.
- `LifecycleResult` stays in `crates/control-plane` as a deliberate alpha seam even though the gateway serializes it into MCP tool-result text; it moves to `crates/contracts` with a pinned JSON schema at broker promotion.
- Broker promotion revisits journaling as part of the privileged transaction log design.
