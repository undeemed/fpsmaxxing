# ADR 0002: Alpha experiment journal scope

- Status: accepted
- Date: 2026-07-20

## Decision

The alpha journal keeps a single `experiment_journal` table of stage records.
Each lifecycle allocates its correlation ID atomically with the snapshot record, commits a write-ahead `apply-intent` record holding the full change request before the provider mutates state, and closes with exactly one terminal `completed` or `failed` record carrying a structured error kind and the failing stage.
Full two-phase intent/result journaling for every stage and a dedicated experiments table are deferred until the seam is promoted to the privileged broker.

## Rationale

The write-ahead apply intent makes a crash between mutation and journaling distinguishable from an apply that never started, and the terminal record guarantees that a surviving process never leaves an experiment with only partial stage rows.
A per-stage two-phase protocol and a separate experiments table would duplicate that machinery for the mock-only alpha before the broker owns the transaction log.

## Consequences

- `doctor` reports experiments with an `apply-intent` record but no terminal outcome as dangling.
- Correlation IDs are allocated inside an immediate transaction with a busy timeout, so concurrent gateways sharing one journal file cannot mint duplicate IDs.
- Broker promotion revisits journaling as part of the privileged transaction log design.
