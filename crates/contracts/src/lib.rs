//! Versioned data contracts shared by `FPSMaxxing` applications and sidecars.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The safety classification attached to a capability or requested change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClass {
    /// Observation only; no system state changes.
    ReadOnly,
    /// Temporary and mechanically reversible without a reboot.
    Reversible,
    /// Requires an explicit approval policy before application.
    ApprovalRequired,
    /// Intentionally unavailable to autonomous agents.
    Denied,
}

/// How long a capability's effect can survive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Persistence {
    /// The operation does not change state.
    None,
    /// The operation expires with its lease or provider process.
    Leased,
    /// The operation remains until it is explicitly reverted.
    Persistent,
    /// The operation requires a reboot to become active.
    RebootRequired,
}

/// A semantic operation advertised by a provider.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CapabilityDescriptor {
    /// Stable capability name, such as `process.cpu-affinity`.
    pub id: String,
    /// Human-readable summary for operators and agents.
    pub description: String,
    /// Safety class enforced by the policy engine.
    pub risk: RiskClass,
    /// Persistence behavior of successful changes.
    pub persistence: Persistence,
    /// JSON Schema describing accepted parameters.
    pub input_schema: Value,
}

/// Provider identity and the capabilities it advertises.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ProviderManifest {
    /// Stable provider identifier.
    pub id: String,
    /// Provider protocol version.
    pub protocol_version: u32,
    /// Targets supported by this build.
    pub targets: Vec<String>,
    /// Semantic capabilities exposed by the provider.
    pub capabilities: Vec<CapabilityDescriptor>,
}

/// A requested provider change after policy validation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ChangeRequest {
    /// Capability being invoked.
    pub capability_id: String,
    /// Capability-specific parameters.
    pub parameters: Value,
    /// Automatic rollback deadline in seconds.
    pub lease_seconds: u64,
}

/// Opaque provider state captured before a change.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct StateSnapshot {
    /// Provider that produced the snapshot.
    pub provider_id: String,
    /// Provider-specific state required for rollback.
    pub state: Value,
}

#[cfg(test)]
mod tests {
    use super::{Persistence, RiskClass};

    #[test]
    fn risk_classes_are_not_implicitly_ordered() {
        assert_ne!(RiskClass::ReadOnly, RiskClass::Reversible);
        assert_ne!(Persistence::Leased, Persistence::Persistent);
    }
}
