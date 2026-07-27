//! The privileged broker: the trusted side of the IPC boundary.
//!
//! The broker owns the [`ControlPlane`] (provider plus durable journal) and
//! exposes exactly two operations - capability discovery and a bounded provider
//! lifecycle - to authenticated local peers. It layers three fail-closed checks
//! over the control plane:
//!
//! - peer authentication ([`fpsmaxxing_ipc::PeerAuthorizer`]) before any request
//!   is read, so an unauthenticated or foreign peer is refused;
//! - a capability catalog check, so a raw shell command, an arbitrary Registry
//!   path, or any out-of-catalog id is rejected before the provider is touched;
//! - single-owner-per-knob enforcement ([`OwnershipLedger`]), so a second
//!   concurrent owner of a setting is refused.
//!
//! The [`ControlPlane`] is not `Send`, so it is confined to one worker thread
//! ([`spawn_service`]) reached through a `Send` [`BrokerHandle`]. Connections are
//! accepted asynchronously over the [`fpsmaxxing_ipc::LocalTransport`] seam - a
//! Unix domain socket today, a Windows named pipe later - and each connection's
//! decoded requests are dispatched to that worker.

mod ownership;

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};

use fpsmaxxing_contracts::ChangeRequest;
use fpsmaxxing_contracts::ipc::{
    BrokerErrorKind, BrokerOp, BrokerRequest, BrokerResponse, LifecycleReport,
};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError};
use fpsmaxxing_ipc::{
    Accepted, FrameError, LocalTransport, PeerAuthorizer, read_frame, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

pub use ownership::{OwnerConflict, OwnershipGuard, OwnershipLedger};

/// Backlog of in-flight requests queued for the single control-plane worker.
const REQUEST_BACKLOG: usize = 64;

/// The request handler at the trusted end of the IPC boundary.
///
/// It owns the control plane and enforces the broker's fail-closed policy and
/// single-owner-per-knob invariant. Because it holds the non-`Send` control
/// plane, one instance lives on a dedicated worker thread; use [`spawn_service`]
/// to create it there and reach it through a [`BrokerHandle`].
pub struct BrokerService {
    plane: Mutex<ControlPlane>,
    ledger: Arc<OwnershipLedger>,
    catalog: BTreeSet<String>,
}

impl BrokerService {
    /// Builds a broker service around an opened control plane.
    ///
    /// The capability catalog is captured once from the provider manifest and
    /// used to reject out-of-catalog requests before the provider is touched.
    #[must_use]
    pub fn new(plane: ControlPlane, ledger: Arc<OwnershipLedger>) -> Self {
        let catalog = plane
            .capabilities()
            .capabilities
            .iter()
            .map(|capability| capability.id.clone())
            .collect();
        Self {
            plane: Mutex::new(plane),
            ledger,
            catalog,
        }
    }

    /// Handles one decoded request and returns a typed response.
    ///
    /// This is synchronous and may block on the durable journal; it runs on the
    /// worker thread created by [`spawn_service`].
    #[must_use]
    pub fn handle(&self, request: BrokerRequest) -> BrokerResponse {
        match request.op {
            BrokerOp::Discover => self.discover(),
            BrokerOp::RunLifecycle => self.run_lifecycle(request.owner, request.change),
        }
    }

    fn discover(&self) -> BrokerResponse {
        match self.plane.lock() {
            Ok(plane) => BrokerResponse::capabilities(plane.capabilities().clone()),
            Err(_) => {
                BrokerResponse::error(BrokerErrorKind::Internal, "control plane is unavailable")
            }
        }
    }

    fn run_lifecycle(
        &self,
        owner: Option<String>,
        change: Option<ChangeRequest>,
    ) -> BrokerResponse {
        let Some(owner) = owner.filter(|owner| !owner.trim().is_empty()) else {
            return BrokerResponse::error(
                BrokerErrorKind::Malformed,
                "run-lifecycle requires a non-empty owner",
            );
        };
        let Some(change) = change else {
            return BrokerResponse::error(
                BrokerErrorKind::Malformed,
                "run-lifecycle requires a change",
            );
        };
        if !self.catalog.contains(&change.capability_id) {
            return BrokerResponse::error(
                BrokerErrorKind::UnknownCapability,
                format!("capability {} is not in the catalog", change.capability_id),
            );
        }
        let _guard = match self.ledger.acquire(&change.capability_id, &owner) {
            Ok(guard) => guard,
            Err(conflict) => {
                return BrokerResponse::error(BrokerErrorKind::OwnerConflict, conflict.to_string());
            }
        };
        let outcome = match self.plane.lock() {
            Ok(mut plane) => plane.run_lifecycle(&change),
            Err(_) => {
                return BrokerResponse::error(
                    BrokerErrorKind::Internal,
                    "control plane is unavailable",
                );
            }
        };
        match outcome {
            Ok(result) => BrokerResponse::lifecycle(LifecycleReport {
                provider_id: result.provider_id,
                preview: result.preview,
                verified: result.verified,
                rolled_back: result.rolled_back,
            }),
            Err(error) => BrokerResponse::error(error_kind(&error), error.to_string()),
        }
    }
}

fn error_kind(error: &ControlPlaneError) -> BrokerErrorKind {
    match error {
        ControlPlaneError::UnknownCapability(_) => BrokerErrorKind::UnknownCapability,
        ControlPlaneError::PolicyDenied(_) => BrokerErrorKind::PolicyDenied,
        _ => BrokerErrorKind::LifecycleFailed,
    }
}

struct Job {
    request: BrokerRequest,
    respond: oneshot::Sender<BrokerResponse>,
}

/// A cloneable, `Send` handle to the control-plane worker thread.
///
/// Connection tasks dispatch decoded requests through this handle; the worker
/// serializes them onto the single control plane and returns typed responses.
#[derive(Clone)]
pub struct BrokerHandle {
    jobs: mpsc::Sender<Job>,
}

impl BrokerHandle {
    /// Dispatches one request to the worker and awaits its typed response.
    ///
    /// If the worker has stopped, a typed [`BrokerErrorKind::Internal`] response
    /// is returned rather than an error, so a connection loop stays uniform.
    pub async fn dispatch(&self, request: BrokerRequest) -> BrokerResponse {
        let (respond, response) = oneshot::channel();
        if self.jobs.send(Job { request, respond }).await.is_err() {
            return BrokerResponse::error(BrokerErrorKind::Internal, "broker worker has stopped");
        }
        response.await.unwrap_or_else(|_| {
            BrokerResponse::error(
                BrokerErrorKind::Internal,
                "broker worker dropped the request",
            )
        })
    }
}

/// Spawns the worker thread that owns the control plane and returns a handle.
///
/// The non-`Send` [`BrokerService`] is constructed by `build` on the worker
/// thread that will own it, so the control plane never crosses a thread
/// boundary. The returned future resolves once `build` reports success.
///
/// # Errors
///
/// Returns an error if the worker thread cannot be spawned, `build` fails, or
/// the worker exits before reporting readiness.
pub async fn spawn_service<F>(build: F) -> io::Result<BrokerHandle>
where
    F: FnOnce() -> io::Result<BrokerService> + Send + 'static,
{
    let (jobs_tx, mut jobs_rx) = mpsc::channel::<Job>(REQUEST_BACKLOG);
    let (ready_tx, ready_rx) = oneshot::channel::<io::Result<()>>();
    std::thread::Builder::new()
        .name("fpsmaxxing-broker-worker".to_owned())
        .spawn(move || match build() {
            Ok(service) => {
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = jobs_rx.blocking_recv() {
                    let response = service.handle(job.request);
                    let _ = job.respond.send(response);
                }
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
        })?;
    match ready_rx.await {
        Ok(Ok(())) => Ok(BrokerHandle { jobs: jobs_tx }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::other("broker worker exited during startup")),
    }
}

/// Serves broker requests over `transport` until it stops accepting.
///
/// Each connection is authenticated once against `authorizer` before any request
/// is read, then handled on its own task and dispatched to `broker`. A transient
/// accept error is logged and the loop continues; a per-connection fault ends
/// only that connection, never the broker.
///
/// # Errors
///
/// Returns an error only if the transport itself fails irrecoverably.
pub async fn serve<T>(
    transport: T,
    broker: BrokerHandle,
    authorizer: Arc<dyn PeerAuthorizer>,
) -> io::Result<()>
where
    T: LocalTransport,
{
    loop {
        let accepted = match transport.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("fpsmaxxing-broker: accept failed: {error}");
                continue;
            }
        };
        let broker = broker.clone();
        let authorizer = Arc::clone(&authorizer);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(accepted, broker, authorizer).await {
                eprintln!("fpsmaxxing-broker: connection error: {error}");
            }
        });
    }
}

async fn handle_connection<S>(
    accepted: Accepted<S>,
    broker: BrokerHandle,
    authorizer: Arc<dyn PeerAuthorizer>,
) -> Result<(), FrameError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let Accepted { mut stream, peer } = accepted;
    if let Err(error) = authorizer.authorize(&peer) {
        let response = BrokerResponse::error(BrokerErrorKind::Unauthenticated, error.to_string());
        let _ = write_response(&mut stream, &response).await;
        return Ok(());
    }
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(FrameError::TooLarge { length }) => {
                let response = BrokerResponse::error(
                    BrokerErrorKind::Malformed,
                    format!("frame length {length} exceeds the maximum"),
                );
                let _ = write_response(&mut stream, &response).await;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let response = match serde_json::from_slice::<BrokerRequest>(&frame) {
            Ok(request) => broker.dispatch(request).await,
            Err(error) => BrokerResponse::error(
                BrokerErrorKind::Malformed,
                format!("invalid request: {error}"),
            ),
        };
        write_response(&mut stream, &response).await?;
    }
}

async fn write_response<W>(writer: &mut W, response: &BrokerResponse) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(response).map_err(io::Error::other)?;
    write_frame(writer, &bytes).await
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;

    use fpsmaxxing_contracts::ChangeRequest;
    use fpsmaxxing_contracts::ipc::{BrokerErrorKind, BrokerOutcome, BrokerRequest};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_mock_provider::MockProvider;
    use serde_json::json;

    use super::{BrokerService, OwnershipLedger};

    fn service() -> (BrokerService, Arc<OwnershipLedger>) {
        let plane = ControlPlane::open(Box::new(MockProvider::new(0)), ":memory:")
            .expect("control plane should open");
        let ledger = Arc::new(OwnershipLedger::new());
        (BrokerService::new(plane, Arc::clone(&ledger)), ledger)
    }

    fn change(capability_id: &str, value: u64, lease: u64) -> ChangeRequest {
        ChangeRequest {
            capability_id: capability_id.to_owned(),
            parameters: json!({ "value": value }),
            lease_seconds: NonZeroU64::new(lease).expect("lease is non-zero"),
        }
    }

    fn error_kind(response: &fpsmaxxing_contracts::ipc::BrokerResponse) -> BrokerErrorKind {
        response
            .error
            .as_ref()
            .expect("response should carry an error")
            .kind
    }

    #[test]
    fn discover_returns_the_catalog() {
        let (service, _ledger) = service();
        let response = service.handle(BrokerRequest::discover());
        assert_eq!(response.outcome, BrokerOutcome::Capabilities);
        let manifest = response
            .capabilities
            .expect("capabilities should be present");
        assert!(manifest.capabilities.iter().any(|c| c.id == "mock.value"));
    }

    #[test]
    fn full_lifecycle_succeeds() {
        let (service, _ledger) = service();
        let response = service.handle(BrokerRequest::run_lifecycle(
            "gateway",
            change("mock.value", 42, 30),
        ));
        assert_eq!(response.outcome, BrokerOutcome::Lifecycle);
        let report = response
            .lifecycle
            .expect("lifecycle report should be present");
        assert!(report.verified && report.rolled_back);
    }

    #[test]
    fn out_of_catalog_capability_is_refused() {
        let (service, _ledger) = service();
        for capability in ["shell.exec", "registry.set", "mock.unknown"] {
            let response = service.handle(BrokerRequest::run_lifecycle(
                "gateway",
                change(capability, 1, 30),
            ));
            assert_eq!(
                error_kind(&response),
                BrokerErrorKind::UnknownCapability,
                "{capability} must be refused as out-of-catalog"
            );
        }
    }

    #[test]
    fn missing_owner_is_malformed() {
        let (service, _ledger) = service();
        let request = BrokerRequest {
            op: fpsmaxxing_contracts::ipc::BrokerOp::RunLifecycle,
            owner: None,
            change: Some(change("mock.value", 1, 30)),
        };
        assert_eq!(
            error_kind(&service.handle(request)),
            BrokerErrorKind::Malformed
        );
    }

    #[test]
    fn out_of_bounds_value_is_policy_denied() {
        let (service, _ledger) = service();
        let response = service.handle(BrokerRequest::run_lifecycle(
            "gateway",
            change("mock.value", 101, 30),
        ));
        assert_eq!(error_kind(&response), BrokerErrorKind::PolicyDenied);
    }

    #[test]
    fn second_owner_of_a_held_knob_is_refused() {
        let (service, ledger) = service();
        let _guard = ledger
            .acquire("mock.value", "owner-a")
            .expect("first owner should hold the knob");
        let response = service.handle(BrokerRequest::run_lifecycle(
            "owner-b",
            change("mock.value", 42, 30),
        ));
        assert_eq!(error_kind(&response), BrokerErrorKind::OwnerConflict);
    }
}
