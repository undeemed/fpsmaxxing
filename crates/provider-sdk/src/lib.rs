//! Minimal provider lifecycle used by native sidecars.

use fpsmaxxing_contracts::{ChangeRequest, ProviderManifest, StateSnapshot};
use thiserror::Error;

/// Errors returned through the provider boundary.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The requested capability is not implemented by this provider.
    #[error("unsupported capability: {0}")]
    UnsupportedCapability(String),
    /// The provider rejected invalid or unsafe input.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The external service or vendor API is unavailable.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// Rollback could not restore the captured state.
    #[error("rollback failed: {0}")]
    RollbackFailed(String),
}

/// Contract implemented by every provider sidecar.
pub trait Provider {
    /// Returns provider identity, supported targets, and capabilities.
    fn manifest(&self) -> ProviderManifest;

    /// Captures the exact state needed to reverse the next operation.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the external provider is unavailable or
    /// cannot produce a complete rollback snapshot.
    fn snapshot(&self) -> Result<StateSnapshot, ProviderError>;

    /// Applies a previously policy-validated request.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the capability is unsupported, input is
    /// invalid, or the external provider rejects the requested change.
    fn apply(&mut self, request: &ChangeRequest) -> Result<(), ProviderError>;

    /// Confirms that the requested effect is observable.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the provider state cannot be observed.
    fn verify(&self, request: &ChangeRequest) -> Result<bool, ProviderError>;

    /// Restores a state captured by [`Provider::snapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the snapshot is invalid or restoration
    /// fails.
    fn rollback(&mut self, snapshot: &StateSnapshot) -> Result<(), ProviderError>;
}
