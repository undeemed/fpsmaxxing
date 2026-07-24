//! Versioned data contracts shared by `FPSMaxxing` applications and sidecars.

use std::num::{NonZeroU32, NonZeroU64};

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
    /// Automatic rollback deadline in seconds; every mutation carries a
    /// non-zero TTL lease.
    pub lease_seconds: NonZeroU64,
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

/// A single deterministic performance measurement captured during a trial.
///
/// Values are recorded verbatim in the experiment journal so a trial can be
/// re-evaluated later without re-running the workload or consulting chat
/// history.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSample {
    /// Frames per second observed for this sample; higher is better.
    pub fps: f64,
    /// Peak component temperature in degrees Celsius for this sample.
    pub temperature_c: f64,
    /// Board power draw in watts for this sample.
    pub power_w: f64,
    /// Correctness errors detected during this sample; zero is nominal.
    pub errors: u64,
}

/// Deterministic aggregate of one measurement set used by the evaluator.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    /// Number of samples aggregated.
    pub samples: u64,
    /// Mean frames per second across the samples.
    pub mean_fps: f64,
    /// Highest temperature observed across the samples.
    pub max_temperature_c: f64,
    /// Highest power draw observed across the samples.
    pub max_power_w: f64,
    /// Total correctness errors observed across the samples.
    pub total_errors: u64,
}

/// Fixed thresholds the immutable evaluator applies to a trial.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionBounds {
    /// Minimum samples required in each of the baseline and candidate sets
    /// before a promotion can be considered.
    pub min_samples: NonZeroU32,
    /// Minimum mean-FPS gain the candidate must show over the baseline.
    pub min_fps_improvement: f64,
    /// Inclusive ceiling for candidate temperature in degrees Celsius.
    pub max_temperature_c: f64,
    /// Inclusive ceiling for candidate power draw in watts.
    pub max_power_w: f64,
    /// Inclusive ceiling for candidate correctness errors.
    pub max_errors: u64,
}

/// A declarative, typed experiment the runner can execute, journal, and replay.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    /// Human-authored hypothesis the trial tests.
    pub hypothesis: String,
    /// Bounded, policy-checkable capability change under test.
    pub target: ChangeRequest,
    /// Leading measurements discarded from each phase before counting.
    pub warmup_samples: u32,
    /// Counted baseline measurements to record.
    pub baseline_samples: NonZeroU32,
    /// Counted candidate measurements to record.
    pub candidate_samples: NonZeroU32,
    /// Thresholds the evaluator uses to promote or reject.
    pub bounds: DecisionBounds,
}

/// The terminal keep-or-rollback decision for a trial.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decision {
    /// The candidate met every bound and is kept.
    Promote,
    /// The candidate failed a bound and is rolled back.
    Reject,
}

/// The single machine-readable reason behind a [`Verdict`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictReason {
    /// The candidate cleared every bound.
    Promoted,
    /// A measurement set held fewer than the required samples.
    InsufficientSamples,
    /// Candidate temperature exceeded the ceiling.
    TemperatureExceeded,
    /// Candidate power draw exceeded the ceiling.
    PowerExceeded,
    /// Candidate correctness errors exceeded the ceiling.
    ErrorsExceeded,
    /// The candidate did not beat the baseline by the required margin.
    InsufficientImprovement,
}

/// The immutable evaluator's deterministic decision for a trial.
///
/// The verdict carries the aggregates the decision used so a journaled trial
/// is self-describing and re-evaluation can be checked against it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// Whether the candidate is kept or rolled back.
    pub decision: Decision,
    /// The reason behind the decision.
    pub reason: VerdictReason,
    /// Candidate mean-FPS gain over the baseline.
    pub fps_improvement: f64,
    /// Baseline aggregate the decision used.
    pub baseline: MetricSummary,
    /// Candidate aggregate the decision used.
    pub candidate: MetricSummary,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::{
        CapabilityDescriptor, ChangeRequest, Decision, DecisionBounds, ExperimentSpec,
        MetricSummary, NonZeroU32, NonZeroU64, Persistence, ProviderManifest, RiskClass, Verdict,
        VerdictReason,
    };

    const CAPABILITY_SCHEMA: &str = include_str!("../../../schemas/capability.schema.json");
    const SIDECAR_SCHEMA: &str = include_str!("../../../schemas/sidecar.schema.json");
    const EXPERIMENT_SCHEMA: &str = include_str!("../../../schemas/experiment.schema.json");
    const VERDICT_SCHEMA: &str = include_str!("../../../schemas/verdict.schema.json");

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
    fn lease_seconds_zero_is_rejected() {
        let mut serialized = json!({
            "capability_id": "mock.value",
            "parameters": { "value": 1 },
            "lease_seconds": 1
        });
        assert!(serde_json::from_value::<ChangeRequest>(serialized.clone()).is_ok());

        serialized["lease_seconds"] = json!(0);
        assert!(serde_json::from_value::<ChangeRequest>(serialized).is_err());
    }

    #[test]
    fn generated_schemas_match_checked_in_schemas() {
        let cases = [
            (
                schemars::schema_for!(CapabilityDescriptor),
                serde_json::from_str::<Value>(CAPABILITY_SCHEMA)
                    .expect("capability schema should parse"),
            ),
            (
                schemars::schema_for!(ProviderManifest),
                serde_json::from_str::<Value>(SIDECAR_SCHEMA).expect("sidecar schema should parse"),
            ),
        ];

        for (generated, checked_in) in cases {
            let generated =
                serde_json::to_value(generated).expect("generated schema should serialize");
            let generated_properties: BTreeSet<String> = generated["properties"]
                .as_object()
                .expect("generated schema should declare properties")
                .keys()
                .cloned()
                .collect();
            let checked_in_properties: BTreeSet<String> = checked_in["properties"]
                .as_object()
                .expect("checked-in schema should declare properties")
                .keys()
                .cloned()
                .collect();
            assert_eq!(generated_properties, checked_in_properties);
            assert_eq!(
                string_set(&generated["required"]),
                string_set(&checked_in["required"])
            );
            assert_eq!(
                generated["additionalProperties"],
                checked_in["additionalProperties"]
            );
        }
    }

    #[test]
    fn wire_types_round_trip() {
        let manifest = sample_manifest();
        let serialized = serde_json::to_value(&manifest).expect("manifest should serialize");
        let deserialized: ProviderManifest =
            serde_json::from_value(serialized).expect("manifest should deserialize");
        assert_eq!(manifest, deserialized);
    }

    fn sample_spec() -> ExperimentSpec {
        ExperimentSpec {
            hypothesis: "Raising mock.value improves throughput".to_owned(),
            target: ChangeRequest {
                capability_id: "mock.value".to_owned(),
                parameters: json!({ "value": 60 }),
                lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
            },
            warmup_samples: 2,
            baseline_samples: NonZeroU32::new(5).expect("count is non-zero"),
            candidate_samples: NonZeroU32::new(5).expect("count is non-zero"),
            bounds: DecisionBounds {
                min_samples: NonZeroU32::new(3).expect("count is non-zero"),
                min_fps_improvement: 5.0,
                max_temperature_c: 85.0,
                max_power_w: 200.0,
                max_errors: 0,
            },
        }
    }

    fn sample_verdict() -> Verdict {
        Verdict {
            decision: Decision::Promote,
            reason: VerdictReason::Promoted,
            fps_improvement: 12.5,
            baseline: MetricSummary {
                samples: 5,
                mean_fps: 100.0,
                max_temperature_c: 60.0,
                max_power_w: 120.0,
                total_errors: 0,
            },
            candidate: MetricSummary {
                samples: 5,
                mean_fps: 112.5,
                max_temperature_c: 70.0,
                max_power_w: 140.0,
                total_errors: 0,
            },
        }
    }

    #[test]
    fn experiment_and_verdict_schemas_match_checked_in() {
        let cases = [
            (
                schemars::schema_for!(ExperimentSpec),
                serde_json::from_str::<Value>(EXPERIMENT_SCHEMA)
                    .expect("experiment schema should parse"),
            ),
            (
                schemars::schema_for!(Verdict),
                serde_json::from_str::<Value>(VERDICT_SCHEMA).expect("verdict schema should parse"),
            ),
        ];

        for (generated, checked_in) in cases {
            let generated =
                serde_json::to_value(generated).expect("generated schema should serialize");
            let generated_properties: BTreeSet<String> = generated["properties"]
                .as_object()
                .expect("generated schema should declare properties")
                .keys()
                .cloned()
                .collect();
            let checked_in_properties: BTreeSet<String> = checked_in["properties"]
                .as_object()
                .expect("checked-in schema should declare properties")
                .keys()
                .cloned()
                .collect();
            assert_eq!(generated_properties, checked_in_properties);
            assert_eq!(
                string_set(&generated["required"]),
                string_set(&checked_in["required"])
            );
            assert_eq!(
                generated["additionalProperties"],
                checked_in["additionalProperties"]
            );
        }
    }

    #[test]
    fn verdict_enum_wire_strings_match_schema() {
        assert_eq!(wire_string(Decision::Promote), "promote");
        assert_eq!(wire_string(Decision::Reject), "reject");
        assert_eq!(wire_string(VerdictReason::Promoted), "promoted");
        assert_eq!(
            wire_string(VerdictReason::InsufficientSamples),
            "insufficient-samples"
        );
        assert_eq!(
            wire_string(VerdictReason::TemperatureExceeded),
            "temperature-exceeded"
        );
        assert_eq!(wire_string(VerdictReason::PowerExceeded), "power-exceeded");
        assert_eq!(
            wire_string(VerdictReason::ErrorsExceeded),
            "errors-exceeded"
        );
        assert_eq!(
            wire_string(VerdictReason::InsufficientImprovement),
            "insufficient-improvement"
        );

        let schema: Value =
            serde_json::from_str(VERDICT_SCHEMA).expect("verdict schema should parse");
        assert_eq!(
            string_set(&schema["properties"]["decision"]["enum"]),
            [Decision::Promote, Decision::Reject]
                .map(wire_string)
                .into_iter()
                .collect()
        );
        assert_eq!(
            string_set(&schema["properties"]["reason"]["enum"]),
            [
                VerdictReason::Promoted,
                VerdictReason::InsufficientSamples,
                VerdictReason::TemperatureExceeded,
                VerdictReason::PowerExceeded,
                VerdictReason::ErrorsExceeded,
                VerdictReason::InsufficientImprovement,
            ]
            .map(wire_string)
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn experiment_and_verdict_wire_types_round_trip() {
        let spec = sample_spec();
        let deserialized: ExperimentSpec =
            serde_json::from_value(serde_json::to_value(&spec).expect("spec should serialize"))
                .expect("spec should deserialize");
        assert_eq!(spec, deserialized);

        let verdict = sample_verdict();
        let deserialized: Verdict = serde_json::from_value(
            serde_json::to_value(&verdict).expect("verdict should serialize"),
        )
        .expect("verdict should deserialize");
        assert_eq!(verdict, deserialized);
    }

    #[test]
    fn experiment_types_reject_unknown_fields() {
        let mut serialized = serde_json::to_value(sample_spec()).expect("spec should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ExperimentSpec>(serialized).is_err());

        let mut serialized =
            serde_json::to_value(sample_verdict()).expect("verdict should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<Verdict>(serialized).is_err());
    }
}
