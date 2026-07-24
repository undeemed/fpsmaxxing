//! Read-only journal access plus append-only restore-outcome records.
//!
//! The watchdog reads the durable experiment journal owned by
//! `crates/control-plane` but writes nothing except its own `watchdog-restore`
//! records and the terminal `failed` record that closes an experiment it has
//! restored. It never mutates or creates the schema, so a live control plane
//! and the watchdog can share one journal file.

use std::{path::Path, time::Duration};

use fpsmaxxing_contracts::{ChangeRequest, StateSnapshot};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::json;

use crate::{ReclaimReason, UnclosedExperiment, WatchdogError};

/// Appends one journal record, timestamped with the same UTC format the
/// control plane uses.
const INSERT_RECORD: &str =
    "INSERT INTO experiment_journal (experiment_id, recorded_at, stage, provider_id, payload)
     VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?2, ?3, ?4)";

pub(crate) struct WatchdogJournal {
    connection: Connection,
}

impl WatchdogJournal {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, WatchdogError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { connection })
    }

    /// Returns unclosed experiments (an `apply-intent` record with no
    /// `completed`/`failed` terminal record) owned by `provider_id`, each
    /// annotated with whether its TTL lease has elapsed. Lease age is computed
    /// with the journal's own clock; the lease itself is decoded from the
    /// write-ahead change request rather than trusting the JSON1 extension.
    pub(crate) fn unclosed_experiments(
        &self,
        provider_id: &str,
    ) -> Result<Vec<UnclosedExperiment>, WatchdogError> {
        if !self.journal_ready()? {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT
                 intent.experiment_id,
                 snapshot.payload,
                 intent.payload,
                 CAST(strftime('%s', 'now') AS INTEGER)
                     - CAST(strftime('%s', intent.recorded_at) AS INTEGER)
             FROM experiment_journal AS intent
             JOIN experiment_journal AS snapshot
                 ON snapshot.experiment_id = intent.experiment_id
                AND snapshot.stage = 'snapshot'
             WHERE intent.stage = 'apply-intent'
               AND intent.provider_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM experiment_journal AS terminal
                   WHERE terminal.experiment_id = intent.experiment_id
                     AND terminal.stage IN ('completed', 'failed')
               )
             ORDER BY intent.experiment_id",
        )?;
        let rows = statement.query_map(params![provider_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut experiments = Vec::new();
        for row in rows {
            let (experiment_id, snapshot_payload, intent_payload, age_seconds) = row?;
            let snapshot: StateSnapshot = serde_json::from_str(&snapshot_payload)?;
            let request: ChangeRequest = serde_json::from_str(&intent_payload)?;
            let lease_expired = lease_expired(age_seconds, request.lease_seconds.get());
            experiments.push(UnclosedExperiment {
                experiment_id,
                provider_id: snapshot.provider_id.clone(),
                snapshot,
                request,
                lease_expired,
            });
        }
        Ok(experiments)
    }

    /// Records a verified restore and closes the experiment with a terminal
    /// `failed` record in a single transaction, so the audit trail is never
    /// left half written.
    pub(crate) fn record_success(
        &mut self,
        experiment_id: i64,
        provider_id: &str,
        reason: ReclaimReason,
        snapshot: &StateSnapshot,
    ) -> Result<(), WatchdogError> {
        let restore_payload = serde_json::to_string(&json!({
            "reason": reason,
            "restored": true,
            "snapshot": snapshot,
        }))?;
        let terminal_payload = serde_json::to_string(&json!({
            "kind": "watchdog-reclaimed",
            "stage": "watchdog",
            "reason": reason,
            "error": reason.terminal_message(),
        }))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            INSERT_RECORD,
            params![
                experiment_id,
                "watchdog-restore",
                provider_id,
                restore_payload
            ],
        )?;
        transaction.execute(
            INSERT_RECORD,
            params![experiment_id, "failed", provider_id, terminal_payload],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records a failed restore attempt without a terminal record, leaving the
    /// experiment unclosed so a later pass retries it.
    pub(crate) fn record_failure(
        &mut self,
        experiment_id: i64,
        provider_id: &str,
        reason: ReclaimReason,
        snapshot: &StateSnapshot,
        error: &str,
    ) -> Result<(), WatchdogError> {
        let payload = serde_json::to_string(&json!({
            "reason": reason,
            "restored": false,
            "error": error,
            "snapshot": snapshot,
        }))?;
        self.connection.execute(
            INSERT_RECORD,
            params![experiment_id, "watchdog-restore", provider_id, payload],
        )?;
        Ok(())
    }

    fn journal_ready(&self) -> Result<bool, WatchdogError> {
        let tables: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'experiment_journal'",
            [],
            |row| row.get(0),
        )?;
        Ok(tables > 0)
    }
}

/// Decides whether a lease has elapsed given the journal-computed age of the
/// apply intent. A missing or negative age (clock skew or an unparseable
/// timestamp) fails safe to "not expired" so recovery relies on the explicit
/// [`ReclaimPolicy::AllUnclosed`] pass rather than acting on a bad clock.
fn lease_expired(age_seconds: Option<i64>, lease_seconds: u64) -> bool {
    match age_seconds {
        Some(age) if age >= 0 => u64::try_from(age).unwrap_or(u64::MAX) >= lease_seconds,
        _ => false,
    }
}
