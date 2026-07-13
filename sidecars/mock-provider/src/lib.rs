//! In-memory provider used by Linux-safe control-plane tests.

use std::num::NonZeroU32;

use fpsmaxxing_contracts::{
    CapabilityDescriptor, ChangeRequest, Persistence, ProviderManifest, RiskClass, StateSnapshot,
};
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use serde_json::{Value, json};

/// A deterministic provider that owns one reversible integer setting.
pub struct MockProvider {
    value: u64,
}

impl MockProvider {
    /// Creates a provider with a known baseline value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self { value }
    }

    /// Returns the current value for assertions in integration seams.
    #[must_use]
    pub const fn value(&self) -> u64 {
        self.value
    }
}

impl Provider for MockProvider {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: "mock".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec![std::env::consts::OS.to_owned()],
            capabilities: vec![CapabilityDescriptor {
                id: "mock.value".to_owned(),
                description: "Sets an in-memory value for contract testing".to_owned(),
                risk: RiskClass::Reversible,
                persistence: Persistence::Leased,
                input_schema: json!({
                    "type": "object",
                    "required": ["value"],
                    "properties": { "value": { "type": "integer", "minimum": 0 } }
                }),
            }],
        }
    }

    fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
        Ok(StateSnapshot {
            provider_id: "mock".to_owned(),
            state: json!({ "value": self.value }),
        })
    }

    fn preview(&self, request: &ChangeRequest) -> Result<String, ProviderError> {
        let value = request
            .parameters
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::InvalidRequest("value must be a u64".to_owned()))?;
        Ok(format!("set mock.value from {} to {value}", self.value))
    }

    fn apply(&mut self, request: &ChangeRequest) -> Result<(), ProviderError> {
        if request.capability_id != "mock.value" {
            return Err(ProviderError::UnsupportedCapability(
                request.capability_id.clone(),
            ));
        }
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
        self.value = snapshot
            .state
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::RollbackFailed("snapshot has no value".to_owned()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    #[test]
    fn lifecycle_restores_the_baseline() {
        let mut provider = MockProvider::new(7);
        let snapshot = provider.snapshot().expect("snapshot should succeed");
        let request = ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 42 }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
        };
        assert!(provider.preview(&request).is_ok());
        provider.apply(&request).expect("apply should succeed");
        assert!(provider.verify(&request).expect("verify should succeed"));
        provider
            .rollback(&snapshot)
            .expect("rollback should succeed");
        assert_eq!(provider.value(), 7);
    }
}
