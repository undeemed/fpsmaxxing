//! Integration tests for the deterministic experiment engine.
//!
//! These exercise the trial runner and journal-only replay end to end against
//! the mock provider through the broker: a promoted experiment that runs the
//! full lifecycle, a rejected experiment that is never applied and leaves the
//! baseline untouched, a promoted experiment whose lifecycle the broker refuses
//! but which is still journaled, and a replay from the durable journal alone -
//! reopened as a fresh handle - that reproduces the recorded verdict exactly.
//! The final test states the MVP acceptance criterion directly.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{
    ChangeRequest, Decision, DecisionBounds, ExperimentSpec, VerdictReason,
};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError};
use fpsmaxxing_experiment_runner::{RunnerError, TrialRecord, evaluate, replay_trial, run_trial};
use fpsmaxxing_mock_provider::MockProvider;
use serde_json::json;
use tempfile::NamedTempFile;

/// Builds a spec that drives the mock knob to `candidate` under the given
/// improvement threshold and temperature ceiling.
fn spec_for(candidate: u64, min_fps_improvement: f64, max_temperature_c: f64) -> ExperimentSpec {
    spec_with_lease(candidate, min_fps_improvement, max_temperature_c, 30)
}

/// Builds the same spec with an explicit lease, so a test can drive the broker
/// policy into denying the change the evaluator promoted.
fn spec_with_lease(
    candidate: u64,
    min_fps_improvement: f64,
    max_temperature_c: f64,
    lease_seconds: u64,
) -> ExperimentSpec {
    ExperimentSpec {
        hypothesis: format!("raise mock.value to {candidate}"),
        target: ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": candidate }),
            lease_seconds: NonZeroU64::new(lease_seconds).expect("lease is non-zero"),
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
        .clone()
        .expect("a promoted trial records a lifecycle");
    assert_eq!(lifecycle.provider_id, "mock");
    assert!(
        lifecycle.verified,
        "candidate value must verify after apply"
    );
    assert!(lifecycle.rolled_back, "leased change must be rolled back");
    assert!(
        trial.record.lifecycle_error.is_none(),
        "a completed lifecycle records no failure"
    );

    // The trial journal is append-only: the outcome is carried by the single
    // row the trial inserted, which is exactly what replay reads back.
    assert_eq!(plane.trial_ids().expect("trial ids"), [trial.id]);
    let journaled: TrialRecord =
        serde_json::from_value(plane.read_trial(trial.id).expect("trial should read"))
            .expect("trial should decode");
    assert_eq!(journaled, trial.record);
    assert!(
        replay_trial(&plane, trial.id)
            .expect("replay trial")
            .is_consistent()
    );

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
fn a_promoted_trial_survives_a_lifecycle_the_broker_refuses() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // The evaluator promotes on the measurements, but the 400 second lease is
    // outside the broker's policy envelope, so the lifecycle never runs.
    let spec = spec_with_lease(40, 5.0, 80.0, 400);
    let error = run_trial(&mut plane, &spec).expect_err("policy should deny the lease");
    assert!(matches!(
        error,
        RunnerError::ControlPlane(ControlPlaneError::PolicyDenied(_))
    ));

    // The measurements that authorized the promotion are journaled with the
    // refusal, in one append, so the trial is still discoverable and replayable.
    let ids = plane.trial_ids().expect("trial ids");
    assert_eq!(
        ids.len(),
        1,
        "the failed promotion is journaled exactly once"
    );
    let record: TrialRecord =
        serde_json::from_value(plane.read_trial(ids[0]).expect("trial should read"))
            .expect("trial should decode");
    assert_eq!(record.verdict.decision, Decision::Promote);
    assert!(
        record.lifecycle.is_none(),
        "no lifecycle completed for a denied change"
    );
    let failure = record
        .lifecycle_error
        .expect("the refused lifecycle is recorded on the trial");
    assert_eq!(failure.kind, "policy-denied");
    assert!(failure.error.contains("lease exceeds 300 seconds"));

    let outcome = replay_trial(&plane, ids[0]).expect("replay trial");
    assert!(outcome.is_consistent());
    assert_eq!(outcome.recomputed.decision, Decision::Promote);

    // Nothing reached the provider, so the baseline is untouched.
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
fn an_out_of_envelope_spec_is_refused_before_any_measurement() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    let mut spec = spec_for(40, 5.0, 80.0);
    spec.warmup_samples = u32::MAX;
    let error = run_trial(&mut plane, &spec).expect_err("unbounded counts should be refused");
    assert!(matches!(error, RunnerError::InvalidSpec(_)));

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused spec journals nothing"
    );
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_candidate_outside_the_policy_bound_is_refused_before_any_measurement() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // The broker's policy bound is only checked inside the lifecycle, which a
    // rejected trial never reaches; measuring such a candidate first would
    // aggregate errors past `u64::MAX`.
    let error = run_trial(&mut plane, &spec_for(u64::MAX, 5.0, 80.0))
        .expect_err("an out-of-policy candidate should be refused");
    assert!(matches!(error, RunnerError::InvalidSpec(_)));

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused spec journals nothing"
    );
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_trial_record_from_an_unsupported_version_fails_closed() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");

    // Stand in for a record a future runner appended: the reader must refuse it
    // rather than decode it under this version's field meanings.
    let mut payload = plane.read_trial(trial.id).expect("trial should read");
    payload["schema_version"] = json!(2);
    let future = plane
        .record_trial(&payload)
        .expect("a future record should append");
    let error = replay_trial(&plane, future).expect_err("a future record should be refused");
    assert!(matches!(error, RunnerError::UnsupportedRecordVersion(2)));

    // A future record that also reshaped a field must still report the version
    // that wrote it, not an opaque decode error about the reshaped field.
    payload["verdict"] = json!("promote");
    let reshaped = plane
        .record_trial(&payload)
        .expect("a future record should append");
    let error = replay_trial(&plane, reshaped).expect_err("a future record should be refused");
    assert!(
        matches!(error, RunnerError::UnsupportedRecordVersion(2)),
        "expected a version error, got {error:?}"
    );

    // Appending those rows left the original trial exactly as it was recorded.
    assert!(
        replay_trial(&plane, trial.id)
            .expect("replay trial")
            .is_consistent()
    );
}

#[test]
fn a_trial_record_carrying_unknown_fields_fails_closed() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");

    // A field this build does not know about, under a version it does, means a
    // divergent writer or a rewritten row. Dropping it silently would let the
    // replay call the record consistent, so decoding refuses it instead.
    let mut payload = plane.read_trial(trial.id).expect("trial should read");
    payload["unexpected"] = json!(true);
    let tampered = plane
        .record_trial(&payload)
        .expect("the extended record should append");
    let error = replay_trial(&plane, tampered).expect_err("an unknown field should be refused");
    assert!(matches!(error, RunnerError::Decode(_)), "{error:?}");

    let mut payload = plane.read_trial(trial.id).expect("trial should read");
    payload["verdict"]["unexpected"] = json!(true);
    let tampered = plane
        .record_trial(&payload)
        .expect("the extended record should append");
    let error = replay_trial(&plane, tampered).expect_err("an unknown field should be refused");
    assert!(matches!(error, RunnerError::Decode(_)), "{error:?}");
}

#[test]
fn a_replay_reports_bounds_outside_the_policy_envelope() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");

    // Stand in for a row whose thresholds were widened after the fact. Both the
    // recorded and the recomputed verdict promote under those widened bounds,
    // so comparing verdicts alone cannot catch it - the replay re-checks the
    // journaled bounds against the policy envelope instead.
    let mut payload = plane.read_trial(trial.id).expect("trial should read");
    payload["spec"]["bounds"]["max_temperature_c"] = json!(200.0);
    let widened = plane
        .record_trial(&payload)
        .expect("a widened record should append");

    let outcome = replay_trial(&plane, widened).expect("replay trial");
    assert!(
        outcome.is_consistent(),
        "the widened bounds still reproduce the recorded verdict"
    );
    assert!(
        !outcome.policy_legal,
        "a temperature ceiling of 200 C is outside the policy envelope"
    );

    // The trial as it was actually run stays legal.
    let outcome = replay_trial(&plane, trial.id).expect("replay trial");
    assert!(outcome.is_consistent() && outcome.policy_legal);
}

#[test]
fn replaying_a_trial_that_was_never_recorded_fails_closed() {
    let journal = NamedTempFile::new().expect("temp journal");
    let plane = ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // An absent row is a consistency signal about recorded history, so it is
    // reported apart from a journal that could not be read at all.
    let error = replay_trial(&plane, 404).expect_err("an unrecorded trial cannot be replayed");
    assert!(
        matches!(
            error,
            RunnerError::ControlPlane(ControlPlaneError::UnknownTrial(404))
        ),
        "{error:?}"
    );
}

#[test]
fn a_trial_targeting_an_unadvertised_capability_is_refused() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // The measurement model only describes the mock knob. Without this gate the
    // trial would be measured, evaluated, and journaled as authoritative before
    // the broker refused the same capability inside the lifecycle.
    let mut spec = spec_for(40, 5.0, 80.0);
    spec.target.capability_id = "gpu.core-clock-offset".to_owned();
    let error = run_trial(&mut plane, &spec).expect_err("unknown hardware should fail closed");
    assert!(
        matches!(
            error,
            RunnerError::ControlPlane(ControlPlaneError::UnknownCapability(_))
        ),
        "{error:?}"
    );

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused spec journals nothing"
    );
    assert_eq!(current_value(&plane), 10);
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
