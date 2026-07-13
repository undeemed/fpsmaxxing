//! Versioned data contracts shared by `FPSMaxxing` applications and sidecars.

use std::num::NonZeroU32;

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ProviderManifest {
    /// Stable provider identifier.
    pub id: String,
    /// Provider protocol version; at least 1, matching the sidecar schema.
    pub protocol_version: NonZeroU32,
    /// Targets supported by this build.
    pub targets: Vec<String>,
    /// Semantic capabilities exposed by the provider.
    pub capabilities: Vec<CapabilityDescriptor>,
}

/// A requested provider change after policy validation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct StateSnapshot {
    /// Provider that produced the snapshot.
    pub provider_id: String,
    /// Provider-specific state required for rollback.
    pub state: Value,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::{CapabilityDescriptor, NonZeroU32, Persistence, ProviderManifest, RiskClass};

    const CAPABILITY_SCHEMA: &str = include_str!("../../../schemas/capability.schema.json");
    const SIDECAR_SCHEMA: &str = include_str!("../../../schemas/sidecar.schema.json");

    fn wire_string(value: impl serde::Serialize) -> String {
        serde_json::to_value(value)
            .expect("serialization should succeed")
            .as_str()
            .expect("enums should serialize to strings")
            .to_owned()
    }

    fn string_set(values: &Value) -> BTreeSet<String> {
        values
            .as_array()
            .expect("schema field should be an array")
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("schema entries should be strings")
                    .to_owned()
            })
            .collect()
    }

    fn sample_capability() -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: "process.cpu-affinity".to_owned(),
            description: "Pins a process to selected cores".to_owned(),
            risk: RiskClass::Reversible,
            persistence: Persistence::Leased,
            input_schema: json!({ "type": "object" }),
        }
    }

    fn sample_manifest() -> ProviderManifest {
        ProviderManifest {
            id: "mock".to_owned(),
            protocol_version: NonZeroU32::MIN,
            targets: vec!["windows".to_owned()],
            capabilities: vec![sample_capability()],
        }
    }

    #[test]
    fn enum_wire_strings_match_capability_schema() {
        assert_eq!(wire_string(RiskClass::ReadOnly), "read-only");
        assert_eq!(wire_string(RiskClass::Reversible), "reversible");
        assert_eq!(
            wire_string(RiskClass::ApprovalRequired),
            "approval-required"
        );
        assert_eq!(wire_string(RiskClass::Denied), "denied");
        assert_eq!(wire_string(Persistence::None), "none");
        assert_eq!(wire_string(Persistence::Leased), "leased");
        assert_eq!(wire_string(Persistence::Persistent), "persistent");
        assert_eq!(wire_string(Persistence::RebootRequired), "reboot-required");

        let schema: Value =
            serde_json::from_str(CAPABILITY_SCHEMA).expect("capability schema should parse");
        assert_eq!(
            string_set(&schema["properties"]["risk"]["enum"]),
            [
                RiskClass::ReadOnly,
                RiskClass::Reversible,
                RiskClass::ApprovalRequired,
                RiskClass::Denied,
            ]
            .map(wire_string)
            .into_iter()
            .collect()
        );
        assert_eq!(
            string_set(&schema["properties"]["persistence"]["enum"]),
            [
                Persistence::None,
                Persistence::Leased,
                Persistence::Persistent,
                Persistence::RebootRequired,
            ]
            .map(wire_string)
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn capability_fields_match_capability_schema() {
        let schema: Value =
            serde_json::from_str(CAPABILITY_SCHEMA).expect("capability schema should parse");
        let serialized =
            serde_json::to_value(sample_capability()).expect("capability should serialize");
        let fields: BTreeSet<String> = serialized
            .as_object()
            .expect("capability should serialize to an object")
            .keys()
            .cloned()
            .collect();

        let properties: BTreeSet<String> = schema["properties"]
            .as_object()
            .expect("schema should declare properties")
            .keys()
            .cloned()
            .collect();
        assert_eq!(fields, properties);
        assert_eq!(fields, string_set(&schema["required"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn manifest_fields_match_sidecar_schema() {
        let schema: Value =
            serde_json::from_str(SIDECAR_SCHEMA).expect("sidecar schema should parse");
        let serialized =
            serde_json::to_value(sample_manifest()).expect("manifest should serialize");
        let fields: BTreeSet<String> = serialized
            .as_object()
            .expect("manifest should serialize to an object")
            .keys()
            .cloned()
            .collect();

        let properties: BTreeSet<String> = schema["properties"]
            .as_object()
            .expect("schema should declare properties")
            .keys()
            .cloned()
            .collect();
        assert_eq!(fields, properties);
        assert_eq!(fields, string_set(&schema["required"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn unknown_fields_are_rejected_like_the_schemas() {
        let mut serialized =
            serde_json::to_value(sample_capability()).expect("capability should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<CapabilityDescriptor>(serialized).is_err());

        let mut serialized =
            serde_json::to_value(sample_manifest()).expect("manifest should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ProviderManifest>(serialized).is_err());
    }

    #[test]
    fn protocol_version_zero_is_rejected_like_the_schema() {
        let schema: Value =
            serde_json::from_str(SIDECAR_SCHEMA).expect("sidecar schema should parse");
        assert_eq!(
            schema["properties"]["protocol_version"]["minimum"],
            json!(1)
        );

        let mut serialized =
            serde_json::to_value(sample_manifest()).expect("manifest should serialize");
        serialized["protocol_version"] = json!(0);
        assert!(serde_json::from_value::<ProviderManifest>(serialized).is_err());
    }

    #[test]
    fn wire_types_round_trip() {
        let manifest = sample_manifest();
        let serialized = serde_json::to_value(&manifest).expect("manifest should serialize");
        let deserialized: ProviderManifest =
            serde_json::from_value(serialized).expect("manifest should deserialize");
        assert_eq!(manifest, deserialized);
    }
}
