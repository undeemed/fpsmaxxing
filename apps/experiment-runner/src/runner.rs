//! The trial runner and its journal-only replay.
//!
//! [`run_trial`] measures a baseline, measures the candidate, applies the
//! immutable evaluator, and - only when the verdict promotes - runs the
//! candidate through the broker lifecycle. Every trial is written to the
//! durable trial journal as a self-describing [`TrialRecord`], so
//! [`replay_trial`] can re-evaluate it from the journal alone and confirm the
//! recorded verdict without chat history or re-running the workload.
//!
//! # Write-ahead trial records
//!
//! The record is journaled *before* the lifecycle runs, following the same
//! write-ahead principle the lifecycle journal uses (ADR 0002). A lifecycle
//! that fails after a promotion - a policy denial, a provider fault, or a
//! rollback that could not be verified - therefore still leaves a replayable
//! record of the measurements that authorized the apply; the failure is
//! amended onto that record as a [`LifecycleFailure`] and also returned to the
//! caller.
//!
//! # Keep-or-rollback in the safe alpha
//!
//! [`ControlPlane::run_lifecycle`] always restores the pre-state before it
//! returns (every mock capability is leased), so physical persistence cannot be
//! driven by the verdict on this path. The verdict instead gates whether the
//! candidate is applied at all: a [`Promote`](Decision::Promote) exercises the
//! full snapshot/preview/apply/verify/rollback lifecycle, while a
//! [`Reject`](Decision::Reject) never mutates the knob, leaving the baseline
//! untouched. The recorded [`LifecycleOutcome`] captures what happened.

use fpsmaxxing_contracts::{Decision, ExperimentSpec, MAX_SAMPLES, MetricSample, Verdict};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError, LifecycleResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{evaluate, model};

/// Version of the durable [`TrialRecord`] format written to the journal.
///
/// A reader refuses any record it was not built to decode, so adding a field
/// to the record is a version bump rather than a silent misread of history.
pub const TRIAL_RECORD_VERSION: u32 = 1;

/// Fail-closed errors raised while running or replaying a trial.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The broker or the durable journal rejected an operation.
    #[error(transparent)]
    ControlPlane(#[from] ControlPlaneError),
    /// The experiment specification is outside the bounded alpha envelope.
    #[error("experiment spec rejected: {0}")]
    InvalidSpec(String),
    /// The experiment target did not carry an unsigned mock value.
    #[error("experiment target is missing an unsigned mock value")]
    InvalidTarget,
    /// The provider snapshot did not carry an unsigned mock value.
    #[error("provider snapshot is missing an unsigned mock value")]
    InvalidBaseline,
    /// A journaled trial record was written by an unsupported record version.
    #[error("journaled trial uses unsupported record version {0}")]
    UnsupportedRecordVersion(u32),
    /// A journaled trial record could not be decoded for replay.
    #[error(transparent)]
    Decode(#[from] serde_json::Error),
}

/// A durable, `Deserialize`-able mirror of a broker [`LifecycleResult`].
///
/// [`LifecycleResult`] is serialize-only; this record round-trips so a promoted
/// trial's lifecycle outcome can be read back during replay and audit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LifecycleOutcome {
    /// Provider that owned the change.
    pub provider_id: String,
    /// Human-readable preview produced before the write.
    pub preview: String,
    /// Whether the requested value was observed after apply.
    pub verified: bool,
    /// Whether the captured baseline was restored before returning.
    pub rolled_back: bool,
}

impl From<&LifecycleResult> for LifecycleOutcome {
    fn from(result: &LifecycleResult) -> Self {
        Self {
            provider_id: result.provider_id.clone(),
            preview: result.preview.clone(),
            verified: result.verified,
            rolled_back: result.rolled_back,
        }
    }
}

/// The durable record of a lifecycle that failed after the trial was measured.
///
/// It mirrors the `kind` and `error` fields of the lifecycle journal's terminal
/// `failed` record, so a promotion the broker refused is auditable from the
/// trial row alone.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LifecycleFailure {
    /// Stable machine-readable error kind reported by the broker.
    pub kind: String,
    /// Human-readable text of the error the broker returned.
    pub error: String,
}

impl From<&ControlPlaneError> for LifecycleFailure {
    fn from(error: &ControlPlaneError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            error: error.to_string(),
        }
    }
}

/// The complete, replayable record of one trial.
///
/// It holds everything the immutable evaluator needs, so re-evaluation reads
/// only this record: the spec (for the decision bounds), the recorded baseline
/// and candidate samples, and the verdict the evaluator produced.
///
/// On a [`Promote`](Decision::Promote) exactly one of `lifecycle` and
/// `lifecycle_error` is normally set. Both being absent means the amend that
/// follows the lifecycle never reached the journal, and the lifecycle journal's
/// stage records for that experiment are the authoritative account.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TrialRecord {
    /// Version of the record format; see [`TRIAL_RECORD_VERSION`].
    pub schema_version: u32,
    /// The experiment specification that was run.
    pub spec: ExperimentSpec,
    /// The knob value measured as the baseline.
    pub baseline_value: u64,
    /// The knob value measured as the candidate.
    pub candidate_value: u64,
    /// The recorded baseline measurement set.
    pub baseline_samples: Vec<MetricSample>,
    /// The recorded candidate measurement set.
    pub candidate_samples: Vec<MetricSample>,
    /// The verdict the immutable evaluator produced.
    pub verdict: Verdict,
    /// The broker lifecycle outcome, present only when a promoted trial's
    /// lifecycle completed.
    pub lifecycle: Option<LifecycleOutcome>,
    /// Why a promoted trial's lifecycle did not complete, present only when the
    /// broker returned an error.
    pub lifecycle_error: Option<LifecycleFailure>,
}

/// A [`TrialRecord`] paired with the journal identifier it was stored under.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTrial {
    /// The durable trial-journal identifier.
    pub id: i64,
    /// The record that was journaled.
    pub record: TrialRecord,
}

/// The result of re-evaluating a journaled trial.
#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    /// The trial-journal identifier that was replayed.
    pub trial_id: i64,
    /// The verdict read back from the journal.
    pub recorded: Verdict,
    /// The verdict recomputed from the journaled samples and bounds.
    pub recomputed: Verdict,
}

impl ReplayOutcome {
    /// Whether the recomputed verdict matches the one recorded at run time.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.recorded == self.recomputed
    }
}

/// Runs one trial end to end and journals a replayable record.
///
/// Validates the spec, measures the baseline from the provider's current
/// state, measures the candidate from the spec target, evaluates the two, and
/// runs the candidate through the broker lifecycle only on a
/// [`Promote`](Decision::Promote). The record is written to the durable trial
/// journal before the lifecycle runs and amended with its outcome afterwards,
/// so the trial is replayable whether or not the lifecycle succeeds.
///
/// # Errors
///
/// Returns an error if the spec is outside the bounded envelope, if the spec
/// target or provider snapshot lacks an unsigned mock value, or if the broker
/// or durable journal rejects an operation. A lifecycle error is returned only
/// after the trial record has been amended with it.
pub fn run_trial(
    plane: &mut ControlPlane,
    spec: &ExperimentSpec,
) -> Result<StoredTrial, RunnerError> {
    validate(spec)?;
    let baseline_value = baseline_value(plane)?;
    let candidate_value = candidate_value(spec)?;
    let baseline_samples = model::measure(
        baseline_value,
        spec.warmup_samples,
        spec.baseline_samples.get(),
    );
    let candidate_samples = model::measure(
        candidate_value,
        spec.warmup_samples,
        spec.candidate_samples.get(),
    );
    let verdict = evaluate(&baseline_samples, &candidate_samples, &spec.bounds);
    let mut record = TrialRecord {
        schema_version: TRIAL_RECORD_VERSION,
        spec: spec.clone(),
        baseline_value,
        candidate_value,
        baseline_samples,
        candidate_samples,
        verdict,
        lifecycle: None,
        lifecycle_error: None,
    };
    let id = plane.record_trial(&record)?;
    match record.verdict.decision {
        Decision::Promote => match plane.run_lifecycle(&spec.target) {
            Ok(result) => {
                record.lifecycle = Some(LifecycleOutcome::from(&result));
                plane.amend_trial(id, &record)?;
            }
            Err(error) => {
                record.lifecycle_error = Some(LifecycleFailure::from(&error));
                if let Err(journal_error) = plane.amend_trial(id, &record) {
                    eprintln!(
                        "fpsmaxxing-experiment-runner: could not amend trial {id} with its lifecycle failure: {journal_error}"
                    );
                }
                return Err(error.into());
            }
        },
        Decision::Reject => {}
    }
    Ok(StoredTrial { id, record })
}

/// Re-evaluates a journaled trial from the journal alone.
///
/// Reads the [`TrialRecord`], recomputes the verdict from its recorded samples
/// and bounds with the same immutable evaluator, and returns both the recorded
/// and recomputed verdicts for comparison. It consults no chat history and
/// re-runs no workload.
///
/// # Errors
///
/// Returns an error if the trial cannot be read from the durable journal, its
/// record cannot be decoded, or the record was written by an unsupported
/// [`TRIAL_RECORD_VERSION`].
pub fn replay_trial(plane: &ControlPlane, id: i64) -> Result<ReplayOutcome, RunnerError> {
    let record: TrialRecord = serde_json::from_value(plane.read_trial(id)?)?;
    if record.schema_version != TRIAL_RECORD_VERSION {
        return Err(RunnerError::UnsupportedRecordVersion(record.schema_version));
    }
    let recomputed = evaluate(
        &record.baseline_samples,
        &record.candidate_samples,
        &record.spec.bounds,
    );
    Ok(ReplayOutcome {
        trial_id: id,
        recorded: record.verdict,
        recomputed,
    })
}

/// Rejects a spec whose sample counts are unbounded or self-contradictory.
///
/// Sample counts arrive over the wire and size the measurement buffers, so they
/// are checked against [`MAX_SAMPLES`] before any measurement work runs. A spec
/// that asks for fewer counted samples than its own bounds require can never
/// promote, so it is refused up front rather than measured and then rejected.
fn validate(spec: &ExperimentSpec) -> Result<(), RunnerError> {
    let min_samples = spec.bounds.min_samples.get();
    for (label, count) in [
        ("warmup_samples", spec.warmup_samples),
        ("baseline_samples", spec.baseline_samples.get()),
        ("candidate_samples", spec.candidate_samples.get()),
    ] {
        if count > MAX_SAMPLES {
            return Err(RunnerError::InvalidSpec(format!(
                "{label} is {count}, above the {MAX_SAMPLES} ceiling"
            )));
        }
    }
    for (label, count) in [
        ("baseline_samples", spec.baseline_samples.get()),
        ("candidate_samples", spec.candidate_samples.get()),
    ] {
        if count < min_samples {
            return Err(RunnerError::InvalidSpec(format!(
                "{label} is {count}, below the {min_samples} the bounds require"
            )));
        }
    }
    Ok(())
}

/// Reads the baseline knob value from the provider's current state.
fn baseline_value(plane: &ControlPlane) -> Result<u64, RunnerError> {
    plane
        .snapshot()?
        .state
        .get("value")
        .and_then(Value::as_u64)
        .ok_or(RunnerError::InvalidBaseline)
}

/// Reads the candidate knob value from the spec target parameters.
fn candidate_value(spec: &ExperimentSpec) -> Result<u64, RunnerError> {
    spec.target
        .parameters
        .get("value")
        .and_then(Value::as_u64)
        .ok_or(RunnerError::InvalidTarget)
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use fpsmaxxing_contracts::{ChangeRequest, DecisionBounds};
    use serde_json::json;

    use super::{ExperimentSpec, MAX_SAMPLES, RunnerError, validate};

    fn spec(warmup: u32, baseline: u32, candidate: u32, min_samples: u32) -> ExperimentSpec {
        ExperimentSpec {
            hypothesis: "raise mock.value".to_owned(),
            target: ChangeRequest {
                capability_id: "mock.value".to_owned(),
                parameters: json!({ "value": 40 }),
                lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
            },
            warmup_samples: warmup,
            baseline_samples: NonZeroU32::new(baseline).expect("baseline count is non-zero"),
            candidate_samples: NonZeroU32::new(candidate).expect("candidate count is non-zero"),
            bounds: DecisionBounds {
                min_samples: NonZeroU32::new(min_samples).expect("min samples is non-zero"),
                min_fps_improvement: 5.0,
                max_temperature_c: 80.0,
                max_power_w: 200.0,
                max_errors: 0,
            },
        }
    }

    fn rejection(spec: &ExperimentSpec) -> String {
        match validate(spec) {
            Err(RunnerError::InvalidSpec(message)) => message,
            other => panic!("spec should be rejected, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_spec_inside_the_envelope() {
        assert!(validate(&spec(2, 5, 5, 3)).is_ok());
        assert!(validate(&spec(MAX_SAMPLES, MAX_SAMPLES, MAX_SAMPLES, 3)).is_ok());
    }

    #[test]
    fn rejects_sample_counts_above_the_ceiling() {
        // Materializing this many samples would abort the process, so the spec
        // is refused before any measurement work runs.
        assert!(rejection(&spec(u32::MAX, 5, 5, 3)).contains("warmup_samples"));
        assert!(rejection(&spec(2, MAX_SAMPLES + 1, 5, 3)).contains("baseline_samples"));
        assert!(rejection(&spec(2, 5, u32::MAX, 3)).contains("candidate_samples"));
    }

    #[test]
    fn rejects_counts_below_the_minimum_the_bounds_require() {
        // Such a spec can only ever reject with InsufficientSamples, so it is
        // refused rather than measured first.
        assert!(rejection(&spec(2, 2, 5, 3)).contains("baseline_samples"));
        assert!(rejection(&spec(2, 5, 2, 3)).contains("candidate_samples"));
    }
}
