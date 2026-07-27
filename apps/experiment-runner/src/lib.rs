//! Deterministic experiment engine.
//!
//! This crate provides the pieces that turn a typed [`ExperimentSpec`] into an
//! auditable, replayable trial:
//!
//! - a deterministic measurement model that stands in for a live telemetry
//!   source on the Linux safe-alpha path,
//! - the immutable [`evaluate`] decision rule, and
//! - a trial runner that journals every sample and verdict so a trial can be
//!   replayed and re-evaluated from the journal alone.
//!
//! [`ExperimentSpec`]: fpsmaxxing_contracts::ExperimentSpec

mod evaluator;
mod model;
mod runner;

pub use evaluator::evaluate;
pub use model::measure;
pub use runner::{
    LifecycleFailure, LifecycleOutcome, ReplayOutcome, RunnerError, StoredTrial,
    TRIAL_RECORD_VERSION, TrialRecord, replay_trial, run_trial,
};
