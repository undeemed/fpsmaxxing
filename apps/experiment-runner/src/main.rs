//! Demonstration entry point for the deterministic experiment engine.
//!
//! Runs one measured experiment against the mock provider through the broker,
//! journals it, then replays it from the durable journal alone and confirms the
//! re-evaluated verdict matches the one recorded at run time. The journal is an
//! in-memory `SQLite` database, so the demo leaves nothing on disk.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{ChangeRequest, DecisionBounds, ExperimentSpec};
use fpsmaxxing_control_plane::ControlPlane;
use fpsmaxxing_experiment_runner::{RunnerError, replay_trial, run_trial};
use fpsmaxxing_mock_provider::MockProvider;
use serde_json::json;

fn main() -> Result<(), RunnerError> {
    let mut plane = ControlPlane::open(Box::new(MockProvider::new(10)), ":memory:")?;
    let spec = demo_spec();

    let trial = run_trial(&mut plane, &spec)?;
    let verdict = &trial.record.verdict;
    println!(
        "trial {} -> {:?} ({:?}); fps_improvement = {:.1}",
        trial.id, verdict.decision, verdict.reason, verdict.fps_improvement
    );
    if let Some(lifecycle) = &trial.record.lifecycle {
        println!(
            "  lifecycle: provider {}, verified = {}, rolled_back = {}",
            lifecycle.provider_id, lifecycle.verified, lifecycle.rolled_back
        );
    }

    let replay = replay_trial(&plane, trial.id)?;
    println!(
        "replay {} -> recomputed {:?}; consistent with journal = {}",
        replay.trial_id,
        replay.recomputed.decision,
        replay.is_consistent()
    );
    Ok(())
}

/// Builds a spec that raises the mock knob within the safety envelope.
fn demo_spec() -> ExperimentSpec {
    ExperimentSpec {
        hypothesis: "raising mock.value from 10 to 40 improves FPS within thermal and power limits"
            .to_owned(),
        target: ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 40 }),
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
