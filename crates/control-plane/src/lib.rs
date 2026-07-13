//! Capability registry, policy seam, broker lifecycle, and durable journal.

use std::{num::NonZeroU64, path::Path};

use fpsmaxxing_contracts::{ChangeRequest, ProviderManifest, RiskClass, StateSnapshot};
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

/// Fail-closed errors from the broker seam.
#[derive(Debug, Error)]
pub enum ControlPlaneError {
    /// A caller requested a capability that is not advertised by a provider.
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    /// A request does not satisfy the bounded alpha policy.
    #[error("policy denied request: {0}")]
    PolicyDenied(String),
    /// A provider could not complete its lifecycle action.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The durable journal could not be read or written.
    #[error(transparent)]
    Journal(#[from] rusqlite::Error),
    /// A journal record could not be encoded.
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

/// The complete, auditable outcome of a temporary experiment.
#[derive(Debug, Serialize)]
pub struct LifecycleResult {
    /// Provider that owned the change.
    pub provider_id: String,
    /// Human-readable preview produced before the write.
    pub preview: String,
    /// Whether the requested value was observed after apply.
    pub verified: bool,
    /// Whether the captured baseline was restored before returning.
    pub rolled_back: bool,
}

/// Registry and broker used by the unprivileged gateway on the mock path.
pub struct ControlPlane {
    provider: Box<dyn Provider>,
    manifest: ProviderManifest,
    journal: Connection,
}

impl ControlPlane {
    /// Opens a control plane for one accepted provider with a durable `SQLite`
    /// experiment journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider does not target this platform or the
    /// journal cannot be opened or initialized.
    pub fn open(
        provider: Box<dyn Provider>,
        journal_path: impl AsRef<Path>,
    ) -> Result<Self, ControlPlaneError> {
        let manifest = provider.manifest();
        if !manifest
            .targets
            .iter()
            .any(|target| target == std::env::consts::OS)
        {
            return Err(ControlPlaneError::PolicyDenied(
                "unknown platform target".to_owned(),
            ));
        }
        let journal = Connection::open(journal_path)?;
        journal.execute_batch(
            "CREATE TABLE IF NOT EXISTS experiment_journal (
                sequence INTEGER PRIMARY KEY,
                experiment_id INTEGER NOT NULL,
                recorded_at TEXT NOT NULL,
                stage TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                payload TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            provider,
            manifest,
            journal,
        })
    }

    /// Lists typed capabilities that the registry has accepted for this host.
    #[must_use]
    pub fn capabilities(&self) -> &ProviderManifest {
        &self.manifest
    }

    /// Executes all provider lifecycle stages, restoring the snapshot even
    /// when apply, verification, or journaling fails after the snapshot is
    /// captured.
    ///
    /// # Errors
    ///
    /// Returns an error when policy, the provider, or the journal rejects a
    /// stage; once apply has been attempted, rollback runs before any error
    /// is returned, and a failed restore takes precedence over other
    /// failures.
    pub fn run_lifecycle(
        &mut self,
        request: &ChangeRequest,
    ) -> Result<LifecycleResult, ControlPlaneError> {
        self.validate(request)?;
        let experiment = self.next_experiment_id()?;
        let snapshot = self.provider.snapshot()?;
        self.record(experiment, "snapshot", &snapshot)?;
        let preview = self.provider.preview(request)?;
        self.record(experiment, "preview", &json!({ "description": preview }))?;
        let observed = self
            .provider
            .apply(request)
            .map_err(ControlPlaneError::from)
            .and_then(|()| self.observe_applied(experiment, request));
        self.restore(experiment, &snapshot)?;
        let verified = observed?;
        Ok(LifecycleResult {
            provider_id: self.manifest.id.clone(),
            preview,
            verified,
            rolled_back: true,
        })
    }

    /// Reads journal stage names in write order for diagnostics and tests.
    ///
    /// # Errors
    ///
    /// Returns an error if the durable journal cannot be queried.
    pub fn journal_stages(&self) -> Result<Vec<String>, ControlPlaneError> {
        let mut statement = self
            .journal
            .prepare("SELECT stage FROM experiment_journal ORDER BY sequence")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(ControlPlaneError::from)
    }

    fn validate(&self, request: &ChangeRequest) -> Result<(), ControlPlaneError> {
        let capability = self
            .manifest
            .capabilities
            .iter()
            .find(|item| item.id == request.capability_id)
            .ok_or_else(|| ControlPlaneError::UnknownCapability(request.capability_id.clone()))?;
        if capability.risk != RiskClass::Reversible {
            return Err(ControlPlaneError::PolicyDenied(
                "only reversible mock capabilities are enabled".to_owned(),
            ));
        }
        if request.lease_seconds > NonZeroU64::new(300).expect("constant is non-zero") {
            return Err(ControlPlaneError::PolicyDenied(
                "lease exceeds 300 seconds".to_owned(),
            ));
        }
        let value = request
            .parameters
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ControlPlaneError::PolicyDenied("mock.value requires an unsigned value".to_owned())
            })?;
        if value > 100 {
            return Err(ControlPlaneError::PolicyDenied(
                "mock.value is bounded to 0..=100".to_owned(),
            ));
        }
        Ok(())
    }

    fn observe_applied(
        &mut self,
        experiment: i64,
        request: &ChangeRequest,
    ) -> Result<bool, ControlPlaneError> {
        self.record(experiment, "apply", request)?;
        let verified = self.provider.verify(request)?;
        self.record(experiment, "verify", &json!({ "verified": verified }))?;
        if verified {
            Ok(true)
        } else {
            Err(ControlPlaneError::PolicyDenied(
                "verification probe failed".to_owned(),
            ))
        }
    }

    fn restore(
        &mut self,
        experiment: i64,
        snapshot: &StateSnapshot,
    ) -> Result<(), ControlPlaneError> {
        self.provider.rollback(snapshot)?;
        self.record(experiment, "rollback", snapshot)?;
        let restored = self.provider.snapshot()?;
        let rolled_back = restored == *snapshot;
        self.record(
            experiment,
            "rollback-verify",
            &json!({ "restored": rolled_back }),
        )?;
        if rolled_back {
            Ok(())
        } else {
            Err(ControlPlaneError::PolicyDenied(
                "rollback verification failed".to_owned(),
            ))
        }
    }

    fn next_experiment_id(&self) -> Result<i64, ControlPlaneError> {
        self.journal
            .query_row(
                "SELECT COALESCE(MAX(experiment_id), 0) + 1 FROM experiment_journal",
                [],
                |row| row.get(0),
            )
            .map_err(ControlPlaneError::from)
    }

    fn record(
        &self,
        experiment: i64,
        stage: &str,
        payload: &impl Serialize,
    ) -> Result<(), ControlPlaneError> {
        self.journal.execute(
            "INSERT INTO experiment_journal (experiment_id, recorded_at, stage, provider_id, payload)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?2, ?3, ?4)",
            params![
                experiment,
                stage,
                self.manifest.id,
                serde_json::to_string(payload)?
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use fpsmaxxing_contracts::{CapabilityDescriptor, Persistence};

    use super::*;

    struct FakeProvider {
        value: u64,
        fail_verification: bool,
        fail_apply: bool,
    }

    impl Provider for FakeProvider {
        fn manifest(&self) -> ProviderManifest {
            ProviderManifest {
                id: "fake".to_owned(),
                protocol_version: NonZeroU32::MIN,
                targets: vec![std::env::consts::OS.to_owned()],
                capabilities: vec![CapabilityDescriptor {
                    id: "mock.value".to_owned(),
                    description: "Sets an in-memory value for lifecycle tests".to_owned(),
                    risk: RiskClass::Reversible,
                    persistence: Persistence::Leased,
                    input_schema: json!({
                        "type": "object",
                        "required": ["value"],
                        "properties": { "value": { "type": "integer", "minimum": 0, "maximum": 100 } }
                    }),
                }],
            }
        }

        fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
            Ok(StateSnapshot {
                provider_id: "fake".to_owned(),
                state: json!({ "value": self.value }),
            })
        }

        fn preview(&self, request: &ChangeRequest) -> Result<String, ProviderError> {
            Ok(format!("set fake value to {}", request.parameters["value"]))
        }

        fn apply(&mut self, request: &ChangeRequest) -> Result<(), ProviderError> {
            self.value = request
                .parameters
                .get("value")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProviderError::InvalidRequest("value must be a u64".to_owned()))?;
            if self.fail_apply {
                return Err(ProviderError::Unavailable(
                    "apply failed after mutating state".to_owned(),
                ));
            }
            Ok(())
        }

        fn verify(&self, request: &ChangeRequest) -> Result<bool, ProviderError> {
            Ok(!self.fail_verification
                && request.parameters.get("value").and_then(Value::as_u64) == Some(self.value))
        }

        fn rollback(&mut self, snapshot: &StateSnapshot) -> Result<(), ProviderError> {
            self.value = snapshot
                .state
                .get("value")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProviderError::RollbackFailed("snapshot has no value".to_owned()))?;
            Ok(())
        }
    }

    const LIFECYCLE_STAGES: [&str; 6] = [
        "snapshot",
        "preview",
        "apply",
        "verify",
        "rollback",
        "rollback-verify",
    ];

    fn request(value: u64) -> ChangeRequest {
        ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": value }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
        }
    }

    fn plane(fail_verification: bool) -> ControlPlane {
        ControlPlane::open(
            Box::new(FakeProvider {
                value: 7,
                fail_verification,
                fail_apply: false,
            }),
            ":memory:",
        )
        .expect("journal should open")
    }

    #[test]
    fn journal_proves_ordered_lifecycle_and_rollback() {
        let mut plane = plane(false);
        let result = plane
            .run_lifecycle(&request(42))
            .expect("lifecycle should succeed");
        assert!(result.verified && result.rolled_back);
        assert_eq!(
            plane.journal_stages().expect("journal should read"),
            LIFECYCLE_STAGES
        );
    }

    #[test]
    fn failed_verification_still_restores_the_snapshot() {
        let mut plane = plane(true);
        let error = plane
            .run_lifecycle(&request(42))
            .expect_err("verification should fail");
        assert!(matches!(error, ControlPlaneError::PolicyDenied(_)));
        assert_eq!(
            plane.journal_stages().expect("journal should read"),
            LIFECYCLE_STAGES
        );
        let restored: String = plane
            .journal
            .query_row(
                "SELECT payload FROM experiment_journal WHERE stage = 'rollback-verify'",
                [],
                |row| row.get(0),
            )
            .expect("rollback-verify record should exist");
        assert_eq!(restored, r#"{"restored":true}"#);
    }

    #[test]
    fn failed_apply_still_restores_the_snapshot() {
        let mut plane = ControlPlane::open(
            Box::new(FakeProvider {
                value: 7,
                fail_verification: false,
                fail_apply: true,
            }),
            ":memory:",
        )
        .expect("journal should open");
        let error = plane
            .run_lifecycle(&request(42))
            .expect_err("apply should fail");
        assert!(matches!(error, ControlPlaneError::Provider(_)));
        assert_eq!(
            plane.journal_stages().expect("journal should read"),
            ["snapshot", "preview", "rollback", "rollback-verify"]
        );
        let restored: String = plane
            .journal
            .query_row(
                "SELECT payload FROM experiment_journal WHERE stage = 'rollback-verify'",
                [],
                |row| row.get(0),
            )
            .expect("rollback-verify record should exist");
        assert_eq!(restored, r#"{"restored":true}"#);
    }

    #[test]
    fn journal_correlates_stages_by_experiment_and_time() {
        let mut plane = plane(false);
        plane
            .run_lifecycle(&request(1))
            .expect("first lifecycle should succeed");
        plane
            .run_lifecycle(&request(2))
            .expect("second lifecycle should succeed");
        let rows = plane
            .journal
            .prepare("SELECT experiment_id, recorded_at FROM experiment_journal ORDER BY sequence")
            .expect("query should prepare")
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query should execute")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows should read");
        assert_eq!(rows.len(), 12);
        assert!(rows[..6].iter().all(|(experiment, _)| *experiment == 1));
        assert!(rows[6..].iter().all(|(experiment, _)| *experiment == 2));
        assert!(rows.iter().all(|(_, recorded_at)| !recorded_at.is_empty()));
    }

    #[test]
    fn unknown_platform_targets_fail_closed() {
        struct ForeignProvider;

        impl Provider for ForeignProvider {
            fn manifest(&self) -> ProviderManifest {
                ProviderManifest {
                    id: "foreign".to_owned(),
                    protocol_version: NonZeroU32::MIN,
                    targets: vec!["not-a-real-os".to_owned()],
                    capabilities: vec![],
                }
            }

            fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
                Err(ProviderError::Unavailable("unreachable".to_owned()))
            }

            fn preview(&self, _: &ChangeRequest) -> Result<String, ProviderError> {
                Err(ProviderError::Unavailable("unreachable".to_owned()))
            }

            fn apply(&mut self, _: &ChangeRequest) -> Result<(), ProviderError> {
                Err(ProviderError::Unavailable("unreachable".to_owned()))
            }

            fn verify(&self, _: &ChangeRequest) -> Result<bool, ProviderError> {
                Err(ProviderError::Unavailable("unreachable".to_owned()))
            }

            fn rollback(&mut self, _: &StateSnapshot) -> Result<(), ProviderError> {
                Err(ProviderError::Unavailable("unreachable".to_owned()))
            }
        }

        let error = ControlPlane::open(Box::new(ForeignProvider), ":memory:")
            .err()
            .expect("foreign targets should be rejected");
        assert!(matches!(error, ControlPlaneError::PolicyDenied(_)));
    }
}
