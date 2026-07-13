//! Reference sidecar used to validate the provider lifecycle without hardware.

use std::num::{NonZeroU32, NonZeroU64};

use fpsmaxxing_contracts::{
    CapabilityDescriptor, ChangeRequest, Persistence, ProviderManifest, RiskClass, StateSnapshot,
};
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use serde_json::{Value, json};

struct MockProvider {
    value: u64,
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

fn main() -> Result<(), ProviderError> {
    let mut provider = MockProvider { value: 0 };
    let snapshot = provider.snapshot()?;
    let request = ChangeRequest {
        capability_id: "mock.value".to_owned(),
        parameters: json!({ "value": 1 }),
        lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
    };

    provider.apply(&request)?;
    assert!(provider.verify(&request)?);
    provider.rollback(&snapshot)?;
    println!("{}", provider.manifest().id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_verify_and_rollback() {
        let mut provider = MockProvider { value: 7 };
        let snapshot = provider.snapshot().expect("snapshot should succeed");
        let request = ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 42 }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
        };

        provider.apply(&request).expect("apply should succeed");
        assert!(provider.verify(&request).expect("verify should succeed"));
        provider
            .rollback(&snapshot)
            .expect("rollback should succeed");
        assert_eq!(provider.value, 7);
    }
}
