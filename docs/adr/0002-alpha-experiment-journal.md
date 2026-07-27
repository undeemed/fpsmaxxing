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
Trial rows are append-only: the journal exposes an insert and no update, and the runner writes each trial exactly once, after the lifecycle it authorized has finished, carrying either that lifecycle's outcome or the error the broker returned.
A promotion the broker refuses therefore still leaves the measurements that authorized it, and a recorded verdict has no API that can rewrite it.
Crash safety for the window between a mutation and that record stays with the lifecycle journal's write-ahead `apply-intent` record rather than being duplicated in the trial table.

## Rationale

The write-ahead apply intent makes a crash between mutation and journaling distinguishable from an apply that never started, and the terminal record guarantees that a surviving process never leaves an experiment with only partial stage rows.
A per-stage two-phase protocol would duplicate that machinery for the mock-only alpha before the broker owns the transaction log.
Trial records carry a `schema_version` so a future field addition is a version bump a reader can refuse rather than a silent misread of journaled history.
The version gate only catches a writer that bumps it, so the record types also reject unknown fields: a row carrying a field this build does not know, under a version it does, means a divergent writer or a rewritten row and fails the replay rather than decoding with that field dropped.
Replay re-runs the policy gate over the journaled spec as well - capability, sample counts, decision bounds, and candidate value - and cross-checks the record's redundant fields against that spec, because a rewritten row re-evaluates to the verdict it carries and so is invisible to a verdict comparison alone.
A row whose thresholds were widened, whose target was pointed at a capability the measurement model never described, or whose samples contradict the counts its spec declared is reported as outside policy even when the recomputed verdict matches.
That detection is structural only: replay checks the spec, the capability, the declared sample counts, the candidate value, and the baseline ceiling, but does not re-derive the recorded samples, so a rewrite of the measurements together with the verdict they imply passes both checks.
Detecting a coherent rewrite of that kind requires anchoring each row outside itself - a signed or hash-chained journal - which is deliberately out of scope for the alpha.
Replay reads the journaled capability against the constant the measurement model describes rather than the attached provider's manifest, so an archived journal audits the same way under any provider instead of raising a false tamper alarm.

## Consequences

- `doctor` reports experiments with an `apply-intent` record but no terminal outcome as dangling; it does not yet inspect `experiment_trials`.
- Correlation IDs are allocated inside an immediate transaction with a busy timeout, so concurrent gateways sharing one journal file cannot mint duplicate IDs.
- `LifecycleResult` stays in `crates/control-plane` as a deliberate alpha seam even though the gateway serializes it into MCP tool-result text; it moves to `crates/contracts` with a pinned JSON schema at broker promotion.
- Broker promotion revisits journaling as part of the privileged transaction log design.
