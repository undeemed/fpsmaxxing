//! The trial runner and its journal-only replay.
//!
//! [`run_trial`] measures a baseline, measures the candidate, applies the
//! immutable evaluator, and - only when the verdict promotes - runs the
//! candidate through the broker lifecycle. Every trial is written to the
//! durable trial journal as a self-describing [`TrialRecord`], so
//! [`replay_trial`] can re-evaluate it from the journal alone and confirm the
//! recorded verdict without chat history or re-running the workload.
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

use fpsmaxxing_contracts::{Decision, ExperimentSpec, MetricSample, Verdict};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError, LifecycleResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{evaluate, model};

/// Fail-closed errors raised while running or replaying a trial.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The broker or the durable journal rejected an operation.
    #[error(transparent)]
    ControlPlane(#[from] ControlPlaneError),
    /// The experiment target did not carry an unsigned mock value.
    #[error("experiment target is missing an unsigned mock value")]
    InvalidTarget,
    /// The provider snapshot did not carry an unsigned mock value.
    #[error("provider snapshot is missing an unsigned mock value")]
    InvalidBaseline,
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

/// The complete, replayable record of one trial.
///
/// It holds everything the immutable evaluator needs, so re-evaluation reads
/// only this record: the spec (for the decision bounds), the recorded baseline
/// and candidate samples, and the verdict the evaluator produced.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TrialRecord {
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
    /// The broker lifecycle outcome, present only when the trial promoted.
    pub lifecycle: Option<LifecycleOutcome>,
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
/// Measures the baseline from the provider's current state, measures the
/// candidate from the spec target, evaluates the two, and - only on a
/// [`Promote`](Decision::Promote) - runs the candidate through the broker
/// lifecycle. The trial is written to the durable trial journal before the
/// stored record is returned.
///
/// # Errors
///
/// Returns an error if the spec target or provider snapshot lacks an unsigned
/// mock value, or if the broker or durable journal rejects an operation.
pub fn run_trial(
    plane: &mut ControlPlane,
    spec: &ExperimentSpec,
) -> Result<StoredTrial, RunnerError> {
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
    let lifecycle = match verdict.decision {
        Decision::Promote => Some(LifecycleOutcome::from(&plane.run_lifecycle(&spec.target)?)),
        Decision::Reject => None,
    };
    let record = TrialRecord {
        spec: spec.clone(),
        baseline_value,
        candidate_value,
        baseline_samples,
        candidate_samples,
        verdict,
        lifecycle,
    };
    let id = plane.record_trial(&record)?;
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
/// Returns an error if the trial cannot be read from the durable journal or its
/// record cannot be decoded.
pub fn replay_trial(plane: &ControlPlane, id: i64) -> Result<ReplayOutcome, RunnerError> {
    let record: TrialRecord = serde_json::from_value(plane.read_trial(id)?)?;
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
