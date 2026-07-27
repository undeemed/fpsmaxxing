//! Integration tests for the deterministic experiment engine.
//!
//! These exercise the trial runner and journal-only replay end to end against
//! the mock provider through the broker: a promoted experiment that runs the
//! full lifecycle, a rejected experiment that is never applied and leaves the
//! baseline untouched, a promoted experiment whose lifecycle the broker refuses
//! but which is still journaled, the same refusal when the journal cannot take
//! the record either, a completed promotion whose record the journal refuses,
//! and a replay from the durable journal alone - reopened as a fresh handle -
//! that reproduces the recorded verdict exactly.
//! The final test states the MVP acceptance criterion directly.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{
    CapabilityDescriptor, ChangeRequest, Decision, DecisionBounds, ExperimentSpec,
    MAX_HYPOTHESIS_CHARS, MAX_LEASE_SECONDS, Persistence, ProviderManifest, RiskClass,
    StateSnapshot, VerdictReason,
};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError, MAX_MOCK_VALUE};
use fpsmaxxing_experiment_runner::{RunnerError, TrialRecord, evaluate, replay_trial, run_trial};
use fpsmaxxing_mock_provider::MockProvider;
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use rusqlite::Connection;
use serde_json::json;
use tempfile::NamedTempFile;

/// Builds a spec that drives the mock knob to `candidate` under the given
/// improvement threshold and temperature ceiling.
fn spec_for(candidate: u64, min_fps_improvement: f64, max_temperature_c: f64) -> ExperimentSpec {
    spec_with_lease(candidate, min_fps_improvement, max_temperature_c, 30)
}

/// Builds the same spec with an explicit TTL lease.
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

/// A provider that advertises a knob no journaled trial ever targeted, standing
/// in for auditing an archived journal on a machine whose hardware has changed.
struct ForeignProvider;

impl Provider for ForeignProvider {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: "foreign".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec![std::env::consts::OS.to_owned()],
            capabilities: vec![CapabilityDescriptor {
                id: "foreign.knob".to_owned(),
                description: "A knob the measurement model never described".to_owned(),
                risk: RiskClass::Reversible,
                persistence: Persistence::Leased,
                input_schema: json!({ "type": "object" }),
            }],
        }
    }

    fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
        Ok(StateSnapshot {
            provider_id: "foreign".to_owned(),
            state: json!({ "knob": 0 }),
        })
    }

    fn preview(&self, _request: &ChangeRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "foreign.knob".to_owned(),
        ))
    }

    fn apply(&mut self, _request: &ChangeRequest) -> Result<(), ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "foreign.knob".to_owned(),
        ))
    }

    fn verify(&self, _request: &ChangeRequest) -> Result<bool, ProviderError> {
        Ok(false)
    }

    fn rollback(&mut self, _snapshot: &StateSnapshot) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// A provider advertising the modeled knob under a risk class the broker's
/// policy refuses, standing in for a lifecycle denied after the evaluator has
/// already promoted on the measurements.
struct ApprovalGatedProvider(MockProvider);

impl Provider for ApprovalGatedProvider {
    fn manifest(&self) -> ProviderManifest {
        let mut manifest = self.0.manifest();
        for capability in &mut manifest.capabilities {
            capability.risk = RiskClass::ApprovalRequired;
        }
        manifest
    }

    fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
        self.0.snapshot()
    }

    fn preview(&self, request: &ChangeRequest) -> Result<String, ProviderError> {
        self.0.preview(request)
    }

    fn apply(&mut self, request: &ChangeRequest) -> Result<(), ProviderError> {
        self.0.apply(request)
    }

    fn verify(&self, request: &ChangeRequest) -> Result<bool, ProviderError> {
        self.0.verify(request)
    }

    fn rollback(&mut self, snapshot: &StateSnapshot) -> Result<(), ProviderError> {
        self.0.rollback(snapshot)
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
    let mut plane = ControlPlane::open(
        Box::new(ApprovalGatedProvider(MockProvider::new(10))),
        journal.path(),
    )
    .expect("open");

    // The evaluator promotes on the measurements, but this provider advertises
    // the knob under a risk class the broker's policy refuses, so the lifecycle
    // never runs.
    let spec = spec_for(40, 5.0, 80.0);
    let error = run_trial(&mut plane, &spec).expect_err("policy should deny the risk class");
    let reported = error.to_string();

    // The measurements that authorized the promotion are journaled with the
    // refusal, in one append, and the error names the row it was written to, so
    // the caller addresses its own trial instead of the journal's last one.
    let RunnerError::LifecycleFailed {
        trial_id,
        journal_error,
        source,
    } = error
    else {
        panic!("a refused lifecycle reports the trial it journaled, got {reported}");
    };
    assert!(
        matches!(source, ControlPlaneError::PolicyDenied(_)),
        "{source:?}"
    );
    assert!(
        journal_error.is_none(),
        "the record was stored, so nothing explains a loss: {journal_error:?}"
    );
    let trial_id = trial_id.expect("the refused lifecycle journaled its trial");
    assert!(
        reported.contains(&format!("trial {trial_id}")),
        "the reported error names the journaled row: {reported}"
    );
    assert_eq!(
        plane.trial_ids().expect("trial ids"),
        [trial_id],
        "the failed promotion is journaled exactly once"
    );
    let record: TrialRecord =
        serde_json::from_value(plane.read_trial(trial_id).expect("trial should read"))
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
    assert!(
        failure
            .error
            .contains("only reversible mock capabilities are enabled"),
        "{}",
        failure.error
    );

    let outcome = replay_trial(&plane, trial_id).expect("replay trial");
    assert!(outcome.is_consistent());
    assert_eq!(outcome.recomputed.decision, Decision::Promote);

    // Nothing reached the provider, so the baseline is untouched.
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_refused_lifecycle_reports_why_its_trial_could_not_be_journaled() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane = ControlPlane::open(
        Box::new(ApprovalGatedProvider(MockProvider::new(10))),
        journal.path(),
    )
    .expect("open");

    // Take the trial table away behind the broker's back, so the append that
    // records the refused promotion fails too. Losing the measurements that
    // authorized a mutation is the graver of the two failures, so the reason
    // travels with the error rather than being reported out of band.
    Connection::open(journal.path())
        .expect("second journal handle")
        .execute_batch("DROP TABLE experiment_trials")
        .expect("the trial table should drop");

    let error =
        run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect_err("policy should deny the change");
    let reported = error.to_string();
    let RunnerError::LifecycleFailed {
        trial_id,
        journal_error,
        source,
    } = error
    else {
        panic!("a refused lifecycle reports how its trial was journaled, got {reported}");
    };

    // The lifecycle error stays the primary one.
    assert!(
        matches!(source, ControlPlaneError::PolicyDenied(_)),
        "{source:?}"
    );
    assert!(
        trial_id.is_none(),
        "no identifier exists for a record that was never stored"
    );
    let journal_error = journal_error.expect("a lost record reports why it was lost");
    assert!(
        matches!(*journal_error, ControlPlaneError::Journal(_)),
        "a storage fault is distinguishable from a serialization one: {journal_error:?}"
    );
    assert!(
        reported.contains("failing to journal its trial") && reported.contains("experiment_trials"),
        "the reported error names both failures: {reported}"
    );

    // Nothing reached the provider, so the baseline is untouched.
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_lost_record_reports_the_lifecycle_the_promotion_had_already_run() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // Take the trial table away behind the broker's back, so the promotion runs
    // its whole lifecycle and only the record of it is lost. The trial row is
    // the only auditable statement that a promotion reached the provider, and
    // the lifecycle journal cannot stand in for it - its terminal `completed`
    // record leaves no dangling apply intent for `doctor` to report - so the
    // outcome travels with the error.
    Connection::open(journal.path())
        .expect("second journal handle")
        .execute_batch("DROP TABLE experiment_trials")
        .expect("the trial table should drop");

    let error =
        run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect_err("the record cannot be stored");
    let reported = error.to_string();
    let RunnerError::TrialNotJournaled { lifecycle, source } = error else {
        panic!("a lost record reports the lifecycle it would have carried, got {reported}");
    };
    assert!(
        matches!(source, ControlPlaneError::Journal(_)),
        "{source:?}"
    );
    let lifecycle = lifecycle.expect("a completed promotion reports its lifecycle");
    assert_eq!(lifecycle.provider_id, "mock");
    assert!(lifecycle.verified && lifecycle.rolled_back);
    assert!(
        reported.contains("completed the lifecycle"),
        "the reported error says the provider was reached: {reported}"
    );

    // A rejection never runs a lifecycle, so its lost record carries none - the
    // distinction the caller could not otherwise draw.
    let error =
        run_trial(&mut plane, &spec_for(70, 5.0, 80.0)).expect_err("the record cannot be stored");
    let reported = error.to_string();
    let RunnerError::TrialNotJournaled { lifecycle, .. } = error else {
        panic!("a lost record is reported as such, got {reported}");
    };
    assert!(
        lifecycle.is_none(),
        "a rejected candidate never reached the provider: {lifecycle:?}"
    );

    // The leased lifecycle still restored the pre-state on the way out.
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
fn target_parameters_the_model_does_not_take_are_refused_before_any_measurement() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // The measurement path reads only the knob value out of the target's
    // free-form parameter object. An unread key would be written verbatim into
    // both the trial row and the lifecycle journal's write-ahead apply intent,
    // so the spec author would choose how large those durable rows are.
    let mut spec = spec_for(40, 5.0, 80.0);
    spec.target.parameters = json!({ "value": 40, "pad": "x".repeat(4096) });
    let error = run_trial(&mut plane, &spec).expect_err("an unread parameter should be refused");
    let RunnerError::InvalidSpec(message) = &error else {
        panic!("an unbounded parameter object is refused as an invalid spec, got {error:?}");
    };
    assert!(message.contains("pad"), "{message}");

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused spec journals nothing"
    );
    assert_eq!(current_value(&plane), 10);
}

#[test]
fn a_lease_outside_the_policy_bound_is_refused_before_any_measurement() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");

    // The lease bounds how long a mutation may persist and the broker only
    // checks it inside the lifecycle, so a spec that can only ever be denied is
    // refused before it is measured and journaled as an authoritative trial.
    let error = run_trial(
        &mut plane,
        &spec_with_lease(40, 5.0, 80.0, MAX_LEASE_SECONDS + 1),
    )
    .expect_err("an out-of-policy lease should be refused");
    assert!(matches!(error, RunnerError::InvalidSpec(_)), "{error:?}");

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused spec journals nothing"
    );
    assert_eq!(current_value(&plane), 10);

    // A lease exactly at the ceiling is inside the envelope and still runs.
    let trial = run_trial(
        &mut plane,
        &spec_with_lease(40, 5.0, 80.0, MAX_LEASE_SECONDS),
    )
    .expect("run trial");
    assert_eq!(trial.record.verdict.decision, Decision::Promote);
}

#[test]
fn a_trial_record_without_a_readable_version_fails_closed() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");
    let recorded = plane.read_trial(trial.id).expect("trial should read");

    // A row stating no version this build can read was not written by an older
    // runner, so it is reported apart from a version merely unsupported here.
    for rewrite in [json!(null), json!("one"), json!(u64::from(u32::MAX) + 1)] {
        let mut payload = recorded.clone();
        payload["schema_version"] = rewrite.clone();
        let malformed = plane
            .record_trial(&payload)
            .expect("a malformed record should append");
        let error =
            replay_trial(&plane, malformed).expect_err("an unreadable version should be refused");
        assert!(
            matches!(error, RunnerError::MalformedRecordVersion),
            "{rewrite}: {error:?}"
        );
    }

    // A record that simply omits the field is the same signal.
    let mut payload = recorded;
    payload
        .as_object_mut()
        .expect("a trial record is an object")
        .remove("schema_version");
    let malformed = plane
        .record_trial(&payload)
        .expect("a malformed record should append");
    let error = replay_trial(&plane, malformed).expect_err("an absent version should be refused");
    assert!(
        matches!(error, RunnerError::MalformedRecordVersion),
        "{error:?}"
    );
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

    // The auditor is told which gate tripped, not merely that one did.
    let reason = outcome
        .policy_reason
        .expect("an illegal row carries a reason");
    assert!(reason.contains("max_temperature_c"), "{reason}");

    // The trial as it was actually run stays legal.
    let outcome = replay_trial(&plane, trial.id).expect("replay trial");
    assert!(outcome.is_consistent() && outcome.policy_legal);
    assert!(outcome.policy_reason.is_none());
}

#[test]
fn a_replay_reports_a_record_the_policy_gate_would_refuse() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let trial = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");
    let recorded = plane.read_trial(trial.id).expect("trial should read");

    // Each of these rows re-evaluates to exactly the verdict it carries, so
    // comparing verdicts cannot catch any of them. Replay applies the policy
    // gate to the journaled spec instead - deliberately without the run-time
    // manifest check - and cross-checks the record against the spec it carries.
    let refused = [
        (
            "a capability the model never described",
            "unknown capability",
            {
                let mut payload = recorded.clone();
                payload["spec"]["target"]["capability_id"] = json!("gpu.core-clock-offset");
                payload
            },
        ),
        (
            "a candidate value above the policy ceiling",
            "target value",
            {
                let mut payload = recorded.clone();
                payload["spec"]["target"]["parameters"]["value"] = json!(MAX_MOCK_VALUE + 1);
                payload["candidate_value"] = json!(MAX_MOCK_VALUE + 1);
                payload
            },
        ),
        ("a lease above the policy ceiling", "lease_seconds", {
            let mut payload = recorded.clone();
            payload["spec"]["target"]["lease_seconds"] = json!(MAX_LEASE_SECONDS + 1);
            payload
        }),
        ("a parameter the model does not take", "pad", {
            let mut payload = recorded.clone();
            payload["spec"]["target"]["parameters"]["pad"] = json!("x".repeat(4096));
            payload
        }),
        ("a hypothesis above the policy ceiling", "hypothesis", {
            let mut payload = recorded.clone();
            payload["spec"]["hypothesis"] = json!("x".repeat(MAX_HYPOTHESIS_CHARS as usize + 1));
            payload
        }),
        // Blanking the statement of what a promotion was for is the cheaper
        // rewrite of the two, so the floor is gated like the ceiling. Only the
        // length is checked, though: a hypothesis rewritten within its bounds
        // has no redundant copy in the record to contradict it.
        ("a blanked hypothesis", "hypothesis", {
            let mut payload = recorded.clone();
            payload["spec"]["hypothesis"] = json!("");
            payload
        }),
        (
            "a candidate value contradicting its own spec",
            "candidate_value",
            {
                let mut payload = recorded.clone();
                payload["candidate_value"] = json!(41);
                payload
            },
        ),
        (
            "a baseline outside the policy envelope",
            "baseline_value",
            {
                let mut payload = recorded.clone();
                payload["baseline_value"] = json!(MAX_MOCK_VALUE + 1);
                payload
            },
        ),
        (
            "fewer samples than the spec declared",
            "candidate_samples",
            {
                let mut payload = recorded.clone();
                payload["spec"]["candidate_samples"] = json!(4);
                payload
            },
        ),
    ];

    for (rewrite, expected_reason, payload) in refused {
        let tampered = plane
            .record_trial(&payload)
            .expect("a rewritten record should append");
        let outcome = replay_trial(&plane, tampered).expect("replay trial");
        assert!(
            outcome.is_consistent(),
            "{rewrite} still reproduces the recorded verdict"
        );
        assert!(!outcome.policy_legal, "{rewrite} must replay as illegal");

        // The reported reason names the gate that tripped, so an auditor is not
        // left to re-derive which field was rewritten.
        let reason = outcome
            .policy_reason
            .unwrap_or_else(|| panic!("{rewrite} must carry a reason"));
        assert!(
            reason.contains(expected_reason),
            "{rewrite}: expected {expected_reason:?} in {reason:?}"
        );
    }

    // The trial as it was actually run stays legal.
    let outcome = replay_trial(&plane, trial.id).expect("replay trial");
    assert!(outcome.is_consistent() && outcome.policy_legal);
    assert!(outcome.policy_reason.is_none());
}

#[test]
fn a_replay_reports_lifecycle_fields_that_contradict_the_verdict() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane =
        ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
    let promoted = run_trial(&mut plane, &spec_for(40, 5.0, 80.0)).expect("run trial");
    let rejected = run_trial(&mut plane, &spec_for(70, 5.0, 80.0)).expect("run trial");
    assert_eq!(rejected.record.verdict.decision, Decision::Reject);

    // A promotion is journaled once its lifecycle has finished, carrying either
    // the outcome or the broker error, so a row with neither claims a promotion
    // whose fate went unrecorded.
    let mut stripped = plane.read_trial(promoted.id).expect("trial should read");
    stripped["lifecycle"] = json!(null);

    // A rejected candidate is never applied, so a lifecycle on that row claims
    // the knob was written when it never was - the trial record is the only
    // auditable statement that it reached the provider.
    let mut fabricated = plane.read_trial(rejected.id).expect("trial should read");
    fabricated["lifecycle"] = json!({
        "provider_id": "mock",
        "preview": "set value to 70",
        "verified": true,
        "rolled_back": true,
    });

    // The broker returns an outcome only once the applied value verified and
    // the captured baseline was restored, so a row claiming a promoted knob was
    // left mutated - or was never verified - is one this runner cannot write.
    let mut left_mutated = plane.read_trial(promoted.id).expect("trial should read");
    left_mutated["lifecycle"]["rolled_back"] = json!(false);

    let mut unverified = plane.read_trial(promoted.id).expect("trial should read");
    unverified["lifecycle"]["verified"] = json!(false);

    for (rewrite, payload) in [
        ("a promotion recording no lifecycle", stripped),
        ("a rejection recording a lifecycle", fabricated),
        ("a promotion left un-rolled-back", left_mutated),
        ("a promotion recording no verification", unverified),
    ] {
        let tampered = plane
            .record_trial(&payload)
            .expect("a rewritten record should append");
        let outcome = replay_trial(&plane, tampered).expect("replay trial");
        assert!(
            outcome.is_consistent(),
            "{rewrite} still reproduces the recorded verdict"
        );
        assert!(!outcome.policy_legal, "{rewrite} must replay as illegal");
        let reason = outcome
            .policy_reason
            .unwrap_or_else(|| panic!("{rewrite} must carry a reason"));
        assert!(reason.contains("lifecycle"), "{rewrite}: {reason}");
    }

    // Both trials as they were actually run stay legal.
    for id in [promoted.id, rejected.id] {
        let outcome = replay_trial(&plane, id).expect("replay trial");
        assert!(outcome.is_consistent() && outcome.policy_legal);
        assert!(outcome.policy_reason.is_none());
    }
}

#[test]
fn a_replay_does_not_depend_on_the_attached_provider() {
    let journal = NamedTempFile::new().expect("temp journal");

    let trial_id = {
        let mut plane =
            ControlPlane::open(Box::new(MockProvider::new(10)), journal.path()).expect("open");
        run_trial(&mut plane, &spec_for(40, 5.0, 80.0))
            .expect("run trial")
            .id
    };

    // The trial journal is a durable file that outlives the process that wrote
    // it. Holding a historical row to what the provider attached now advertises
    // would report every archived trial as tampered with, so replay checks the
    // journaled capability against the one the measurement model describes.
    let archived = ControlPlane::open(Box::new(ForeignProvider), journal.path()).expect("reopen");
    let outcome = replay_trial(&archived, trial_id).expect("replay trial");

    assert!(outcome.is_consistent());
    assert!(
        outcome.policy_legal,
        "an untampered row must stay legal under any provider: {:?}",
        outcome.policy_reason
    );

    // The run-time gate still refuses a trial that provider cannot serve.
    let mut archived = archived;
    let error =
        run_trial(&mut archived, &spec_for(40, 5.0, 80.0)).expect_err("unknown hardware fails");
    assert!(
        matches!(
            error,
            RunnerError::ControlPlane(ControlPlaneError::UnknownCapability(_))
        ),
        "{error:?}"
    );
}

#[test]
fn a_baseline_outside_the_policy_bound_is_refused_before_any_measurement() {
    let journal = NamedTempFile::new().expect("temp journal");
    let mut plane = ControlPlane::open(
        Box::new(MockProvider::new(MAX_MOCK_VALUE + 1)),
        journal.path(),
    )
    .expect("open");

    // The baseline drives the same model as the candidate, so a provider parked
    // outside the envelope fails closed rather than being measured into a
    // journaled trial the policy would never have permitted.
    let error = run_trial(&mut plane, &spec_for(40, 5.0, 80.0))
        .expect_err("an out-of-policy baseline should be refused");
    assert!(
        matches!(error, RunnerError::BaselineOutOfPolicy(value) if value == MAX_MOCK_VALUE + 1),
        "{error:?}"
    );

    assert!(
        plane.trial_ids().expect("trial ids").is_empty(),
        "a refused baseline journals nothing"
    );
    assert_eq!(current_value(&plane), MAX_MOCK_VALUE + 1);
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
