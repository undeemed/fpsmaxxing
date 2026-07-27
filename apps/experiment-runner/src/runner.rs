//! The trial runner and its journal-only replay.
//!
//! [`run_trial`] measures a baseline, measures the candidate, applies the
//! immutable evaluator, and - only when the verdict promotes - runs the
//! candidate through the broker lifecycle. Every trial is written to the
//! durable trial journal as a self-describing [`TrialRecord`], so
//! [`replay_trial`] can re-evaluate it from the journal alone and confirm the
//! recorded verdict without chat history or re-running the workload.
//!
//! # Append-only trial records
//!
//! A trial is journaled exactly once, after the lifecycle it authorized has
//! finished, and the trial journal exposes no write that could rewrite that row
//! afterwards. A lifecycle that fails after a promotion - a policy denial, a
//! provider fault, or a rollback that could not be verified - is therefore
//! still recorded, as a [`LifecycleFailure`] carried by the same record as the
//! measurements that authorized the apply, and is also returned to the caller
//! together with the identifier that record was stored under, so the caller
//! addresses its own row rather than the journal's last one - or, when the
//! journal itself refused the write, with the reason those measurements were
//! lost.
//! Crash safety for the window between the mutation and that record stays with
//! the lifecycle journal's write-ahead `apply-intent` stage (ADR 0002).
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
//!
//! Measuring the candidate before it is applied is likewise specific to the
//! pure stand-in model, whose samples depend only on the knob value. Real
//! `PresentMon` or hardware telemetry cannot observe a candidate that was never
//! written, so swapping it in means moving the candidate measurement inside the
//! apply/lease window and running the evaluator gate after it.

use std::num::NonZeroU32;

use fpsmaxxing_contracts::{
    Decision, DecisionBounds, ExperimentSpec, MAX_DECISION_ERRORS, MAX_DECISION_POWER_W,
    MAX_DECISION_TEMPERATURE_C, MAX_HYPOTHESIS_CHARS, MAX_LEASE_SECONDS, MAX_SAMPLES, MetricSample,
    ProviderManifest, Verdict,
};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError, LifecycleResult, MAX_MOCK_VALUE};
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
    /// A promoted trial's lifecycle failed after its record was journaled.
    ///
    /// The record carrying the measurements that authorized the promotion is
    /// written before the error surfaces, so its identifier travels with the
    /// error and the caller can address that exact row. Recovering it by
    /// reading back the last identifier the journal holds would be wrong: the
    /// trial journal is shared, and a concurrent runner can append between the
    /// write and the read.
    ///
    /// Exactly one of `trial_id` and `journal_error` is set: journaling the
    /// record either produced an identifier or failed with a reason. Losing the
    /// measurements is the graver of the two conditions, so the reason it was
    /// lost travels with the error rather than being reported out of band, even
    /// though the lifecycle error stays the primary one.
    #[error("lifecycle failed after {}: {source}", journaled_as(*trial_id, journal_error.as_deref()))]
    LifecycleFailed {
        /// Identifier of the journaled trial, absent only when journaling the
        /// record failed too.
        trial_id: Option<i64>,
        /// Why the record could not be journaled, present only when it was not.
        ///
        /// Boxed so carrying a second broker error does not widen every
        /// `Result` this module returns to the size of two of them.
        journal_error: Option<Box<ControlPlaneError>>,
        /// The broker error that ended the lifecycle.
        #[source]
        source: ControlPlaneError,
    },
    /// The experiment specification is outside the bounded alpha envelope.
    #[error("experiment spec rejected: {0}")]
    InvalidSpec(String),
    /// The experiment target did not carry an unsigned mock value.
    #[error("experiment target is missing an unsigned mock value")]
    InvalidTarget,
    /// The provider snapshot did not carry an unsigned mock value.
    #[error("provider snapshot is missing an unsigned mock value")]
    InvalidBaseline,
    /// The provider is sitting at a value outside the policy envelope.
    #[error("provider snapshot value is {0}, above the {MAX_MOCK_VALUE} the policy allows")]
    BaselineOutOfPolicy(u64),
    /// A journaled trial record disagrees with the spec it carries.
    #[error("journaled trial contradicts its own spec: {0}")]
    InconsistentRecord(String),
    /// A journaled trial record was written by an unsupported record version.
    #[error("journaled trial uses unsupported record version {0}")]
    UnsupportedRecordVersion(u32),
    /// A journaled trial record does not carry a readable record version.
    ///
    /// Distinct from [`UnsupportedRecordVersion`](Self::UnsupportedRecordVersion):
    /// the row states no version at all, so it was not written by any runner
    /// this format describes rather than by an older one.
    #[error("journaled trial does not carry a readable record version")]
    MalformedRecordVersion,
    /// A journaled trial record could not be decoded for replay.
    #[error(transparent)]
    Decode(#[from] serde_json::Error),
}

/// A durable, `Deserialize`-able mirror of a broker [`LifecycleResult`].
///
/// [`LifecycleResult`] is serialize-only; this record round-trips so a promoted
/// trial's lifecycle outcome can be read back during replay and audit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
/// `lifecycle_error` is set, because the record is written once the lifecycle
/// has finished. On a [`Reject`](Decision::Reject) both are absent: no
/// lifecycle ran.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
///
/// A replay is a tamper-detection read, so the outcome must be inspected:
/// [`is_consistent`](Self::is_consistent) reports whether the recomputation
/// agrees with the journal, and `policy_legal` reports whether the record is one
/// the policy gate would still accept.
#[derive(Clone, Debug)]
#[must_use]
pub struct ReplayOutcome {
    /// The trial-journal identifier that was replayed.
    pub trial_id: i64,
    /// The verdict read back from the journal.
    pub recorded: Verdict,
    /// The verdict recomputed from the journaled samples and bounds.
    pub recomputed: Verdict,
    /// Whether the journaled record passes the policy gate as it stands now and
    /// agrees with the spec it carries; see [`replay_trial`].
    pub policy_legal: bool,
    /// Which gate the record tripped, absent when `policy_legal` holds.
    ///
    /// The gate produces a precise message naming the offending field, so a
    /// flagged trial is diagnosable without re-deriving the cause by hand.
    pub policy_reason: Option<String>,
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
/// [`Promote`](Decision::Promote). The record is appended to the durable trial
/// journal once, carrying whichever of the lifecycle outcome or the lifecycle
/// error the promotion produced, so the trial is replayable whether or not the
/// lifecycle succeeded.
///
/// # Errors
///
/// Returns an error if the spec is outside the bounded envelope, if its target
/// names a capability the measurement model does not describe or the accepted
/// provider does not advertise, if the spec target lacks an unsigned mock value,
/// if the provider snapshot lacks one or reports one outside the policy
/// envelope, or if the broker or durable journal rejects an operation. A
/// lifecycle error is returned only after the trial record carrying it has been
/// journaled, as a [`LifecycleFailed`](RunnerError::LifecycleFailed) naming the
/// identifier that record was stored under; if that write also fails, the
/// lifecycle error still takes precedence, the identifier is absent, and the
/// same error carries why the record was lost.
pub fn run_trial(
    plane: &mut ControlPlane,
    spec: &ExperimentSpec,
) -> Result<StoredTrial, RunnerError> {
    let candidate_value = validate(plane.capabilities(), spec)?;
    let baseline_value = baseline_value(plane)?;
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
    let outcome = match verdict.decision {
        Decision::Promote => Some(plane.run_lifecycle(&spec.target)),
        Decision::Reject => None,
    };
    let record = TrialRecord {
        schema_version: TRIAL_RECORD_VERSION,
        spec: spec.clone(),
        baseline_value,
        candidate_value,
        baseline_samples,
        candidate_samples,
        verdict,
        lifecycle: outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok())
            .map(LifecycleOutcome::from),
        lifecycle_error: outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().err())
            .map(LifecycleFailure::from),
    };
    let journaled = plane.record_trial(&record);
    if let Some(Err(source)) = outcome {
        let (trial_id, journal_error) = match journaled {
            Ok(id) => (Some(id), None),
            Err(journal_error) => (None, Some(Box::new(journal_error))),
        };
        return Err(RunnerError::LifecycleFailed {
            trial_id,
            journal_error,
            source,
        });
    }
    Ok(StoredTrial {
        id: journaled?,
        record,
    })
}

/// Re-evaluates a journaled trial from the journal alone.
///
/// Reads the [`TrialRecord`], recomputes the verdict from its recorded samples
/// and bounds with the same immutable evaluator, and returns both the recorded
/// and recomputed verdicts for comparison. It consults no chat history and
/// re-runs no workload.
///
/// The journaled record is re-checked against the policy gate as well, so a row
/// that was rewritten after the fact is reported as `policy_legal = false`, with
/// the gate it tripped in `policy_reason`, even when re-evaluating it reproduces
/// the recorded verdict.
///
/// # Errors
///
/// Returns an error if the trial cannot be read from the durable journal, its
/// record cannot be decoded, the record was written by an unsupported
/// [`TRIAL_RECORD_VERSION`], or it states no readable version at all.
pub fn replay_trial(plane: &ControlPlane, id: i64) -> Result<ReplayOutcome, RunnerError> {
    let payload = plane.read_trial(id)?;
    // The version is read off the raw payload so a record whose fields this
    // build cannot decode still reports the version that wrote it rather than a
    // decode error that says nothing about why. A row stating no readable
    // version is not an older record but a foreign or corrupted one, so it is
    // reported apart from a version this build merely does not support.
    let version = payload
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or(RunnerError::MalformedRecordVersion)?;
    if version != TRIAL_RECORD_VERSION {
        return Err(RunnerError::UnsupportedRecordVersion(version));
    }
    let record: TrialRecord = serde_json::from_value(payload)?;
    let recomputed = evaluate(
        &record.baseline_samples,
        &record.candidate_samples,
        &record.spec.bounds,
    );
    let rejection = check_policy(&record).err();
    Ok(ReplayOutcome {
        trial_id: id,
        recorded: record.verdict,
        recomputed,
        policy_legal: rejection.is_none(),
        policy_reason: rejection.map(|error| error.to_string()),
    })
}

/// Checks a journaled record against the gate as it stands now.
///
/// Replay is the tamper-detection read, so it re-runs the whole of
/// [`validate_spec`] over the journaled spec rather than only its bounds: a row
/// whose capability, hypothesis, sample counts, decision bounds, candidate
/// value, or TTL lease were rewritten is reported as illegal even though
/// re-evaluating it reproduces the recorded verdict.
///
/// The gate applied is the current one, so tightening a policy constant flags
/// every archived row recorded under the looser ceiling. That is the intended
/// reading rather than a false alarm - those trials are outside the envelope the
/// alpha now permits - and ADR 0002 makes a constant change a
/// [`TRIAL_RECORD_VERSION`] bump so a flagged archive stays attributable.
///
/// The attached provider's manifest is deliberately not consulted. A trial
/// journal outlives the process that wrote it, so requiring the provider running
/// now to still advertise the journaled capability would flag every archived row
/// as tampered with. The capability is instead held to the constant the
/// measurement model describes, which is the term that decided the run-time
/// rejection anyway.
///
/// The record's redundant self-describing fields are cross-checked against that
/// spec as well. [`run_trial`] always journals the value it validated, exactly
/// the sample counts the spec asked for, and a baseline inside the same
/// [`MAX_MOCK_VALUE`] ceiling, so a record contradicting its own spec was not
/// written by this runner. The lifecycle fields are held to the decision that
/// authorized them for the same reason: the trial row is the only auditable
/// statement about whether a promotion reached the provider, so a promotion
/// whose lifecycle went unrecorded, or a rejection carrying one, is a claim this
/// runner cannot make. A recorded lifecycle is held to its own contents too:
/// [`ControlPlane::run_lifecycle`] returns an outcome only after the applied
/// value verified and the captured baseline was restored, so a row claiming a
/// promotion that went unverified, or one whose knob was left mutated, is the
/// same kind of unwritable claim.
///
/// Detection stops at that structural layer. The recorded samples are not
/// re-derived, so a rewrite of the measurements together with the verdict they
/// imply passes both this check and the verdict comparison; catching that needs
/// each row anchored outside itself - a signed or hash-chained journal - which
/// the alpha deliberately does not do.
///
/// # Errors
///
/// Returns the gate the record tripped, naming the offending field.
fn check_policy(record: &TrialRecord) -> Result<(), RunnerError> {
    let candidate_value = validate_spec(&record.spec)?;
    if candidate_value != record.candidate_value {
        return Err(RunnerError::InconsistentRecord(format!(
            "candidate_value is {}, but its spec asks for {candidate_value}",
            record.candidate_value
        )));
    }
    if record.baseline_value > MAX_MOCK_VALUE {
        return Err(RunnerError::InconsistentRecord(format!(
            "baseline_value is {}, above the {MAX_MOCK_VALUE} the policy allows",
            record.baseline_value
        )));
    }
    for (label, samples, declared) in [
        (
            "baseline_samples",
            &record.baseline_samples,
            record.spec.baseline_samples,
        ),
        (
            "candidate_samples",
            &record.candidate_samples,
            record.spec.candidate_samples,
        ),
    ] {
        if !holds_declared_count(samples, declared) {
            return Err(RunnerError::InconsistentRecord(format!(
                "{label} holds {} samples, but its spec declares {}",
                samples.len(),
                declared.get()
            )));
        }
    }
    let recorded_lifecycle = record.lifecycle.is_some();
    let recorded_failure = record.lifecycle_error.is_some();
    let holds = match record.verdict.decision {
        Decision::Promote => recorded_lifecycle != recorded_failure,
        Decision::Reject => !recorded_lifecycle && !recorded_failure,
    };
    if !holds {
        return Err(RunnerError::InconsistentRecord(format!(
            "verdict is {:?} with lifecycle {} and lifecycle_error {}, but a promotion records exactly one of the two and a rejection neither",
            record.verdict.decision,
            presence(recorded_lifecycle),
            presence(recorded_failure)
        )));
    }
    if let Some(lifecycle) = &record.lifecycle
        && !(lifecycle.verified && lifecycle.rolled_back)
    {
        return Err(RunnerError::InconsistentRecord(format!(
            "lifecycle records verified {} and rolled_back {}, but a completed lifecycle observes the applied value and restores the captured baseline",
            lifecycle.verified, lifecycle.rolled_back
        )));
    }
    Ok(())
}

/// Renders where a failed lifecycle's trial record came to rest.
fn journaled_as(trial_id: Option<i64>, journal_error: Option<&ControlPlaneError>) -> String {
    match (trial_id, journal_error) {
        (Some(id), _) => format!("journaling trial {id}"),
        (None, Some(error)) => format!("failing to journal its trial: {error}"),
        (None, None) => "failing to journal its trial".to_owned(),
    }
}

/// Renders whether an optional record field was journaled.
fn presence(recorded: bool) -> &'static str {
    if recorded { "present" } else { "absent" }
}

/// Whether a recorded measurement set holds exactly the count its spec declared.
fn holds_declared_count(samples: &[MetricSample], declared: NonZeroU32) -> bool {
    u32::try_from(samples.len()).is_ok_and(|counted| counted == declared.get())
}

/// Rejects a spec the attached provider cannot serve, then applies
/// [`validate_spec`] and returns the validated candidate knob value.
///
/// The provider check belongs to the run-time path alone: a trial is about to
/// drive a lifecycle against whatever provider is attached now, so a target that
/// provider does not advertise must fail closed here. The broker refuses it too,
/// but only inside the lifecycle, which a rejected trial never reaches - and by
/// then a measurement would already have been journaled as an authoritative
/// trial. Replay has no lifecycle to run and so applies [`validate_spec`] on its
/// own; see [`check_policy`].
fn validate(manifest: &ProviderManifest, spec: &ExperimentSpec) -> Result<u64, RunnerError> {
    if !manifest
        .capabilities
        .iter()
        .any(|capability| capability.id == spec.target.capability_id)
    {
        return Err(ControlPlaneError::UnknownCapability(spec.target.capability_id.clone()).into());
    }
    validate_spec(spec)
}

/// Rejects a spec whose parameters are unbounded or self-contradictory and
/// returns the validated candidate knob value.
///
/// Everything the measurement phase and the decision gate consume arrives over
/// the wire, so all of it is bounded before any measurement work runs: the
/// target must name the single capability the measurement model describes
/// ([`MODELED_CAPABILITY_ID`](model::MODELED_CAPABILITY_ID)), sample counts size
/// the measurement buffers and are checked against [`MAX_SAMPLES`], the decision
/// bounds are intersected with the policy envelope by [`validate_bounds`], and
/// the candidate knob value drives the modeled metrics and is checked against
/// [`MAX_MOCK_VALUE`], the same ceiling the broker policy enforces later in the
/// lifecycle and the same one [`baseline_value`] holds the provider to. The
/// target's TTL lease is bounded here too, against the broker's own
/// [`MAX_LEASE_SECONDS`]: it is the last broker-enforced field on the change
/// request, and a spec the lifecycle can only ever deny must not be measured
/// and journaled as an authoritative trial first. A spec that asks for fewer
/// counted samples than its own bounds require can never promote either, so it
/// is refused up front rather than measured and then rejected. The hypothesis is
/// the one field neither phase consumes, but it is written verbatim into a
/// single durable trial row, so it is held to [`MAX_HYPOTHESIS_CHARS`] rather
/// than sizing that row by whatever the author sent. The validated value is
/// returned so the measurement uses exactly what was bounded here.
///
/// The capability check is what keeps unknown hardware failing closed, and it is
/// the model rather than the registry that decides what is known: the
/// measurement path is hard-wired to the mock knob, so advertising a second
/// capability must not make it measurable.
///
/// Nothing here reads process state, so [`replay_trial`] can re-run the whole of
/// it over a journaled spec.
fn validate_spec(spec: &ExperimentSpec) -> Result<u64, RunnerError> {
    if spec.target.capability_id != model::MODELED_CAPABILITY_ID {
        return Err(ControlPlaneError::UnknownCapability(spec.target.capability_id.clone()).into());
    }
    validate_bounds(&spec.bounds)?;
    let hypothesis = spec.hypothesis.chars().count();
    if hypothesis > MAX_HYPOTHESIS_CHARS as usize {
        return Err(RunnerError::InvalidSpec(format!(
            "hypothesis is {hypothesis} characters, above the {MAX_HYPOTHESIS_CHARS} ceiling"
        )));
    }
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
    let value = candidate_value(spec)?;
    if value > MAX_MOCK_VALUE {
        return Err(RunnerError::InvalidSpec(format!(
            "target value is {value}, above the {MAX_MOCK_VALUE} the policy allows"
        )));
    }
    let lease = spec.target.lease_seconds.get();
    if lease > MAX_LEASE_SECONDS {
        return Err(RunnerError::InvalidSpec(format!(
            "lease_seconds is {lease}, above the {MAX_LEASE_SECONDS} the policy allows"
        )));
    }
    Ok(value)
}

/// Rejects decision bounds that are looser than the policy envelope.
///
/// The evaluator is immutable, but its thresholds arrive in the spec, so a spec
/// could otherwise disarm the gate it is supposed to pass by declaring a
/// ceiling nothing can exceed. Each threshold is intersected with the
/// policy-owned envelope: a spec may tighten a bound but never loosen it past
/// [`MAX_DECISION_TEMPERATURE_C`], [`MAX_DECISION_POWER_W`],
/// [`MAX_DECISION_ERRORS`], or [`MAX_SAMPLES`], and a required improvement must
/// be a finite, non-negative gain. Non-finite thresholds are refused outright,
/// because a `NaN` compares false against every ceiling and would silently pass
/// the gate.
///
/// This covers every field of [`DecisionBounds`], so [`replay_trial`] can re-run
/// it over a journaled spec to decide whether a recorded verdict was reached
/// under thresholds the policy ever allowed.
fn validate_bounds(bounds: &DecisionBounds) -> Result<(), RunnerError> {
    let min_samples = bounds.min_samples.get();
    if min_samples > MAX_SAMPLES {
        return Err(RunnerError::InvalidSpec(format!(
            "min_samples is {min_samples}, above the {MAX_SAMPLES} ceiling"
        )));
    }
    for (label, bound, ceiling) in [
        (
            "max_temperature_c",
            bounds.max_temperature_c,
            MAX_DECISION_TEMPERATURE_C,
        ),
        ("max_power_w", bounds.max_power_w, MAX_DECISION_POWER_W),
    ] {
        if !bound.is_finite() || bound <= 0.0 || bound > ceiling {
            return Err(RunnerError::InvalidSpec(format!(
                "{label} is {bound}, outside the 0 exclusive to {ceiling} inclusive the policy allows"
            )));
        }
    }
    let improvement = bounds.min_fps_improvement;
    if !improvement.is_finite() || improvement < 0.0 {
        return Err(RunnerError::InvalidSpec(format!(
            "min_fps_improvement is {improvement}, but the policy requires a finite gain of at least 0"
        )));
    }
    if bounds.max_errors > MAX_DECISION_ERRORS {
        return Err(RunnerError::InvalidSpec(format!(
            "max_errors is {}, above the {MAX_DECISION_ERRORS} the policy allows",
            bounds.max_errors
        )));
    }
    Ok(())
}

/// Reads the baseline knob value from the provider's current state.
///
/// The baseline arrives from the provider rather than the spec, but it drives
/// the same measurement model, so it is held to the same [`MAX_MOCK_VALUE`]
/// ceiling as the candidate. A provider sitting outside the policy envelope
/// fails closed instead of contributing a modeled baseline the envelope would
/// never have permitted to the journaled trial.
fn baseline_value(plane: &ControlPlane) -> Result<u64, RunnerError> {
    let value = plane
        .snapshot()?
        .state
        .get("value")
        .and_then(Value::as_u64)
        .ok_or(RunnerError::InvalidBaseline)?;
    if value > MAX_MOCK_VALUE {
        return Err(RunnerError::BaselineOutOfPolicy(value));
    }
    Ok(value)
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

    use fpsmaxxing_contracts::{
        CapabilityDescriptor, ChangeRequest, DecisionBounds, Persistence, RiskClass,
    };
    use fpsmaxxing_control_plane::ControlPlaneError;
    use serde_json::json;

    use super::{
        ExperimentSpec, MAX_DECISION_ERRORS, MAX_DECISION_POWER_W, MAX_DECISION_TEMPERATURE_C,
        MAX_HYPOTHESIS_CHARS, MAX_LEASE_SECONDS, MAX_MOCK_VALUE, MAX_SAMPLES, ProviderManifest,
        RunnerError, validate,
    };

    /// A manifest advertising only the knob the measurement model describes.
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            id: "mock".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec![std::env::consts::OS.to_owned()],
            capabilities: vec![CapabilityDescriptor {
                id: "mock.value".to_owned(),
                description: "Sets an in-memory value".to_owned(),
                risk: RiskClass::Reversible,
                persistence: Persistence::Leased,
                input_schema: json!({ "type": "object" }),
            }],
        }
    }

    fn spec(warmup: u32, baseline: u32, candidate: u32, min_samples: u32) -> ExperimentSpec {
        spec_for_value(warmup, baseline, candidate, min_samples, 40)
    }

    fn spec_for_value(
        warmup: u32,
        baseline: u32,
        candidate: u32,
        min_samples: u32,
        value: u64,
    ) -> ExperimentSpec {
        ExperimentSpec {
            hypothesis: "raise mock.value".to_owned(),
            target: ChangeRequest {
                capability_id: "mock.value".to_owned(),
                parameters: json!({ "value": value }),
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
        match validate(&manifest(), spec) {
            Err(RunnerError::InvalidSpec(message)) => message,
            other => panic!("spec should be rejected, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_spec_inside_the_envelope() {
        assert_eq!(
            validate(&manifest(), &spec(2, 5, 5, 3)).expect("spec is in envelope"),
            40
        );
        assert!(validate(&manifest(), &spec(MAX_SAMPLES, MAX_SAMPLES, MAX_SAMPLES, 3)).is_ok());
    }

    #[test]
    fn rejects_bounds_looser_than_the_policy_envelope() {
        // The gate's own thresholds arrive in the spec, so a spec that declares
        // ceilings nothing can exceed would promote unconditionally.
        let mut loosened = spec(2, 5, 5, 3);
        loosened.bounds.max_temperature_c = MAX_DECISION_TEMPERATURE_C + 0.1;
        assert!(rejection(&loosened).contains("max_temperature_c"));
        loosened.bounds.max_temperature_c = f64::INFINITY;
        assert!(rejection(&loosened).contains("max_temperature_c"));
        loosened.bounds.max_temperature_c = f64::NAN;
        assert!(rejection(&loosened).contains("max_temperature_c"));
        loosened.bounds.max_temperature_c = 0.0;
        assert!(rejection(&loosened).contains("max_temperature_c"));

        let mut loosened = spec(2, 5, 5, 3);
        loosened.bounds.max_power_w = MAX_DECISION_POWER_W + 0.1;
        assert!(rejection(&loosened).contains("max_power_w"));

        let mut loosened = spec(2, 5, 5, 3);
        loosened.bounds.min_fps_improvement = -1.0;
        assert!(rejection(&loosened).contains("min_fps_improvement"));
        loosened.bounds.min_fps_improvement = f64::NAN;
        assert!(rejection(&loosened).contains("min_fps_improvement"));

        let mut loosened = spec(2, 5, 5, 3);
        loosened.bounds.max_errors = MAX_DECISION_ERRORS + 1;
        assert!(rejection(&loosened).contains("max_errors"));

        let mut loosened = spec(2, 5, 5, 3);
        loosened.bounds.min_samples =
            NonZeroU32::new(MAX_SAMPLES + 1).expect("min samples is non-zero");
        assert!(rejection(&loosened).contains("min_samples"));
    }

    #[test]
    fn accepts_bounds_exactly_at_the_policy_envelope() {
        // The envelope is inclusive, and a spec is free to tighten within it.
        let mut tightest = spec(2, 5, 5, 3);
        tightest.bounds.max_temperature_c = MAX_DECISION_TEMPERATURE_C;
        tightest.bounds.max_power_w = MAX_DECISION_POWER_W;
        tightest.bounds.max_errors = MAX_DECISION_ERRORS;
        tightest.bounds.min_fps_improvement = 0.0;
        assert!(validate(&manifest(), &tightest).is_ok());
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

    #[test]
    fn rejects_a_candidate_value_above_the_policy_bound() {
        // The broker only sees the value inside the lifecycle, which a rejected
        // trial never reaches, so the measurement path bounds it itself.
        assert!(validate(&manifest(), &spec_for_value(2, 5, 5, 3, MAX_MOCK_VALUE)).is_ok());
        let message = rejection(&spec_for_value(2, 5, 5, 3, MAX_MOCK_VALUE + 1));
        assert!(message.contains("target value"), "{message}");
        assert!(rejection(&spec_for_value(2, 5, 5, 3, u64::MAX)).contains("target value"));
    }

    #[test]
    fn rejects_a_lease_above_the_policy_bound() {
        // The lease is the TTL bounding how long a mutation may persist, and
        // the broker only sees it inside the lifecycle. Measuring first would
        // journal an authoritative trial the lifecycle can only ever deny.
        let mut leased = spec(2, 5, 5, 3);
        leased.target.lease_seconds =
            NonZeroU64::new(MAX_LEASE_SECONDS).expect("lease is non-zero");
        assert!(validate(&manifest(), &leased).is_ok());

        leased.target.lease_seconds =
            NonZeroU64::new(MAX_LEASE_SECONDS + 1).expect("lease is non-zero");
        assert!(rejection(&leased).contains("lease_seconds"));

        leased.target.lease_seconds = NonZeroU64::MAX;
        assert!(rejection(&leased).contains("lease_seconds"));
    }

    #[test]
    fn rejects_a_hypothesis_above_the_policy_bound() {
        // The hypothesis is free text the runner writes verbatim into one
        // durable row, so the row is not sized by whatever the author sent.
        // The ceiling counts characters, as the schema's maxLength does.
        let ceiling = MAX_HYPOTHESIS_CHARS as usize;
        let mut verbose = spec(2, 5, 5, 3);
        verbose.hypothesis = "\u{e9}".repeat(ceiling);
        assert!(validate(&manifest(), &verbose).is_ok());

        verbose.hypothesis = "\u{e9}".repeat(ceiling + 1);
        let message = rejection(&verbose);
        assert!(message.contains("hypothesis"), "{message}");
        assert!(
            message.contains(&format!("{} characters", ceiling + 1)),
            "the ceiling counts characters rather than bytes: {message}"
        );
    }

    #[test]
    fn rejects_a_capability_the_provider_does_not_advertise() {
        // The measurement model only describes the mock knob, so a target the
        // registry never accepted must fail closed before anything is measured
        // or journaled - not later, inside the lifecycle.
        let mut foreign = spec(2, 5, 5, 3);
        foreign.target.capability_id = "gpu.core-clock-offset".to_owned();
        let error = validate(&manifest(), &foreign).expect_err("unknown hardware fails closed");
        assert!(
            matches!(
                &error,
                RunnerError::ControlPlane(ControlPlaneError::UnknownCapability(id))
                    if id == "gpu.core-clock-offset"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn rejects_an_advertised_capability_the_model_does_not_describe() {
        // Advertising a knob does not make it measurable: the model encodes the
        // mock knob's coefficients only, so a second capability on the same
        // provider must not be measured with it and journaled as authoritative.
        let mut manifest = manifest();
        let mut other = manifest.capabilities[0].clone();
        other.id = "mock.other".to_owned();
        manifest.capabilities.push(other);

        let mut foreign = spec(2, 5, 5, 3);
        foreign.target.capability_id = "mock.other".to_owned();
        let error = validate(&manifest, &foreign).expect_err("an unmodeled knob fails closed");
        assert!(
            matches!(
                &error,
                RunnerError::ControlPlane(ControlPlaneError::UnknownCapability(id))
                    if id == "mock.other"
            ),
            "{error:?}"
        );
        assert!(validate(&manifest, &spec(2, 5, 5, 3)).is_ok());
    }

    #[test]
    fn rejects_a_target_without_an_unsigned_value() {
        let mut spec = spec(2, 5, 5, 3);
        spec.target.parameters = json!({ "value": "high" });
        assert!(matches!(
            validate(&manifest(), &spec),
            Err(RunnerError::InvalidTarget)
        ));
    }
}
