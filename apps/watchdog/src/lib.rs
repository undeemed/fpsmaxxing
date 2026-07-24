//! Independent lease and crash-recovery watchdog.
//!
//! The watchdog restores prior state after the gateway, workload, or agent
//! dies. It reads the durable experiment journal owned by
//! `crates/control-plane`, finds experiments that recorded a write-ahead
//! apply intent but no terminal outcome, and rolls each one back through its
//! provider using the journaled snapshot. It never depends on the gateway or
//! experiment runner at run time and writes nothing to the journal except its
//! own restore-outcome records, keeping the recovery path independent as the
//! safety invariants require.

mod journal;

use std::path::Path;

use fpsmaxxing_contracts::{ChangeRequest, StateSnapshot};
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use serde::Serialize;
use thiserror::Error;

use crate::journal::WatchdogJournal;

/// Fail-closed errors raised while the watchdog reads or repairs the journal.
#[derive(Debug, Error)]
pub enum WatchdogError {
    /// The durable journal could not be read or written.
    #[error(transparent)]
    Journal(#[from] rusqlite::Error),
    /// A journal record could not be decoded or encoded.
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

/// Which unclosed experiments a reclaim pass should restore.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReclaimPolicy {
    /// Restore every unclosed experiment regardless of its lease deadline.
    ///
    /// Used for crash and reboot recovery, when the watchdog assumes any
    /// experiment without a terminal record was abandoned by a dead owner.
    AllUnclosed,
    /// Restore only experiments whose TTL lease has already elapsed.
    ///
    /// Used by the steady-state poll loop so that experiments still inside
    /// their lease are left to their owner.
    ExpiredLeasesOnly,
}

/// Why the watchdog restored an experiment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReclaimReason {
    /// The experiment's TTL lease elapsed with no terminal outcome.
    LeaseExpired,
    /// The experiment had no terminal outcome and was restored during crash
    /// or reboot recovery before its lease elapsed.
    CrashRecovery,
}

impl ReclaimReason {
    /// Human-readable summary written into the terminal `failed` record.
    pub(crate) fn terminal_message(self) -> &'static str {
        match self {
            Self::LeaseExpired => "watchdog restored prior state after the TTL lease expired",
            Self::CrashRecovery => {
                "watchdog restored prior state after detecting an unclosed experiment"
            }
        }
    }
}

/// One journaled experiment that recorded an apply intent but no terminal
/// outcome, i.e. a crash between the mutation and its rollback.
#[derive(Clone, Debug)]
pub struct UnclosedExperiment {
    /// Correlation ID shared by every journal record of this experiment.
    pub experiment_id: i64,
    /// Provider that owns the leaked change.
    pub provider_id: String,
    /// Pre-change state captured before the mutation, used for rollback.
    pub snapshot: StateSnapshot,
    /// The write-ahead change request, which carries the TTL lease.
    pub request: ChangeRequest,
    /// Whether the TTL lease has elapsed relative to the journal clock.
    pub lease_expired: bool,
}

impl UnclosedExperiment {
    fn reason(&self) -> ReclaimReason {
        if self.lease_expired {
            ReclaimReason::LeaseExpired
        } else {
            ReclaimReason::CrashRecovery
        }
    }

    fn selected_by(&self, policy: ReclaimPolicy) -> bool {
        match policy {
            ReclaimPolicy::AllUnclosed => true,
            ReclaimPolicy::ExpiredLeasesOnly => self.lease_expired,
        }
    }
}

/// The outcome of one watchdog restore attempt.
#[derive(Clone, Debug)]
pub struct Restoration {
    /// Experiment the watchdog acted on.
    pub experiment_id: i64,
    /// Why the watchdog acted on this experiment.
    pub reason: ReclaimReason,
    /// The TTL lease the experiment carried, in seconds.
    pub lease_seconds: u64,
    /// Whether rollback restored the journaled snapshot and was verified.
    pub restored: bool,
    /// Failure detail when `restored` is false; the experiment is left
    /// unclosed so a later pass retries it.
    pub error: Option<String>,
}

/// An independent watchdog bound to one provider and one journal.
pub struct Watchdog<P: Provider> {
    provider: P,
    provider_id: String,
    journal: WatchdogJournal,
}

impl<P: Provider> Watchdog<P> {
    /// Opens a watchdog for `provider` against the journal at `journal_path`.
    ///
    /// The journal file need not exist yet; an absent or schema-less journal
    /// simply yields no work.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal connection cannot be opened.
    pub fn open(provider: P, journal_path: impl AsRef<Path>) -> Result<Self, WatchdogError> {
        let provider_id = provider.manifest().id;
        let journal = WatchdogJournal::open(journal_path)?;
        Ok(Self {
            provider,
            provider_id,
            journal,
        })
    }

    /// Borrows the bound provider, e.g. to inspect restored state.
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Lists every unclosed experiment this provider owns, newest last.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be queried or a record cannot be
    /// decoded.
    pub fn scan(&self) -> Result<Vec<UnclosedExperiment>, WatchdogError> {
        self.journal.unclosed_experiments(&self.provider_id)
    }

    /// Restores every unclosed experiment selected by `policy`, journaling a
    /// correlation-ID-linked restore record for each and closing every
    /// successfully restored experiment with a terminal record.
    ///
    /// Rollback and journaling are per-experiment: a provider that fails to
    /// roll one experiment back leaves that experiment unclosed for a later
    /// pass without blocking the others. The pass is idempotent - a restored
    /// experiment gains a terminal record and is never selected again, so
    /// running the watchdog twice does not roll the same experiment back
    /// twice.
    ///
    /// # Errors
    ///
    /// Returns an error only if the journal cannot be scanned or a restore
    /// record cannot be written; per-experiment rollback failures are reported
    /// in the returned [`Restoration`] list, not as errors.
    pub fn reclaim(&mut self, policy: ReclaimPolicy) -> Result<Vec<Restoration>, WatchdogError> {
        let candidates = self.scan()?;
        let mut restorations = Vec::new();
        for experiment in candidates {
            if experiment.selected_by(policy) {
                restorations.push(self.reclaim_one(&experiment)?);
            }
        }
        Ok(restorations)
    }

    fn reclaim_one(
        &mut self,
        experiment: &UnclosedExperiment,
    ) -> Result<Restoration, WatchdogError> {
        let reason = experiment.reason();
        let lease_seconds = experiment.request.lease_seconds.get();
        match self.restore_state(&experiment.snapshot) {
            Ok(()) => {
                self.journal.record_success(
                    experiment.experiment_id,
                    &self.provider_id,
                    reason,
                    &experiment.snapshot,
                )?;
                Ok(Restoration {
                    experiment_id: experiment.experiment_id,
                    reason,
                    lease_seconds,
                    restored: true,
                    error: None,
                })
            }
            Err(error) => {
                let message = error.to_string();
                self.journal.record_failure(
                    experiment.experiment_id,
                    &self.provider_id,
                    reason,
                    &experiment.snapshot,
                    &message,
                )?;
                Ok(Restoration {
                    experiment_id: experiment.experiment_id,
                    reason,
                    lease_seconds,
                    restored: false,
                    error: Some(message),
                })
            }
        }
    }

    /// Rolls the provider back to `snapshot` and confirms the restore by
    /// re-reading provider state, mirroring the control plane's own
    /// rollback-verify step.
    fn restore_state(&mut self, snapshot: &StateSnapshot) -> Result<(), ProviderError> {
        self.provider.rollback(snapshot)?;
        let restored = self.provider.snapshot()?;
        if restored == *snapshot {
            Ok(())
        } else {
            Err(ProviderError::RollbackFailed(
                "post-rollback snapshot did not match the journaled snapshot".to_owned(),
            ))
        }
    }
}
