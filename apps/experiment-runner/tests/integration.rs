//! Integration tests for the deterministic experiment engine.
//!
//! These exercise the trial runner and journal-only replay end to end against
//! the mock provider through the broker: a promoted experiment that runs the
//! full lifecycle, a rejected experiment that is never applied and leaves the
//! baseline untouched, and a replay from the durable journal alone - reopened
//! as a fresh handle - that reproduces the recorded verdict exactly. The final
//! test states the MVP acceptance criterion directly.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{
    ChangeRequest, Decision, DecisionBounds, ExperimentSpec, VerdictReason,
};
use fpsmaxxing_control_plane::ControlPlane;
use fpsmaxxing_experiment_runner::{evaluate, replay_trial, run_trial};
use fpsmaxxing_mock_provider::MockProvider;
use serde_json::json;
use tempfile::NamedTempFile;

/// Builds a spec that drives the mock knob to `candidate` under the given
/// improvement threshold and temperature ceiling.
fn spec_for(candidate: u64, min_fps_improvement: f64, max_temperature_c: f64) -> ExperimentSpec {
    ExperimentSpec {
        hypothesis: format!("raise mock.value to {candidate}"),
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
            min_fps_improvement,
            max_temperature_c,
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

#[test]
fn a_promoted_experiment_runs_the_full_lifecycle() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");

    assert_eq!(trial.record.verdict.decision, Decision::Promote);
    assert_eq!(trial.record.verdict.reason, VerdictReason::Promoted);

    let lifecycle = trial
        .record
        .lifecycle
        .expect("a promoted trial records a lifecycle");
    assert_eq!(lifecycle.provider_id, "mock");
    assert!(
        lifecycle.verified,
        "candidate value must verify after apply"
    );
    assert!(lifecycle.rolled_back, "leased change must be rolled back");

    // The leased lifecycle restores the pre-state, so the provider is left at
    // its baseline value even after a promotion.
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_rejected_experiment_is_never_applied_and_leaves_the_baseline() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // Candidate 70 would raise FPS but drives modeled temperature to 85 C,
    // above the 80 C ceiling, so the evaluator rejects it on safety.
    let trial = run_trial(&mut plane, &spec_for(70, 5.0, 80.0)).expect("run trial");

    assert_eq!(trial.record.verdict.decision, Decision::Reject);
    assert_eq!(
        trial.record.verdict.reason,
        VerdictReason::TemperatureExceeded
    );
    assert!(
        trial.record.lifecycle.is_none(),
        "a rejected candidate is never applied"
    );

    // Nothing was applied, so the provider still holds the baseline value.
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_trial_replays_from_the_journal_alone_with_an_identical_verdict() {
    let journal = NamedTempFile::new().expect("temp journal");

    let trial_id = {
        let mut plane =
            ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
        run_trial(&mut plane, &spec_for(40, 5.0, 80.0))
            .expect("run trial")
            .id
    };

    // Reopen the journal as a brand new handle - no in-memory trial state, no
    // chat history - and with a provider at an unrelated value to prove replay
    // reads only what was persisted.
    let replayed =
        ControlPlane::open(Box::new(MockProvider::new(0)), journal.path()).expect("reopen");
    let outcome = replay_trial(&replayed, trial_id).expect("replay trial");

    assert!(
        outcome.is_consistent(),
        "recomputed verdict must match the journal"
    );
    assert_eq!(outcome.recorded, outcome.recomputed);
    assert_eq!(outcome.recomputed.decision, Decision::Promote);
}

#[test]
fn mvp_one_measured_experiment_is_promoted_or_rejected_by_the_evaluator() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");

    // The acceptance criterion: a measured experiment reaches a promote/reject
    // decision, and that decision is the immutable evaluator's - recomputing it
    // from the recorded samples and bounds reproduces the journaled verdict.
    assert!(matches!(
        trial.record.verdict.decision,
        Decision::Promote | Decision::Reject
    ));
    let recomputed = evaluate(
        &trial.record.baseline_samples,
        &trial.record.candidate_samples,
        &trial.record.spec.bounds,
    );
    assert_eq!(recomputed, trial.record.verdict);
}
