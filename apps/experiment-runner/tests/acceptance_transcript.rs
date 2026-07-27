//! The MVP acceptance criterion as one auditable transcript.
//!
//! `tests/integration.rs` asserts each behaviour of the experiment engine in
//! isolation. This test walks the whole story once, in the order an operator
//! experiences it, over a single durable journal file, and prints what it
//! observes at each step so the run is readable as evidence rather than only as
//! a pass: a measured experiment is promoted through the broker lifecycle, a
//! second measured experiment is rejected and never reaches the provider, the
//! rows both trials left behind are shown as they are stored in `SQLite`, and a
//! fresh handle on that file alone re-evaluates both trials to the identical
//! verdicts without the specs, the provider state, or any chat history that
//! produced them.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{ChangeRequest, Decision, DecisionBounds, ExperimentSpec, VerdictReason};
use fpsmaxxing_control_plane::ControlPlane;
use fpsmaxxing_experiment_runner::{StoredTrial, TrialRecord, replay_trial, run_trial};
use fpsmaxxing_mock_provider::MockProvider;
use rusqlite::Connection;
use serde_json::json;
use tempfile::NamedTempFile;

/// The knob value the provider is parked at before either trial runs.
const BASELINE_VALUE: u64 = 10;

/// Builds a spec driving the mock knob to `candidate` under a fixed envelope.
fn spec_for(candidate: u64) -> ExperimentSpec {
    ExperimentSpec {
        hypothesis: format!(
            "raising mock.value from {BASELINE_VALUE} to {candidate} improves FPS within thermal and power limits"
        ),
        target: ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": candidate }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
        },
        warmup_samples: 2,
        baseline_samples: NonZeroU32::new(5).expect("baseline count is non-zero"),
        candidate_samples: NonZeroU32::new(5).expect("candidate count is non-zero"),
        bounds: DecisionBounds {
            min_samples: NonZeroU32::new(3).expect("min samples is non-zero"),
            min_fps_improvement: 5.0,
            max_temperature_c: 80.0,
            max_power_w: 200.0,
            max_errors: 0,
        },
    }
}

/// Reads the mock provider's current knob value through a broker snapshot.
fn current_value(plane: &ControlPlane) -> u64 {
    plane
        .snapshot()
        .expect("snapshot")
        .state
        .get("value")
        .and_then(serde_json::Value::as_u64)
        .expect("mock value")
}

/// Prints the measurements and verdict a trial recorded.
fn report(label: &str, trial: &StoredTrial) {
    let verdict = &trial.record.verdict;
    println!("[{label}] trial id {}", trial.id);
    println!("  hypothesis      : {}", trial.record.spec.hypothesis);
    println!(
        "  baseline value  : {} -> mean {:.1} fps, {:.1} C, {:.1} W, {} errors over {} samples",
        trial.record.baseline_value,
        verdict.baseline.mean_fps,
        verdict.baseline.max_temperature_c,
        verdict.baseline.max_power_w,
        verdict.baseline.total_errors,
        verdict.baseline.samples
    );
    println!(
        "  candidate value : {} -> mean {:.1} fps, {:.1} C, {:.1} W, {} errors over {} samples",
        trial.record.candidate_value,
        verdict.candidate.mean_fps,
        verdict.candidate.max_temperature_c,
        verdict.candidate.max_power_w,
        verdict.candidate.total_errors,
        verdict.candidate.samples
    );
    println!(
        "  bounds          : >= {:.1} fps gain, <= {:.1} C, <= {:.1} W, <= {} errors, >= {} samples",
        trial.record.spec.bounds.min_fps_improvement,
        trial.record.spec.bounds.max_temperature_c,
        trial.record.spec.bounds.max_power_w,
        trial.record.spec.bounds.max_errors,
        trial.record.spec.bounds.min_samples
    );
    println!(
        "  verdict         : {:?} ({:?}), fps_improvement = {:+.1}",
        verdict.decision, verdict.reason, verdict.fps_improvement
    );
    match (&trial.record.lifecycle, &trial.record.lifecycle_error) {
        (Some(lifecycle), _) => println!(
            "  lifecycle       : provider {}, preview {:?}, verified = {}, rolled_back = {}",
            lifecycle.provider_id, lifecycle.preview, lifecycle.verified, lifecycle.rolled_back
        ),
        (None, Some(failure)) => println!(
            "  lifecycle       : refused ({}): {}",
            failure.kind, failure.error
        ),
        (None, None) => println!("  lifecycle       : none - the candidate was never applied"),
    }
}

#[test]
fn one_measured_experiment_is_promoted_and_another_rejected_then_both_replay() {
    let journal = NamedTempFile::new().expect("temp journal");
    println!("journal file: an on-disk SQLite database\n");

    let promoted;
    let rejected;
    {
        let mut plane =
            ControlPlane::open(Box::new(MockProvider::new(BASELINE_VALUE)), journal.path())
                .expect("open");
        println!("provider parked at mock.value = {}\n", current_value(&plane));

        // A measured experiment the evaluator promotes: the candidate gains
        // 30 fps and stays inside every safety ceiling, so the broker runs the
        // full snapshot/preview/apply/verify/rollback lifecycle.
        promoted = run_trial(&mut plane, &spec_for(40)).expect("run promoted trial");
        report("promoted", &promoted);
        assert_eq!(promoted.record.verdict.decision, Decision::Promote);
        assert_eq!(promoted.record.verdict.reason, VerdictReason::Promoted);
        let lifecycle = promoted
            .record
            .lifecycle
            .clone()
            .expect("a promotion records its lifecycle");
        assert!(lifecycle.verified && lifecycle.rolled_back);
        println!(
            "  provider after  : mock.value = {} (the lease restored the pre-state)\n",
            current_value(&plane)
        );
        assert_eq!(current_value(&plane), BASELINE_VALUE);

        // A measured experiment the evaluator rejects: the candidate gains even
        // more fps but drives modeled temperature to 85 C, past the ceiling, so
        // it is never applied and the baseline is left exactly as it was.
        rejected = run_trial(&mut plane, &spec_for(70)).expect("run rejected trial");
        report("rejected", &rejected);
        assert_eq!(rejected.record.verdict.decision, Decision::Reject);
        assert_eq!(
            rejected.record.verdict.reason,
            VerdictReason::TemperatureExceeded
        );
        assert!(rejected.record.lifecycle.is_none());
        assert!(rejected.record.lifecycle_error.is_none());
        println!(
            "  provider after  : mock.value = {} (untouched - nothing was applied)\n",
            current_value(&plane)
        );
        assert_eq!(current_value(&plane), BASELINE_VALUE);
    }

    // The rows as the journal actually holds them, read with a plain SQLite
    // connection rather than through the control plane, so the durable state is
    // shown rather than described.
    let raw = Connection::open(journal.path()).expect("open journal directly");
    let mut statement = raw
        .prepare("SELECT id, recorded_at, length(payload) FROM experiment_trials ORDER BY id")
        .expect("query trials");
    let rows: Vec<(i64, String, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("read trials")
        .collect::<Result<_, _>>()
        .expect("read trials");
    println!("persisted experiment_trials rows:");
    for (id, recorded_at, payload_bytes) in &rows {
        println!("  id {id}, recorded_at {recorded_at}, payload {payload_bytes} bytes");
    }
    assert_eq!(
        rows.iter().map(|(id, ..)| *id).collect::<Vec<_>>(),
        [promoted.id, rejected.id]
    );

    // The stored payload of the promotion, verbatim. Everything re-evaluation
    // needs is in this one row: the spec that was run, both measurement sets,
    // and the verdict, under a record version a reader can refuse.
    let stored_payload: String = raw
        .query_row(
            "SELECT payload FROM experiment_trials WHERE id = ?1",
            [promoted.id],
            |row| row.get(0),
        )
        .expect("read the promoted payload");
    println!("\nstored payload of trial {}:\n  {stored_payload}", promoted.id);

    // Reopen the file as a brand new handle, with a provider parked at an
    // unrelated value, and re-evaluate both trials from the journal alone.
    let archive =
        ControlPlane::open(Box::new(MockProvider::new(0)), journal.path()).expect("reopen");
    println!(
        "\nreplay from a fresh handle (provider now at mock.value = {}, no spec in hand, no chat history):",
        current_value(&archive)
    );
    for (label, trial) in [("promoted", &promoted), ("rejected", &rejected)] {
        let stored: TrialRecord =
            serde_json::from_value(archive.read_trial(trial.id).expect("read trial"))
                .expect("decode trial");
        assert_eq!(
            stored, trial.record,
            "the journaled record must survive the round trip verbatim"
        );

        let outcome = replay_trial(&archive, trial.id).expect("replay trial");
        println!(
            "  [{label}] trial {}: recorded {:?} ({:?}) -> recomputed {:?} ({:?}); identical = {}, policy legal = {}",
            outcome.trial_id,
            outcome.recorded.decision,
            outcome.recorded.reason,
            outcome.recomputed.decision,
            outcome.recomputed.reason,
            outcome.is_consistent(),
            outcome.policy_legal
        );
        assert!(
            outcome.is_consistent(),
            "replay must reproduce the recorded verdict exactly"
        );
        assert!(outcome.policy_legal, "{:?}", outcome.policy_reason);
        assert_eq!(outcome.recomputed, trial.record.verdict);
    }

    // A clean replay is a real check rather than a rubber stamp: append a copy
    // of the promotion whose temperature ceiling was widened after the fact.
    // It re-evaluates to the very verdict it carries, so only the policy gate
    // replay re-applies can catch it.
    let mut widened = archive.read_trial(promoted.id).expect("read trial");
    widened["spec"]["bounds"]["max_temperature_c"] = json!(200.0);
    let widened_id = archive
        .record_trial(&widened)
        .expect("append the widened row");
    let outcome = replay_trial(&archive, widened_id).expect("replay the widened row");
    println!(
        "  [tampered] trial {}: bounds widened to 200 C; recomputed verdict still identical = {}, but policy legal = {} ({})",
        outcome.trial_id,
        outcome.is_consistent(),
        outcome.policy_legal,
        outcome.policy_reason.as_deref().unwrap_or("no reason given")
    );
    assert!(outcome.is_consistent());
    assert!(!outcome.policy_legal);

    println!(
        "\nacceptance criterion: {} measured experiments reached a decision through the immutable evaluator - {:?} and {:?} - and both re-evaluate identically from the journal alone",
        rows.len(),
        promoted.record.verdict.decision,
        rejected.record.verdict.decision
    );
}
