//! Wire contracts for the authenticated local broker IPC boundary.
//!
//! These types are the only vocabulary the unprivileged side may speak to the
//! privileged broker. The broker exposes exactly two operations - capability
//! discovery and a bounded provider lifecycle - and no field can carry a raw
//! shell command, an arbitrary Registry path, or a raw hardware primitive.
//! The transport that carries these messages is abstracted behind a trait in
//! the `fpsmaxxing-ipc` crate: a Unix domain socket on Linux today, a Windows
//! named pipe later.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ChangeRequest, ProviderManifest};

/// The operation a client asks the privileged broker to perform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrokerOp {
    /// Return the typed, policy-approved capability catalog.
    Discover,
    /// Run snapshot, preview, apply, verify, and rollback for one bounded
    /// change.
    RunLifecycle,
}

/// A request sent to the broker over the authenticated local IPC boundary.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerRequest {
    /// Operation the client is invoking.
    pub op: BrokerOp,
    /// Logical owner that holds the single-owner lease for `run-lifecycle`; the
    /// broker rejects a second concurrent owner of the same knob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Bounded change for `run-lifecycle`; ignored by `discover`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeRequest>,
}

impl BrokerRequest {
    /// Builds a capability discovery request.
    #[must_use]
    pub const fn discover() -> Self {
        Self {
            op: BrokerOp::Discover,
            owner: None,
            change: None,
        }
    }

    /// Builds a lifecycle request owned by `owner` for one bounded change.
    #[must_use]
    pub fn run_lifecycle(owner: impl Into<String>, change: ChangeRequest) -> Self {
        Self {
            op: BrokerOp::RunLifecycle,
            owner: Some(owner.into()),
            change: Some(change),
        }
    }
}

/// Which payload a [`BrokerResponse`] carries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrokerOutcome {
    /// The response carries a capability catalog.
    Capabilities,
    /// The response carries a completed lifecycle report.
    Lifecycle,
    /// The response carries a typed error.
    Error,
}

/// A response returned by the broker.
///
/// The `outcome` discriminator and the payload it names are one value, so a tag
/// without its payload - or a tag paired with another variant's payload - is not
/// representable and does not deserialize. A consumer therefore never has to
/// unwrap a payload the protocol promises is present, and a malformed or
/// version-skewed peer is refused at decode time rather than panicking a caller.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BrokerResponse {
    /// The typed, policy-approved capability catalog.
    Capabilities {
        /// The catalog the broker advertises.
        capabilities: ProviderManifest,
    },
    /// A completed provider lifecycle.
    Lifecycle {
        /// The auditable result of the lifecycle.
        lifecycle: LifecycleReport,
    },
    /// A typed denial or failure.
    Error {
        /// Why the request was denied or failed.
        error: BrokerErrorBody,
    },
}

impl BrokerResponse {
    /// Builds a capability-catalog response.
    #[must_use]
    pub const fn capabilities(manifest: ProviderManifest) -> Self {
        Self::Capabilities {
            capabilities: manifest,
        }
    }

    /// Builds a completed-lifecycle response.
    #[must_use]
    pub const fn lifecycle(report: LifecycleReport) -> Self {
        Self::Lifecycle { lifecycle: report }
    }

    /// Builds a typed error response.
    #[must_use]
    pub fn error(kind: BrokerErrorKind, message: impl Into<String>) -> Self {
        Self::Error {
            error: BrokerErrorBody {
                kind,
                message: message.into(),
            },
        }
    }

    /// Returns which payload this response carries.
    #[must_use]
    pub const fn outcome(&self) -> BrokerOutcome {
        match self {
            Self::Capabilities { .. } => BrokerOutcome::Capabilities,
            Self::Lifecycle { .. } => BrokerOutcome::Lifecycle,
            Self::Error { .. } => BrokerOutcome::Error,
        }
    }
}

/// The auditable result of a completed provider lifecycle, mirrored onto the
/// wire so the unprivileged side never links against the control plane.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleReport {
    /// Provider that owned the change.
    pub provider_id: String,
    /// Human-readable preview produced before the write.
    pub preview: String,
    /// Whether the requested value was observed after apply.
    pub verified: bool,
    /// Whether the captured baseline was restored before returning.
    pub rolled_back: bool,
}

/// A stable, machine-readable classification for a broker denial or failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrokerErrorKind {
    /// The peer failed the local IPC authentication check and was rejected
    /// before any request was processed.
    Unauthenticated,
    /// The frame or request could not be decoded into a typed request.
    Malformed,
    /// The capability id is not in the provider catalog.
    UnknownCapability,
    /// The request violated the bounded broker policy.
    PolicyDenied,
    /// Another owner already holds the requested knob.
    OwnerConflict,
    /// The provider lifecycle failed after the request was accepted.
    LifecycleFailed,
    /// The broker hit an unexpected internal fault.
    Internal,
}

/// A typed error body returned to the client.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerErrorBody {
    /// Stable machine-readable error classification.
    pub kind: BrokerErrorKind,
    /// Human-readable detail; never contains secrets or raw host primitives.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroU64;

    use serde_json::{Value, json};

    use super::{
        BrokerErrorBody, BrokerErrorKind, BrokerOp, BrokerOutcome, BrokerRequest, BrokerResponse,
        LifecycleReport,
    };
    use crate::ChangeRequest;
    use crate::test_support::{assert_same_shape, properties, string_set, wire_string};

    const REQUEST_SCHEMA: &str = include_str!("../../../schemas/broker-request.schema.json");
    const RESPONSE_SCHEMA: &str = include_str!("../../../schemas/broker-response.schema.json");

    fn request_schema() -> Value {
        serde_json::from_str(REQUEST_SCHEMA).expect("request schema should parse")
    }

    fn response_schema() -> Value {
        serde_json::from_str(RESPONSE_SCHEMA).expect("response schema should parse")
    }

    /// Indexes a tagged-union schema's `oneOf` branches by their `outcome` tag.
    fn branches_by_tag(schema: &Value) -> BTreeSet<String> {
        schema["oneOf"]
            .as_array()
            .expect("a tagged union should declare oneOf")
            .iter()
            .map(|branch| {
                branch["properties"]["outcome"]["const"]
                    .as_str()
                    .expect("every branch should pin its outcome tag")
                    .to_owned()
            })
            .collect()
    }

    /// Returns the `oneOf` branch a tagged-union schema gives to `tag`.
    fn branch(schema: &Value, tag: &str) -> Value {
        schema["oneOf"]
            .as_array()
            .expect("a tagged union should declare oneOf")
            .iter()
            .find(|branch| branch["properties"]["outcome"]["const"] == json!(tag))
            .unwrap_or_else(|| panic!("the schema should declare a {tag} branch"))
            .clone()
    }

    fn sample_change() -> ChangeRequest {
        ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 42 }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
        }
    }

    fn sample_report() -> LifecycleReport {
        LifecycleReport {
            provider_id: "mock".to_owned(),
            preview: "set mock.value from 0 to 42".to_owned(),
            verified: true,
            rolled_back: true,
        }
    }

    #[test]
    fn enum_wire_strings_match_schemas() {
        assert_eq!(wire_string(BrokerOp::Discover), "discover");
        assert_eq!(wire_string(BrokerOp::RunLifecycle), "run-lifecycle");
        assert_eq!(wire_string(BrokerOutcome::Capabilities), "capabilities");
        assert_eq!(wire_string(BrokerOutcome::Lifecycle), "lifecycle");
        assert_eq!(wire_string(BrokerOutcome::Error), "error");
        assert_eq!(
            wire_string(BrokerErrorKind::Unauthenticated),
            "unauthenticated"
        );
        assert_eq!(wire_string(BrokerErrorKind::Malformed), "malformed");
        assert_eq!(
            wire_string(BrokerErrorKind::UnknownCapability),
            "unknown-capability"
        );
        assert_eq!(wire_string(BrokerErrorKind::PolicyDenied), "policy-denied");
        assert_eq!(
            wire_string(BrokerErrorKind::OwnerConflict),
            "owner-conflict"
        );
        assert_eq!(
            wire_string(BrokerErrorKind::LifecycleFailed),
            "lifecycle-failed"
        );
        assert_eq!(wire_string(BrokerErrorKind::Internal), "internal");

        assert_eq!(
            string_set(&request_schema()["properties"]["op"]["enum"]),
            [BrokerOp::Discover, BrokerOp::RunLifecycle]
                .map(wire_string)
                .into_iter()
                .collect()
        );
        let response = response_schema();
        assert_eq!(
            branches_by_tag(&response),
            [
                BrokerOutcome::Capabilities,
                BrokerOutcome::Lifecycle,
                BrokerOutcome::Error,
            ]
            .map(wire_string)
            .into_iter()
            .collect()
        );
        assert_eq!(
            string_set(&response["$defs"]["BrokerErrorBody"]["properties"]["kind"]["enum"]),
            [
                BrokerErrorKind::Unauthenticated,
                BrokerErrorKind::Malformed,
                BrokerErrorKind::UnknownCapability,
                BrokerErrorKind::PolicyDenied,
                BrokerErrorKind::OwnerConflict,
                BrokerErrorKind::LifecycleFailed,
                BrokerErrorKind::Internal,
            ]
            .map(wire_string)
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn request_fields_match_schema() {
        let schema = request_schema();
        assert_same_shape(&schemars::schema_for!(BrokerRequest), &schema);
        assert_eq!(schema["required"], json!(["op"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn response_variants_match_schema() {
        let schema = response_schema();
        let generated = serde_json::to_value(schemars::schema_for!(BrokerResponse))
            .expect("generated schema should serialize");
        assert_eq!(branches_by_tag(&generated), branches_by_tag(&schema));
        for tag in branches_by_tag(&schema) {
            let generated = branch(&generated, &tag);
            let checked_in = branch(&schema, &tag);
            assert_eq!(properties(&generated), properties(&checked_in), "{tag}");
            assert_eq!(
                string_set(&generated["required"]),
                string_set(&checked_in["required"]),
                "{tag}"
            );
            assert_eq!(checked_in["additionalProperties"], json!(false), "{tag}");
            assert!(
                string_set(&checked_in["required"]).contains("outcome"),
                "{tag} must require its own tag"
            );
        }
    }

    #[test]
    fn change_request_fields_match_request_schema_defs() {
        assert_same_shape(
            &schemars::schema_for!(ChangeRequest),
            &request_schema()["$defs"]["ChangeRequest"],
        );
    }

    #[test]
    fn lifecycle_report_fields_match_response_schema_defs() {
        assert_same_shape(
            &schemars::schema_for!(LifecycleReport),
            &response_schema()["$defs"]["LifecycleReport"],
        );
    }

    #[test]
    fn error_body_fields_match_response_schema_defs() {
        assert_same_shape(
            &schemars::schema_for!(BrokerErrorBody),
            &response_schema()["$defs"]["BrokerErrorBody"],
        );
    }

    #[test]
    fn change_request_parameters_are_an_object_in_both() {
        assert_eq!(
            request_schema()["$defs"]["ChangeRequest"]["properties"]["parameters"]["type"],
            json!("object")
        );
        let generated = serde_json::to_value(schemars::schema_for!(ChangeRequest))
            .expect("generated schema should serialize");
        assert_eq!(
            generated["properties"]["parameters"]["type"],
            json!("object")
        );

        let mut serialized =
            serde_json::to_value(sample_change()).expect("change should serialize");
        serialized["parameters"] = json!("value=42");
        assert!(serde_json::from_value::<ChangeRequest>(serialized).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut serialized =
            serde_json::to_value(BrokerRequest::discover()).expect("request should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<BrokerRequest>(serialized).is_err());

        let mut serialized = serde_json::to_value(BrokerResponse::error(
            BrokerErrorKind::PolicyDenied,
            "denied",
        ))
        .expect("response should serialize");
        serialized["unexpected"] = json!(true);
        assert!(serde_json::from_value::<BrokerResponse>(serialized).is_err());
    }

    #[test]
    fn a_tag_without_its_payload_is_not_representable() {
        for tag in ["capabilities", "lifecycle", "error"] {
            assert!(
                serde_json::from_value::<BrokerResponse>(json!({ "outcome": tag })).is_err(),
                "{tag} must not deserialize without its payload"
            );
        }
    }

    #[test]
    fn a_mismatched_tag_and_payload_is_not_representable() {
        let report = serde_json::to_value(sample_report()).expect("report should serialize");
        for tag in ["capabilities", "error"] {
            assert!(
                serde_json::from_value::<BrokerResponse>(
                    json!({ "outcome": tag, "lifecycle": report })
                )
                .is_err(),
                "{tag} must not deserialize carrying a lifecycle report"
            );
        }
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            BrokerRequest::discover(),
            BrokerRequest::run_lifecycle("owner-a", sample_change()),
        ] {
            let serialized = serde_json::to_value(&request).expect("request should serialize");
            let deserialized: BrokerRequest =
                serde_json::from_value(serialized).expect("request should deserialize");
            assert_eq!(request, deserialized);
        }
    }

    #[test]
    fn responses_round_trip() {
        let responses = [
            BrokerResponse::lifecycle(sample_report()),
            BrokerResponse::error(BrokerErrorKind::OwnerConflict, "held by owner-a"),
        ];
        for response in responses {
            let serialized = serde_json::to_value(&response).expect("response should serialize");
            assert_eq!(
                serialized["outcome"],
                json!(wire_string(response.outcome()))
            );
            let deserialized: BrokerResponse =
                serde_json::from_value(serialized).expect("response should deserialize");
            assert_eq!(response, deserialized);
        }
    }

    #[test]
    fn error_body_carries_the_typed_kind() {
        let body = BrokerErrorBody {
            kind: BrokerErrorKind::UnknownCapability,
            message: "shell.exec".to_owned(),
        };
        let value = serde_json::to_value(&body).expect("error body should serialize");
        assert_eq!(value["kind"], "unknown-capability");
        assert_eq!(value["message"], "shell.exec");
    }
}
