//! Reference sidecar used to validate the provider lifecycle without hardware.

use std::num::NonZeroU64;

use fpsmaxxing_contracts::ChangeRequest;
use fpsmaxxing_mock_provider::MockProvider;
use fpsmaxxing_provider_sdk::Provider;
use serde_json::json;

fn main() -> Result<(), fpsmaxxing_provider_sdk::ProviderError> {
    let mut provider = MockProvider::new(0);
    let snapshot = provider.snapshot()?;
    let request = ChangeRequest {
        capability_id: "mock.value".to_owned(),
        parameters: json!({ "value": 1 }),
        lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
    };
    provider.preview(&request)?;
    provider.apply(&request)?;
    assert!(provider.verify(&request)?);
    provider.rollback(&snapshot)?;
    println!("{}", provider.manifest().id);
    Ok(())
}
