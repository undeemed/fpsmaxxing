//! Versioned data contracts shared by `FPSMaxxing` applications and sidecars.

pub mod ipc;
#[cfg(test)]
mod test_support;

use std::num::{NonZeroU32, NonZeroU64};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
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

/// Inclusive ceiling on a [`ChangeRequest`]'s TTL lease, in seconds.
///
/// The lease is what bounds how long a mutation may survive, so the ceiling
/// belongs to the shared request type rather than to any one enforcement point:
/// it is mirrored as `maximum` on `lease_seconds` in
/// `schemas/experiment.schema.json`, applied by the broker policy in
/// `crates/control-plane` before a lifecycle runs, and applied again by the
/// experiment runner, which bounds a spec before it measures anything. Callers
/// that publish their own request schema, such as the gateway's advertised tool
/// input, state the bound independently.
pub const MAX_LEASE_SECONDS: u64 = 300;

/// A requested provider change after policy validation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeRequest {
    /// Capability being invoked.
    pub capability_id: String,
    /// Capability-specific parameters; always a JSON object.
    #[serde(deserialize_with = "object_parameters")]
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub parameters: Value,
    /// Automatic rollback deadline in seconds; every mutation carries a
    /// non-zero TTL lease, at most [`MAX_LEASE_SECONDS`].
    #[schemars(range(max = MAX_LEASE_SECONDS))]
    pub lease_seconds: NonZeroU64,
}

/// Accepts only a JSON object for [`ChangeRequest::parameters`].
///
/// Capability parameters are always named, and the checked-in schemas type the
/// field as an object. Without this the Rust type would accept a bare scalar or
/// array that every schema validator on the same wire would reject.
fn object_parameters<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    let parameters = Value::deserialize(deserializer)?;
    if parameters.is_object() {
        Ok(parameters)
    } else {
        Err(serde::de::Error::custom("parameters must be a JSON object"))
    }
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

/// Inclusive ceiling the bounded alpha policy enforces on a trial's temperature
/// bound, in degrees Celsius.
///
/// The thresholds an immutable evaluator applies arrive in an LLM-authored
/// experiment spec, so a spec could otherwise disarm its own safety gate by
/// declaring an unreachable ceiling. Policy owns the hard envelope: a spec may
/// tighten these bounds but never loosen them. This value sits below the
/// throttle point of the consumer hardware the alpha targets. It is mirrored as
/// `maximum` on `max_temperature_c` in `schemas/experiment.schema.json`.
pub const MAX_DECISION_TEMPERATURE_C: f64 = 90.0;

/// Inclusive ceiling the bounded alpha policy enforces on a trial's power
/// bound, in watts.
///
/// See [`MAX_DECISION_TEMPERATURE_C`] for why the envelope is policy-owned.
pub const MAX_DECISION_POWER_W: f64 = 250.0;

/// Inclusive ceiling the bounded alpha policy enforces on a trial's error
/// budget.
///
/// A correctness fault is never an acceptable cost of a performance gain on
/// this path, so the alpha promotes nothing that reported one; a spec may
/// restate this ceiling but not raise it. See [`MAX_DECISION_TEMPERATURE_C`]
/// for why the envelope is policy-owned.
pub const MAX_DECISION_ERRORS: u64 = 0;

/// Fixed thresholds the immutable evaluator applies to a trial.
///
/// Every threshold is bounded by the policy envelope a spec may tighten but
/// never loosen; the same bounds are mirrored in
/// `schemas/experiment.schema.json` and re-checked at run time by the runner.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionBounds {
    /// Minimum samples required in each of the baseline and candidate sets
    /// before a promotion can be considered; at most [`MAX_SAMPLES`].
    #[schemars(range(max = MAX_SAMPLES))]
    pub min_samples: NonZeroU32,
    /// Minimum mean-FPS gain the candidate must show over the baseline; a
    /// non-negative gain.
    #[schemars(range(min = 0.0))]
    pub min_fps_improvement: f64,
    /// Inclusive ceiling for candidate temperature in degrees Celsius; above 0
    /// and at most [`MAX_DECISION_TEMPERATURE_C`].
    #[schemars(range(max = MAX_DECISION_TEMPERATURE_C), extend("exclusiveMinimum" = 0.0))]
    pub max_temperature_c: f64,
    /// Inclusive ceiling for candidate power draw in watts; above 0 and at most
    /// [`MAX_DECISION_POWER_W`].
    #[schemars(range(max = MAX_DECISION_POWER_W), extend("exclusiveMinimum" = 0.0))]
    pub max_power_w: f64,
    /// Inclusive ceiling for candidate correctness errors; at most
    /// [`MAX_DECISION_ERRORS`].
    #[schemars(range(max = MAX_DECISION_ERRORS))]
    pub max_errors: u64,
}

/// Inclusive ceiling on every sample count in an [`ExperimentSpec`].
///
/// Sample counts arrive over the wire from an LLM-authored spec and size the
/// measurement buffers a runner allocates, so they are bounded like every other
/// parameter the broker accepts. The ceiling also bounds the journaled trial
/// record, which carries every counted sample in a single row. The ceiling is
/// mirrored as `maximum` in `schemas/experiment.schema.json`.
pub const MAX_SAMPLES: u32 = 10_000;

/// Inclusive ceiling on an [`ExperimentSpec`]'s hypothesis, in characters.
///
/// The hypothesis is free text an agent authors and the runner writes verbatim
/// into a single durable trial row, so it is bounded like every other field of
/// the spec rather than sizing that row by whatever the author sends. The
/// ceiling is mirrored as `maxLength` in `schemas/experiment.schema.json`, which
/// counts Unicode characters, so the runner counts them the same way. The floor
/// is [`MIN_HYPOTHESIS_CHARS`].
pub const MAX_HYPOTHESIS_CHARS: u32 = 4_096;

/// Inclusive floor on an [`ExperimentSpec`]'s hypothesis, in characters.
///
/// A trial row is the durable statement of what a promotion was for, so the
/// hypothesis that states it may not be blank. The floor is mirrored as
/// `minLength` in `schemas/experiment.schema.json`.
pub const MIN_HYPOTHESIS_CHARS: u32 = 1;

/// A declarative, typed experiment the runner can execute, journal, and replay.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    /// Human-authored hypothesis the trial tests; from
    /// [`MIN_HYPOTHESIS_CHARS`] to [`MAX_HYPOTHESIS_CHARS`] characters.
    #[schemars(length(min = MIN_HYPOTHESIS_CHARS, max = MAX_HYPOTHESIS_CHARS))]
    pub hypothesis: String,
    /// Bounded, policy-checkable capability change under test.
    pub target: ChangeRequest,
    /// Leading measurements discarded from each phase before counting; at most
    /// [`MAX_SAMPLES`].
    #[schemars(range(max = MAX_SAMPLES))]
    pub warmup_samples: u32,
    /// Counted baseline measurements to record; at most [`MAX_SAMPLES`].
    #[schemars(range(max = MAX_SAMPLES))]
    pub baseline_samples: NonZeroU32,
    /// Counted candidate measurements to record; at most [`MAX_SAMPLES`].
    #[schemars(range(max = MAX_SAMPLES))]
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
        MAX_DECISION_ERRORS, MAX_DECISION_POWER_W, MAX_DECISION_TEMPERATURE_C,
        MAX_HYPOTHESIS_CHARS, MAX_LEASE_SECONDS, MAX_SAMPLES, MIN_HYPOTHESIS_CHARS, MetricSample,
        MetricSummary, NonZeroU32, NonZeroU64, Persistence, ProviderManifest, RiskClass, Verdict,
        VerdictReason,
    };
    use crate::test_support::{properties, serialized_fields, string_set, wire_string};

    const CAPABILITY_SCHEMA: &str = include_str!("../../../schemas/capability.schema.json");
    const SIDECAR_SCHEMA: &str = include_str!("../../../schemas/sidecar.schema.json");
    const EXPERIMENT_SCHEMA: &str = include_str!("../../../schemas/experiment.schema.json");
    const VERDICT_SCHEMA: &str = include_str!("../../../schemas/verdict.schema.json");
    const METRIC_SAMPLE_SCHEMA: &str = include_str!("../../../schemas/metric-sample.schema.json");

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
        let fields = serialized_fields(sample_capability());
        assert_eq!(fields, properties(&schema));
        assert_eq!(fields, string_set(&schema["required"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn manifest_fields_match_sidecar_schema() {
        let schema: Value =
            serde_json::from_str(SIDECAR_SCHEMA).expect("sidecar schema should parse");
        let fields = serialized_fields(sample_manifest());
        assert_eq!(fields, properties(&schema));
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
    fn lease_seconds_is_bounded_like_the_schema() {
        // The lease bounds how long a mutation may survive, so the ceiling is
        // declared once on the shared request type and mirrored wherever a
        // change request is published for an agent to author against.
        let checked_in: Value =
            serde_json::from_str(EXPERIMENT_SCHEMA).expect("experiment schema should parse");
        assert_eq!(
            checked_in["$defs"]["change_request"]["properties"]["lease_seconds"]["maximum"],
            json!(MAX_LEASE_SECONDS)
        );

        let generated = serde_json::to_value(schemars::schema_for!(ExperimentSpec))
            .expect("generated schema should serialize");
        assert_eq!(
            generated["$defs"]["ChangeRequest"]["properties"]["lease_seconds"]["maximum"],
            json!(MAX_LEASE_SECONDS)
        );
    }

    #[test]
    fn non_object_parameters_are_rejected_like_the_schema() {
        for parameters in [json!("value=1"), json!(1), json!([1]), json!(null)] {
            let serialized = json!({
                "capability_id": "mock.value",
                "parameters": parameters,
                "lease_seconds": 1
            });
            assert!(
                serde_json::from_value::<ChangeRequest>(serialized).is_err(),
                "{parameters} must not deserialize as capability parameters"
            );
        }
    }

    /// Asserts that two object schemas declare the same fields.
    ///
    /// Property names, the `required` list, and `additionalProperties` are
    /// compared; neither property types nor bounds are, so a `type` that
    /// disagrees passes here just as a dropped bound does. A field whose
    /// checked-in schema constrains it beyond a bare `type` (a `minLength`, a
    /// `pattern`, a numeric bound) or that carries a `deserialize_with`
    /// validator requires its own dedicated test instead. That rule covers the
    /// narrower subset, so a field declared with nothing but a `type` stays
    /// uncovered by both.
    fn assert_object_parity(label: &str, generated: &Value, checked_in: &Value) {
        let generated_properties: BTreeSet<String> = generated["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("generated {label} should declare properties"))
            .keys()
            .cloned()
            .collect();
        let checked_in_properties: BTreeSet<String> = checked_in["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("checked-in {label} should declare properties"))
            .keys()
            .cloned()
            .collect();
        assert_eq!(generated_properties, checked_in_properties, "{label}");
        assert_eq!(
            string_set(&generated["required"]),
            string_set(&checked_in["required"]),
            "{label}"
        );
        assert_eq!(
            generated["additionalProperties"], checked_in["additionalProperties"],
            "{label}"
        );
    }

    /// Names the `$defs` entries of a generated schema that describe objects.
    ///
    /// Enum definitions are skipped: they carry no `properties` and the
    /// checked-in schemas inline them, so the `*_enum_wire_strings_match_*`
    /// tests cover their parity instead.
    fn generated_object_definitions(generated: &Value) -> BTreeSet<String> {
        generated["$defs"]
            .as_object()
            .map(|definitions| {
                definitions
                    .iter()
                    .filter(|(_, definition)| definition.get("properties").is_some())
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One generated schema paired with the checked-in file it must mirror.
    struct SchemaCase {
        label: &'static str,
        generated: Value,
        checked_in: Value,
        /// Every object definition the generated schema puts in `$defs`, mapped
        /// to the checked-in `$defs` key that mirrors it. `None` marks a
        /// definition that a separate checked-in schema file already covers.
        definitions: &'static [(&'static str, Option<&'static str>)],
    }

    fn schema_cases() -> Vec<SchemaCase> {
        vec![
            SchemaCase {
                label: "CapabilityDescriptor",
                generated: serde_json::to_value(schemars::schema_for!(CapabilityDescriptor))
                    .expect("generated schema should serialize"),
                checked_in: serde_json::from_str(CAPABILITY_SCHEMA)
                    .expect("capability schema should parse"),
                definitions: &[],
            },
            SchemaCase {
                label: "ProviderManifest",
                generated: serde_json::to_value(schemars::schema_for!(ProviderManifest))
                    .expect("generated schema should serialize"),
                checked_in: serde_json::from_str(SIDECAR_SCHEMA)
                    .expect("sidecar schema should parse"),
                // capability.schema.json is referenced across files and checked
                // as its own case.
                definitions: &[("CapabilityDescriptor", None)],
            },
            SchemaCase {
                label: "ExperimentSpec",
                generated: serde_json::to_value(schemars::schema_for!(ExperimentSpec))
                    .expect("generated schema should serialize"),
                checked_in: serde_json::from_str(EXPERIMENT_SCHEMA)
                    .expect("experiment schema should parse"),
                definitions: &[
                    ("ChangeRequest", Some("change_request")),
                    ("DecisionBounds", Some("decision_bounds")),
                ],
            },
            SchemaCase {
                label: "Verdict",
                generated: serde_json::to_value(schemars::schema_for!(Verdict))
                    .expect("generated schema should serialize"),
                checked_in: serde_json::from_str(VERDICT_SCHEMA)
                    .expect("verdict schema should parse"),
                definitions: &[("MetricSummary", Some("metric_summary"))],
            },
            SchemaCase {
                label: "MetricSample",
                generated: serde_json::to_value(schemars::schema_for!(MetricSample))
                    .expect("generated schema should serialize"),
                checked_in: serde_json::from_str(METRIC_SAMPLE_SCHEMA)
                    .expect("metric sample schema should parse"),
                definitions: &[],
            },
        ]
    }

    #[test]
    fn generated_schemas_match_checked_in_schemas() {
        for case in schema_cases() {
            assert_object_parity(case.label, &case.generated, &case.checked_in);

            // Every nested object type must be mapped, so introducing one
            // fails this test until the checked-in schema gains a matching
            // definition.
            assert_eq!(
                generated_object_definitions(&case.generated),
                case.definitions
                    .iter()
                    .map(|(name, _)| (*name).to_owned())
                    .collect::<BTreeSet<String>>(),
                "{} nested object definitions",
                case.label
            );
            let checked_in_definitions: BTreeSet<String> = case.checked_in["$defs"]
                .as_object()
                .map(|definitions| definitions.keys().cloned().collect())
                .unwrap_or_default();
            assert_eq!(
                checked_in_definitions,
                case.definitions
                    .iter()
                    .filter_map(|(_, target)| target.map(ToOwned::to_owned))
                    .collect::<BTreeSet<String>>(),
                "{} checked-in $defs",
                case.label
            );

            for (name, target) in case.definitions {
                let Some(target) = target else { continue };
                assert_object_parity(
                    &format!("{}/$defs/{target}", case.label),
                    &case.generated["$defs"][name],
                    &case.checked_in["$defs"][target],
                );
            }
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
    fn sample_counts_are_bounded_like_the_schema() {
        let schema: Value =
            serde_json::from_str(EXPERIMENT_SCHEMA).expect("experiment schema should parse");
        for field in ["warmup_samples", "baseline_samples", "candidate_samples"] {
            assert_eq!(schema["properties"][field]["maximum"], json!(MAX_SAMPLES));
        }

        let generated = serde_json::to_value(schemars::schema_for!(ExperimentSpec))
            .expect("generated schema should serialize");
        for field in ["warmup_samples", "baseline_samples", "candidate_samples"] {
            assert_eq!(
                generated["properties"][field]["maximum"],
                json!(MAX_SAMPLES),
                "{field}"
            );
        }
    }

    #[test]
    fn the_hypothesis_is_bounded_like_the_schema() {
        // The hypothesis is free text an agent authors and the runner journals
        // verbatim, so both its ceiling and its floor are declared here and
        // mirrored by the schema the agent writes against. The floor matters as
        // much as the ceiling: a blank hypothesis leaves a promoted trial with
        // no statement of what it was for.
        let checked_in: Value =
            serde_json::from_str(EXPERIMENT_SCHEMA).expect("experiment schema should parse");
        let generated = serde_json::to_value(schemars::schema_for!(ExperimentSpec))
            .expect("generated schema should serialize");
        for schema in [&checked_in, &generated] {
            assert_eq!(
                schema["properties"]["hypothesis"]["maxLength"],
                json!(MAX_HYPOTHESIS_CHARS)
            );
            assert_eq!(
                schema["properties"]["hypothesis"]["minLength"],
                json!(MIN_HYPOTHESIS_CHARS)
            );
        }
    }

    #[test]
    fn decision_bounds_are_bounded_like_the_schema() {
        // The evaluator's thresholds arrive in the spec, so the policy envelope
        // is declared once in this crate and mirrored by both schemas.
        let checked_in: Value =
            serde_json::from_str(EXPERIMENT_SCHEMA).expect("experiment schema should parse");
        let generated = serde_json::to_value(schemars::schema_for!(ExperimentSpec))
            .expect("generated schema should serialize");
        let envelope = [
            ("min_samples", "maximum", json!(MAX_SAMPLES)),
            ("min_fps_improvement", "minimum", json!(0.0)),
            ("max_temperature_c", "exclusiveMinimum", json!(0.0)),
            (
                "max_temperature_c",
                "maximum",
                json!(MAX_DECISION_TEMPERATURE_C),
            ),
            ("max_power_w", "exclusiveMinimum", json!(0.0)),
            ("max_power_w", "maximum", json!(MAX_DECISION_POWER_W)),
            ("max_errors", "maximum", json!(MAX_DECISION_ERRORS)),
        ];
        for (field, keyword, expected) in envelope {
            assert_eq!(
                checked_in["$defs"]["decision_bounds"]["properties"][field][keyword], expected,
                "checked-in decision_bounds.{field}.{keyword}"
            );
            assert_eq!(
                generated["$defs"]["DecisionBounds"]["properties"][field][keyword], expected,
                "generated DecisionBounds.{field}.{keyword}"
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
