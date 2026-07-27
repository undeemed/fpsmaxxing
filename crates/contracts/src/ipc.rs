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
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerResponse {
    /// Which payload field is populated.
    pub outcome: BrokerOutcome,
    /// Present when `outcome` is `capabilities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ProviderManifest>,
    /// Present when `outcome` is `lifecycle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleReport>,
    /// Present when `outcome` is `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BrokerErrorBody>,
}

impl BrokerResponse {
    /// Builds a capability-catalog response.
    #[must_use]
    pub fn capabilities(manifest: ProviderManifest) -> Self {
        Self {
            outcome: BrokerOutcome::Capabilities,
            capabilities: Some(manifest),
            lifecycle: None,
            error: None,
        }
    }

    /// Builds a completed-lifecycle response.
    #[must_use]
    pub fn lifecycle(report: LifecycleReport) -> Self {
        Self {
            outcome: BrokerOutcome::Lifecycle,
            capabilities: None,
            lifecycle: Some(report),
            error: None,
        }
    }

    /// Builds a typed error response.
    #[must_use]
    pub fn error(kind: BrokerErrorKind, message: impl Into<String>) -> Self {
        Self {
            outcome: BrokerOutcome::Error,
            capabilities: None,
            lifecycle: None,
            error: Some(BrokerErrorBody {
                kind,
                message: message.into(),
            }),
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

    const REQUEST_SCHEMA: &str = include_str!("../../../schemas/broker-request.schema.json");
    const RESPONSE_SCHEMA: &str = include_str!("../../../schemas/broker-response.schema.json");

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

    fn generated_properties(schema: &schemars::Schema) -> BTreeSet<String> {
        serde_json::to_value(schema).expect("schema should serialize")["properties"]
            .as_object()
            .expect("schema should declare properties")
            .keys()
            .cloned()
            .collect()
    }

    fn schema_properties(schema: &Value) -> BTreeSet<String> {
        schema["properties"]
            .as_object()
            .expect("schema should declare properties")
            .keys()
            .cloned()
            .collect()
    }

    fn sample_change() -> ChangeRequest {
        ChangeRequest {
            capability_id: "mock.value".to_owned(),
            parameters: json!({ "value": 42 }),
            lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
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

        let request: Value =
            serde_json::from_str(REQUEST_SCHEMA).expect("request schema should parse");
        assert_eq!(
            string_set(&request["properties"]["op"]["enum"]),
            [BrokerOp::Discover, BrokerOp::RunLifecycle]
                .map(wire_string)
                .into_iter()
                .collect()
        );
        let response: Value =
            serde_json::from_str(RESPONSE_SCHEMA).expect("response schema should parse");
        assert_eq!(
            string_set(&response["properties"]["outcome"]["enum"]),
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
        let schema: Value =
            serde_json::from_str(REQUEST_SCHEMA).expect("request schema should parse");
        let generated = schemars::schema_for!(BrokerRequest);
        assert_eq!(generated_properties(&generated), schema_properties(&schema));
        assert_eq!(
            string_set(&serde_json::to_value(&generated).expect("schema serializes")["required"]),
            string_set(&schema["required"])
        );
        assert_eq!(schema["required"], json!(["op"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn response_fields_match_schema() {
        let schema: Value =
            serde_json::from_str(RESPONSE_SCHEMA).expect("response schema should parse");
        let generated = schemars::schema_for!(BrokerResponse);
        assert_eq!(generated_properties(&generated), schema_properties(&schema));
        assert_eq!(
            string_set(&serde_json::to_value(&generated).expect("schema serializes")["required"]),
            string_set(&schema["required"])
        );
        assert_eq!(schema["required"], json!(["outcome"]));
        assert_eq!(schema["additionalProperties"], json!(false));
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
        let report = LifecycleReport {
            provider_id: "mock".to_owned(),
            preview: "set mock.value from 0 to 42".to_owned(),
            verified: true,
            rolled_back: true,
        };
        let responses = [
            BrokerResponse::lifecycle(report),
            BrokerResponse::error(BrokerErrorKind::OwnerConflict, "held by owner-a"),
        ];
        for response in responses {
            let serialized = serde_json::to_value(&response).expect("response should serialize");
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
