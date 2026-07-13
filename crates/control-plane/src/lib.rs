//! Capability registry, policy seam, broker lifecycle, and durable journal.

use std::{num::NonZeroU64, path::Path};

use fpsmaxxing_contracts::{ChangeRequest, ProviderManifest, RiskClass};
use fpsmaxxing_mock_provider::MockProvider;
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
    provider: MockProvider,
    manifest: ProviderManifest,
    journal: Connection,
}

impl ControlPlane {
    /// Opens a control plane with a durable `SQLite` experiment journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be opened or initialized.
    pub fn open(journal_path: impl AsRef<Path>) -> Result<Self, ControlPlaneError> {
        let journal = Connection::open(journal_path)?;
        journal.execute_batch(
            "CREATE TABLE IF NOT EXISTS experiment_journal (
                sequence INTEGER PRIMARY KEY,
                stage TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                payload TEXT NOT NULL
            );",
        )?;
        let provider = MockProvider::new(0);
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

    /// Executes all provider lifecycle stages and always restores the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when policy, the provider, or the journal rejects a stage.
    pub fn run_lifecycle(
        &mut self,
        request: &ChangeRequest,
    ) -> Result<LifecycleResult, ControlPlaneError> {
        self.validate(request)?;
        let snapshot = self.provider.snapshot()?;
        self.record("snapshot", &snapshot)?;
        let preview = self.provider.preview(request)?;
        self.record("preview", &json!({ "description": preview }))?;
        self.provider.apply(request)?;
        self.record("apply", &request)?;
        let verified = self.provider.verify(request)?;
        self.record("verify", &json!({ "verified": verified }))?;
        if !verified {
            return Err(ControlPlaneError::PolicyDenied(
                "verification probe failed".to_owned(),
            ));
        }
        self.provider.rollback(&snapshot)?;
        self.record("rollback", &snapshot)?;
        let restored = self.provider.snapshot()?;
        let rolled_back = restored == snapshot;
        self.record("rollback-verify", &json!({ "restored": rolled_back }))?;
        if !rolled_back {
            return Err(ControlPlaneError::PolicyDenied(
                "rollback verification failed".to_owned(),
            ));
        }
        Ok(LifecycleResult {
            provider_id: self.manifest.id.clone(),
            preview,
            verified,
            rolled_back,
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

    fn record(&self, stage: &str, payload: &impl Serialize) -> Result<(), ControlPlaneError> {
        self.journal.execute(
            "INSERT INTO experiment_journal (stage, provider_id, payload) VALUES (?1, ?2, ?3)",
            params![stage, self.manifest.id, serde_json::to_string(payload)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_proves_ordered_lifecycle_and_rollback() {
        let mut plane = ControlPlane::open(":memory:").expect("journal should open");
        let result = plane
            .run_lifecycle(&ChangeRequest {
                capability_id: "mock.value".to_owned(),
                parameters: json!({ "value": 42 }),
                lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
            })
            .expect("lifecycle should succeed");
        assert!(result.verified && result.rolled_back);
        assert_eq!(
            plane.journal_stages().expect("journal should read"),
            [
                "snapshot",
                "preview",
                "apply",
                "verify",
                "rollback",
                "rollback-verify"
            ]
        );
    }
}
