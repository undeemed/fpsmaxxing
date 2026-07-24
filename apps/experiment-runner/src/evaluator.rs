//! The immutable experiment evaluator.
//!
//! [`evaluate`] is a pure function: given recorded baseline and candidate
//! samples plus fixed [`DecisionBounds`] it returns a deterministic
//! [`Verdict`]. It performs no I/O, reads no clock, holds no state, and never
//! mutates its inputs, so a trial can be re-evaluated from the journal alone
//! and always yield an identical verdict.
//!
//! # Decision rule (fixed and ordered)
//!
//! 1. **Minimum samples.** Both the baseline and candidate sets must hold at
//!    least `bounds.min_samples` samples, otherwise the verdict is a
//!    [`Reject`](Decision::Reject) with
//!    [`InsufficientSamples`](VerdictReason::InsufficientSamples).
//! 2. **Constraint bounds.** The candidate's worst temperature, then worst
//!    power draw, then total errors must each stay within the inclusive
//!    ceilings. The first ceiling exceeded rejects, in that order.
//! 3. **Improvement threshold.** The candidate mean FPS must beat the baseline
//!    mean FPS by at least `bounds.min_fps_improvement`. If it does, the
//!    verdict is a [`Promote`](Decision::Promote); otherwise a
//!    [`Reject`](Decision::Reject) with
//!    [`InsufficientImprovement`](VerdictReason::InsufficientImprovement).

use fpsmaxxing_contracts::{
    Decision, DecisionBounds, MetricSample, MetricSummary, Verdict, VerdictReason,
};

/// Applies the immutable decision rule to recorded samples.
///
/// The returned [`Verdict`] carries the baseline and candidate aggregates the
/// decision used, so a journaled trial is self-describing and re-evaluation can
/// be checked against it.
#[must_use]
pub fn evaluate(
    baseline: &[MetricSample],
    candidate: &[MetricSample],
    bounds: &DecisionBounds,
) -> Verdict {
    let baseline_summary = summarize(baseline);
    let candidate_summary = summarize(candidate);
    let fps_improvement = candidate_summary.mean_fps - baseline_summary.mean_fps;
    let min_samples = u64::from(bounds.min_samples.get());

    let reason =
        if baseline_summary.samples < min_samples || candidate_summary.samples < min_samples {
            VerdictReason::InsufficientSamples
        } else if candidate_summary.max_temperature_c > bounds.max_temperature_c {
            VerdictReason::TemperatureExceeded
        } else if candidate_summary.max_power_w > bounds.max_power_w {
            VerdictReason::PowerExceeded
        } else if candidate_summary.total_errors > bounds.max_errors {
            VerdictReason::ErrorsExceeded
        } else if fps_improvement >= bounds.min_fps_improvement {
            VerdictReason::Promoted
        } else {
            VerdictReason::InsufficientImprovement
        };

    let decision = if matches!(reason, VerdictReason::Promoted) {
        Decision::Promote
    } else {
        Decision::Reject
    };

    Verdict {
        decision,
        reason,
        fps_improvement,
        baseline: baseline_summary,
        candidate: candidate_summary,
    }
}

/// Aggregates one measurement set deterministically.
///
/// The FPS mean is the arithmetic mean (zero for an empty set), while the
/// temperature and power fields report the worst (highest) observed value and
/// errors are summed. Counting the divisor as an `f64` avoids a lossy
/// integer-to-float cast without changing the result for realistic sample
/// counts.
fn summarize(samples: &[MetricSample]) -> MetricSummary {
    let mut sum_fps = 0.0_f64;
    let mut divisor = 0.0_f64;
    let mut max_temperature_c = 0.0_f64;
    let mut max_power_w = 0.0_f64;
    let mut total_errors = 0_u64;
    for sample in samples {
        sum_fps += sample.fps;
        divisor += 1.0;
        max_temperature_c = max_temperature_c.max(sample.temperature_c);
        max_power_w = max_power_w.max(sample.power_w);
        total_errors += sample.errors;
    }
    MetricSummary {
        samples: samples.len() as u64,
        mean_fps: if samples.is_empty() {
            0.0
        } else {
            sum_fps / divisor
        },
        max_temperature_c,
        max_power_w,
        total_errors,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{Decision, DecisionBounds, MetricSample, VerdictReason, evaluate};

    fn bounds() -> DecisionBounds {
        DecisionBounds {
            min_samples: NonZeroU32::new(3).expect("min samples is non-zero"),
            min_fps_improvement: 5.0,
            max_temperature_c: 80.0,
            max_power_w: 200.0,
            max_errors: 0,
        }
    }

    fn samples(
        fps: f64,
        temperature_c: f64,
        power_w: f64,
        errors: u64,
        count: usize,
    ) -> Vec<MetricSample> {
        (0..count)
            .map(|_| MetricSample {
                fps,
                temperature_c,
                power_w,
                errors,
            })
            .collect()
    }

    #[test]
    fn promotes_when_every_bound_is_met() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(120.0, 70.0, 180.0, 0, 5);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Promote);
        assert_eq!(verdict.reason, VerdictReason::Promoted);
        assert!((verdict.fps_improvement - 20.0).abs() < f64::EPSILON);
        assert_eq!(verdict.baseline.samples, 5);
        assert!((verdict.candidate.mean_fps - 120.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_when_a_set_is_below_minimum_samples() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(120.0, 70.0, 180.0, 0, 2);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Reject);
        assert_eq!(verdict.reason, VerdictReason::InsufficientSamples);
    }

    #[test]
    fn temperature_ceiling_is_checked_before_improvement() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(140.0, 80.1, 180.0, 0, 5);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Reject);
        assert_eq!(verdict.reason, VerdictReason::TemperatureExceeded);
    }

    #[test]
    fn power_ceiling_rejects_even_with_a_large_gain() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(140.0, 70.0, 200.1, 0, 5);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Reject);
        assert_eq!(verdict.reason, VerdictReason::PowerExceeded);
    }

    #[test]
    fn errors_ceiling_rejects_a_regressing_candidate() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(140.0, 70.0, 180.0, 1, 5);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Reject);
        assert_eq!(verdict.reason, VerdictReason::ErrorsExceeded);
    }

    #[test]
    fn rejects_when_improvement_is_below_threshold() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 5);
        let candidate = samples(104.0, 70.0, 180.0, 0, 5);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Reject);
        assert_eq!(verdict.reason, VerdictReason::InsufficientImprovement);
        assert!((verdict.fps_improvement - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn improvement_exactly_at_threshold_promotes() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 3);
        let candidate = samples(105.0, 70.0, 180.0, 0, 3);
        let verdict = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(verdict.decision, Decision::Promote);
        assert_eq!(verdict.reason, VerdictReason::Promoted);
    }

    #[test]
    fn evaluation_is_deterministic_across_repeated_calls() {
        let baseline = samples(100.0, 60.0, 150.0, 0, 4);
        let candidate = samples(118.0, 72.0, 190.0, 0, 4);
        let first = evaluate(&baseline, &candidate, &bounds());
        let second = evaluate(&baseline, &candidate, &bounds());
        assert_eq!(first, second);
    }
}
