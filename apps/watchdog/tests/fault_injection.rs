//! Fault-injection tests for the independent watchdog restore path.
//!
//! These tests reconstruct the on-disk journal state a crash leaves behind (an
//! `apply-intent` record with no terminal outcome) and drive the watchdog with
//! no gateway, experiment runner, or LLM in the loop, exactly as the safety
//! invariants require. The journal schema is created by the real control plane
//! so the watchdog is always exercised against the production table shape, and
//! one end-to-end test runs a full control-plane lifecycle to prove the
//! watchdog never disturbs a cleanly closed experiment.

use std::{
    num::NonZeroU32,
    num::NonZeroU64,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::Command,
};

use fpsmaxxing_contracts::{
    CapabilityDescriptor, ChangeRequest, Persistence, ProviderManifest, RiskClass, StateSnapshot,
};
use fpsmaxxing_control_plane::ControlPlane;
use fpsmaxxing_mock_provider::MockProvider;
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use fpsmaxxing_watchdog::{ReclaimPolicy, ReclaimReason, Watchdog};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const INSERT: &str =
    "INSERT INTO experiment_journal (experiment_id, recorded_at, stage, provider_id, payload)
     VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?2), ?3, ?4, ?5)";

/// A crash-leaked experiment to reconstruct in the journal: a snapshot and an
/// `apply-intent` record with no terminal outcome.
struct Leaked {
    experiment_id: i64,
    provider_id: &'static str,
    snapshot_value: u64,
    request_value: u64,
    lease_seconds: u64,
    /// `SQLite` time modifier applied to the intent timestamp, e.g. `-3600
    /// seconds` to age the lease past its deadline or `-1 seconds` to keep it
    /// live.
    intent_age: &'static str,
}

fn journal_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("journal.sqlite")
}

/// Creates the durable journal with the production schema by opening (and
/// dropping) a real control plane, so tests never duplicate the DDL.
fn create_journal(path: &Path) {
    let _plane = ControlPlane::open(Box::new(MockProvider::new(0)), path)
        .expect("control plane should create the journal schema");
}

/// Reconstructs a crash-leaked experiment directly in the journal.
fn seed_leaked(conn: &Connection, leaked: &Leaked) {
    let snapshot = StateSnapshot {
        provider_id: leaked.provider_id.to_owned(),
        state: json!({ "value": leaked.snapshot_value }),
    };
    let request = ChangeRequest {
        capability_id: "mock.value".to_owned(),
        parameters: json!({ "value": leaked.request_value }),
        lease_seconds: NonZeroU64::new(leaked.lease_seconds).expect("lease is non-zero"),
    };
    conn.execute(
        INSERT,
        params![
            leaked.experiment_id,
            "-1 seconds",
            "snapshot",
            leaked.provider_id,
            serde_json::to_string(&snapshot).expect("snapshot serializes"),
        ],
    )
    .expect("snapshot record inserts");
    conn.execute(
        INSERT,
        params![
            leaked.experiment_id,
            leaked.intent_age,
            "apply-intent",
            leaked.provider_id,
            serde_json::to_string(&request).expect("request serializes"),
        ],
    )
    .expect("apply-intent record inserts");
}

fn terminal_count(conn: &Connection, experiment_id: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM experiment_journal
         WHERE experiment_id = ?1 AND stage IN ('completed', 'failed')",
        params![experiment_id],
        |row| row.get(0),
    )
    .expect("terminal count query succeeds")
}

fn stage_count(conn: &Connection, experiment_id: i64, stage: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM experiment_journal WHERE experiment_id = ?1 AND stage = ?2",
        params![experiment_id, stage],
        |row| row.get(0),
    )
    .expect("stage count query succeeds")
}

#[test]
fn crash_mid_apply_restores_defaults() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    // Snapshot baseline 7, but the change pushed the value to 99 before the
    // owner died with the lease still live.
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 7,
            request_value: 99,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    // The watchdog starts from the leaked value; recovery must return it to 7.
    let mut watchdog = Watchdog::open(MockProvider::new(99), &path).expect("watchdog opens");

    let candidates = watchdog.scan().expect("scan succeeds");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].experiment_id, 1);
    assert!(
        !candidates[0].lease_expired,
        "a live lease is recovered as a crash, not an expiry"
    );

    let restorations = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("reclaim succeeds");
    assert_eq!(restorations.len(), 1);
    assert!(restorations[0].restored);
    assert_eq!(restorations[0].reason, ReclaimReason::CrashRecovery);
    assert_eq!(watchdog.provider().value(), 7, "defaults are restored");
    assert_eq!(
        terminal_count(&conn, 1),
        1,
        "the restored experiment is closed with a terminal record"
    );
    assert_eq!(stage_count(&conn, 1, "watchdog-restore"), 1);
}

#[test]
fn expired_lease_is_reclaimed_by_the_steady_state_poll() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 5,
            request_value: 80,
            lease_seconds: 30,
            intent_age: "-3600 seconds",
        },
    );

    let mut watchdog = Watchdog::open(MockProvider::new(80), &path).expect("watchdog opens");
    let candidates = watchdog.scan().expect("scan succeeds");
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].lease_expired, "an aged lease has expired");

    let restorations = watchdog
        .reclaim(ReclaimPolicy::ExpiredLeasesOnly)
        .expect("reclaim succeeds");
    assert_eq!(restorations.len(), 1);
    assert!(restorations[0].restored);
    assert_eq!(restorations[0].reason, ReclaimReason::LeaseExpired);
    assert_eq!(restorations[0].lease_seconds, 30);
    assert_eq!(watchdog.provider().value(), 5);
    assert_eq!(terminal_count(&conn, 1), 1);
}

#[test]
fn expired_leases_only_leaves_live_experiments_for_their_owner() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    // Experiment 1: lease expired. Experiment 2: still inside its lease.
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 3,
            request_value: 60,
            lease_seconds: 30,
            intent_age: "-3600 seconds",
        },
    );
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 2,
            provider_id: "mock",
            snapshot_value: 4,
            request_value: 70,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    let mut watchdog = Watchdog::open(MockProvider::new(60), &path).expect("watchdog opens");
    let restorations = watchdog
        .reclaim(ReclaimPolicy::ExpiredLeasesOnly)
        .expect("reclaim succeeds");

    assert_eq!(restorations.len(), 1, "only the expired lease is reclaimed");
    assert_eq!(restorations[0].experiment_id, 1);
    assert_eq!(terminal_count(&conn, 1), 1, "expired experiment is closed");
    assert_eq!(
        terminal_count(&conn, 2),
        0,
        "the live experiment is left for its owner"
    );
    assert_eq!(
        stage_count(&conn, 2, "watchdog-restore"),
        0,
        "the watchdog does not even record a restore for a live lease"
    );
}

#[test]
fn reclaim_is_idempotent_and_does_not_double_rollback() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 7,
            request_value: 99,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    let mut watchdog = Watchdog::open(MockProvider::new(99), &path).expect("watchdog opens");
    let first = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("first reclaim succeeds");
    assert_eq!(first.len(), 1);
    assert!(first[0].restored);
    assert_eq!(watchdog.provider().value(), 7);

    let second = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("second reclaim succeeds");
    assert!(
        second.is_empty(),
        "a closed experiment is never reclaimed again"
    );
    assert_eq!(
        stage_count(&conn, 1, "watchdog-restore"),
        1,
        "no second restore record is written"
    );
    assert_eq!(
        terminal_count(&conn, 1),
        1,
        "no second terminal record is written"
    );
}

#[test]
fn closed_experiments_are_never_touched() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    // A full, real control-plane lifecycle that completes cleanly.
    let mut plane = ControlPlane::open(Box::new(MockProvider::new(7)), &path).expect("plane opens");
    let request = ChangeRequest {
        capability_id: "mock.value".to_owned(),
        parameters: json!({ "value": 42 }),
        lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
    };
    let result = plane.run_lifecycle(&request).expect("lifecycle succeeds");
    assert!(result.verified && result.rolled_back);
    drop(plane);

    let mut watchdog = Watchdog::open(MockProvider::new(7), &path).expect("watchdog opens");
    assert!(
        watchdog.scan().expect("scan succeeds").is_empty(),
        "a completed experiment is not a candidate"
    );
    assert!(
        watchdog
            .reclaim(ReclaimPolicy::AllUnclosed)
            .expect("reclaim succeeds")
            .is_empty(),
        "the watchdog does nothing to a cleanly closed experiment"
    );
}

#[test]
fn other_providers_experiments_are_ignored() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "other-provider",
            snapshot_value: 1,
            request_value: 50,
            lease_seconds: 30,
            intent_age: "-3600 seconds",
        },
    );

    let mut watchdog = Watchdog::open(MockProvider::new(50), &path).expect("watchdog opens");
    assert!(
        watchdog.scan().expect("scan succeeds").is_empty(),
        "only knobs this provider owns are in scope"
    );
    assert!(
        watchdog
            .reclaim(ReclaimPolicy::AllUnclosed)
            .expect("reclaim succeeds")
            .is_empty()
    );
    assert_eq!(
        terminal_count(&conn, 1),
        0,
        "another provider's experiment is left untouched"
    );
}

#[test]
fn failed_rollback_leaves_the_experiment_for_a_later_pass() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 7,
            request_value: 99,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    // The provider rejects its first rollback, then recovers.
    let provider = FaultyProvider {
        value: 99,
        fail_rollbacks: 1,
        corrupt_rollbacks: 0,
    };
    let mut watchdog = Watchdog::open(provider, &path).expect("watchdog opens");

    let first = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("reclaim reports per-experiment failures rather than erroring");
    assert_eq!(first.len(), 1);
    assert!(!first[0].restored, "the rollback failed");
    assert!(first[0].error.is_some());
    assert_eq!(
        terminal_count(&conn, 1),
        0,
        "a failed restore leaves the experiment unclosed"
    );

    let second = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("second reclaim succeeds");
    assert_eq!(second.len(), 1);
    assert!(second[0].restored, "the retry restores state");
    assert_eq!(watchdog.provider().value, 7);
    assert_eq!(terminal_count(&conn, 1), 1);
}

#[test]
fn corrupted_rollback_is_detected_by_the_verify_probe() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 7,
            request_value: 99,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    // The provider claims success but leaves the wrong value the first time.
    let provider = FaultyProvider {
        value: 99,
        fail_rollbacks: 0,
        corrupt_rollbacks: 1,
    };
    let mut watchdog = Watchdog::open(provider, &path).expect("watchdog opens");

    let first = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("reclaim succeeds");
    assert_eq!(first.len(), 1);
    assert!(
        !first[0].restored,
        "the post-rollback probe rejects the mismatch"
    );
    assert_eq!(terminal_count(&conn, 1), 0);

    let second = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("second reclaim succeeds");
    assert!(second[0].restored);
    assert_eq!(watchdog.provider().value, 7);
    assert_eq!(terminal_count(&conn, 1), 1);
}

#[test]
fn binary_once_reclaims_and_closes_a_leaked_experiment() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    create_journal(&path);
    let conn = Connection::open(&path).expect("journal opens");
    seed_leaked(
        &conn,
        &Leaked {
            experiment_id: 1,
            provider_id: "mock",
            snapshot_value: 7,
            request_value: 99,
            lease_seconds: 300,
            intent_age: "-1 seconds",
        },
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fpsmaxxing-watchdog"))
        .args([
            "--once",
            "--recover-all",
            "--journal",
            path.to_str().expect("path is valid UTF-8"),
        ])
        .output()
        .expect("the watchdog binary runs");
    assert!(output.status.success(), "the single pass exits cleanly");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(
        stdout.contains("restored experiment 1"),
        "the binary reports the restore, got: {stdout}"
    );
    assert_eq!(
        terminal_count(&conn, 1),
        1,
        "the binary closes the leaked experiment"
    );
}

#[test]
fn gateway_termination_mid_apply_is_recovered() {
    let dir = TempDir::new().expect("temp dir");
    let path = journal_path(&dir);
    // Drive a real control-plane lifecycle that dies during apply, after the
    // write-ahead apply-intent is committed but before any terminal record.
    {
        let mut plane =
            ControlPlane::open(Box::new(PanicOnApply), &path).expect("control plane opens");
        let request = ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 99 }),
            lease_seconds: NonZeroU64::new(120).expect("lease is non-zero"),
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| plane.run_lifecycle(&request)));
        assert!(
            outcome.is_err(),
            "the provider terminated the process mid-apply"
        );
    }

    let conn = Connection::open(&path).expect("journal opens");
    assert_eq!(
        terminal_count(&conn, 1),
        0,
        "termination left the experiment unclosed"
    );

    // A fresh watchdog, with no surviving gateway state, recovers the leak.
    let mut watchdog = Watchdog::open(MockProvider::new(99), &path).expect("watchdog opens");
    let restorations = watchdog
        .reclaim(ReclaimPolicy::AllUnclosed)
        .expect("reclaim succeeds");
    assert_eq!(restorations.len(), 1);
    assert!(restorations[0].restored);
    assert_eq!(
        watchdog.provider().value(),
        7,
        "state is restored after gateway termination"
    );
    assert_eq!(terminal_count(&conn, 1), 1);
}

/// A provider that dies during apply, after the control plane has captured its
/// baseline snapshot and committed the write-ahead apply-intent.
struct PanicOnApply;

impl Provider for PanicOnApply {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: "mock".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec![std::env::consts::OS.to_owned()],
            capabilities: vec![CapabilityDescriptor {
                id: "mock.value".to_owned(),
                description: "Terminates mid-apply for watchdog tests".to_owned(),
                risk: RiskClass::Reversible,
                persistence: Persistence::Leased,
                input_schema: json!({ "type": "object" }),
            }],
        }
    }

    fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
        Ok(StateSnapshot {
            provider_id: "mock".to_owned(),
            state: json!({ "value": 7 }),
        })
    }

    fn preview(&self, _: &ChangeRequest) -> Result<String, ProviderError> {
        Ok("set mock.value to 99".to_owned())
    }

    fn apply(&mut self, _: &ChangeRequest) -> Result<(), ProviderError> {
        panic!("simulated process termination during apply");
    }

    fn verify(&self, _: &ChangeRequest) -> Result<bool, ProviderError> {
        Ok(true)
    }

    fn rollback(&mut self, _: &StateSnapshot) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// A provider that can be told to fail or silently corrupt its rollback a fixed
/// number of times, then behave. Fault counters live behind `&mut self` because
/// the watchdog owns one provider across reclaim passes.
struct FaultyProvider {
    value: u64,
    fail_rollbacks: u32,
    corrupt_rollbacks: u32,
}

impl Provider for FaultyProvider {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: "mock".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec![std::env::consts::OS.to_owned()],
            capabilities: vec![CapabilityDescriptor {
                id: "mock.value".to_owned(),
                description: "Injectable in-memory value for watchdog tests".to_owned(),
                risk: RiskClass::Reversible,
                persistence: Persistence::Leased,
                input_schema: json!({ "type": "object" }),
            }],
        }
    }

    fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
        Ok(StateSnapshot {
            provider_id: "mock".to_owned(),
            state: json!({ "value": self.value }),
        })
    }

    fn preview(&self, _: &ChangeRequest) -> Result<String, ProviderError> {
        Ok("no-op preview".to_owned())
    }

    fn apply(&mut self, request: &ChangeRequest) -> Result<(), ProviderError> {
        self.value = request
            .parameters
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::InvalidRequest("value must be a u64".to_owned()))?;
        Ok(())
    }

    fn verify(&self, request: &ChangeRequest) -> Result<bool, ProviderError> {
        Ok(request.parameters.get("value").and_then(Value::as_u64) == Some(self.value))
    }

    fn rollback(&mut self, snapshot: &StateSnapshot) -> Result<(), ProviderError> {
        if self.fail_rollbacks > 0 {
            self.fail_rollbacks -= 1;
            return Err(ProviderError::RollbackFailed("injected fault".to_owned()));
        }
        self.value = snapshot
            .state
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::RollbackFailed("snapshot has no value".to_owned()))?;
        if self.corrupt_rollbacks > 0 {
            self.corrupt_rollbacks -= 1;
            self.value += 1;
        }
        Ok(())
    }
}
