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
//!
//! The broker fails fast rather than degrading. If the worker thread ever stops,
//! including by panicking mid-lifecycle and leaving provider state applied and
//! un-rolled-back, [`serve`] returns an error instead of answering every later
//! request with an internal fault, and the process exits non-zero. A fatal
//! accept error does the same. Deploy the broker under a supervisor (systemd
//! `Restart=on-failure` or equivalent) so that exit becomes a restart, and let
//! the watchdog own recovery of any state left behind.

mod ownership;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use fpsmaxxing_contracts::ChangeRequest;
use fpsmaxxing_contracts::ipc::{
    BrokerErrorKind, BrokerOp, BrokerRequest, BrokerResponse, LifecycleReport,
};
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError};
use fpsmaxxing_ipc::{
    Accepted, FrameError, LocalTransport, PeerAuthorizer, read_frame, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

pub use ownership::{OwnerConflict, OwnershipGuard, OwnershipLedger};

/// Backlog of in-flight requests queued for the single control-plane worker.
const REQUEST_BACKLOG: usize = 64;

/// Connections served at once; further peers wait in the transport's backlog.
pub const MAX_CONNECTIONS: usize = 32;

/// How long a served connection may stall in one direction before it is closed.
///
/// Both directions are bounded by it: an idle peer holding a descriptor, a peer
/// that declares a large frame and then stalls while the reader holds its
/// buffer, and a peer that stops draining its responses until the broker's own
/// write blocks. Without the last of these a peer that never reads could pin a
/// connection slot in `write_all` forever.
pub const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause after a per-connection accept failure, so a repeated one cannot spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);

/// The request handler at the trusted end of the IPC boundary.
///
/// It owns the control plane and enforces the broker's fail-closed policy and
/// single-owner-per-knob invariant. Because it holds the non-`Send` control
/// plane, one instance lives on a dedicated worker thread; use [`spawn_service`]
/// to create it there and reach it through a [`BrokerHandle`].
pub struct BrokerService {
    plane: RefCell<ControlPlane>,
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
            plane: RefCell::new(plane),
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
        BrokerResponse::capabilities(self.plane.borrow().capabilities().clone())
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
        let outcome = self.plane.borrow_mut().run_lifecycle(&change);
        match outcome {
            Ok(result) => BrokerResponse::lifecycle(LifecycleReport {
                provider_id: result.provider_id,
                preview: result.preview,
                verified: result.verified,
                rolled_back: result.rolled_back,
            }),
            Err(error) => wire_error(&error),
        }
    }
}

/// Reduces a control-plane failure to a response that may cross the boundary.
///
/// One match decides both the wire kind and the message, so the two cannot
/// disagree about whether a variant is client-caused when a new one is added.
///
/// Only the two client-caused variants describe the caller's own request, so
/// only they are forwarded verbatim. Every other variant wraps a `SQLite`,
/// serialization, or provider error whose `Display` can carry journal paths and
/// other host detail, which [`fpsmaxxing_contracts::ipc::BrokerErrorBody`]
/// promises never to expose: those are reduced to the stable machine-readable
/// stage kind and the full error is traced locally instead.
fn wire_error(error: &ControlPlaneError) -> BrokerResponse {
    match error {
        ControlPlaneError::UnknownCapability(_) => {
            BrokerResponse::error(BrokerErrorKind::UnknownCapability, error.to_string())
        }
        ControlPlaneError::PolicyDenied(_) => {
            BrokerResponse::error(BrokerErrorKind::PolicyDenied, error.to_string())
        }
        _ => {
            eprintln!("fpsmaxxing-broker: lifecycle failed: {error}");
            BrokerResponse::error(
                BrokerErrorKind::LifecycleFailed,
                format!("lifecycle failed: {}", error.kind()),
            )
        }
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

    /// Resolves once the control-plane worker thread has stopped.
    ///
    /// The worker owns the receiving end of the job channel, so it is dropped
    /// when the thread ends for any reason - including a panic unwinding out of
    /// [`BrokerService::handle`]. [`serve`] waits on this so a broker that has
    /// permanently lost its control plane exits instead of staying up and
    /// answering every request with an internal fault.
    pub async fn worker_stopped(&self) {
        self.jobs.closed().await;
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

/// Serves broker requests over `transport` until the broker can no longer run.
///
/// Each connection is authenticated once against `authorizer` before any request
/// is read, then handled on its own task and dispatched to `broker`. A
/// per-connection fault - including an accept error that aborted a single
/// connection - ends only that connection. At most [`MAX_CONNECTIONS`] are
/// served concurrently, and a connection that stalls for
/// [`CONNECTION_IDLE_TIMEOUT`] in either direction - a peer that sends nothing,
/// or one that stops reading its responses - is closed, so a peer cannot pin a
/// task, a descriptor, or a frame buffer forever.
///
/// This never returns `Ok`: it runs until a fatal condition, then reports it so
/// the process can exit non-zero and a supervisor can restart it.
///
/// # Errors
///
/// Returns an error when the control-plane worker thread has stopped or the
/// transport fails in a way that is not specific to one connection.
pub async fn serve<T>(
    transport: T,
    broker: BrokerHandle,
    authorizer: Arc<dyn PeerAuthorizer>,
) -> io::Result<()>
where
    T: LocalTransport,
{
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (permit, accepted) = tokio::select! {
            biased;
            () = broker.worker_stopped() => {
                return Err(io::Error::other(
                    "broker control-plane worker stopped; restart the broker process",
                ));
            }
            admitted = admit(&transport, &connections) => match admitted {
                Ok(admitted) => admitted,
                Err(error) if is_transient_accept_error(&error) => {
                    eprintln!("fpsmaxxing-broker: accept failed: {error}");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }
                Err(error) => return Err(error),
            },
        };
        let connection_broker = broker.clone();
        let authorizer = Arc::clone(&authorizer);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_connection(accepted, connection_broker, authorizer).await {
                eprintln!("fpsmaxxing-broker: connection error: {error}");
            }
        });
    }
}

/// Waits for a free connection slot, then accepts the next peer into it.
///
/// The slot is taken before the accept so a peer beyond [`MAX_CONNECTIONS`]
/// waits in the transport's own backlog rather than in a task of its own. This
/// whole future is dropped when [`serve`] sees the worker stop, so a queued slot
/// or a pending accept is released rather than leaked.
async fn admit<T>(
    transport: &T,
    connections: &Arc<Semaphore>,
) -> io::Result<(OwnedSemaphorePermit, Accepted<T::Stream>)>
where
    T: LocalTransport,
{
    let permit = Arc::clone(connections)
        .acquire_owned()
        .await
        .map_err(io::Error::other)?;
    Ok((permit, transport.accept().await?))
}

/// Whether an accept failure aborted one connection rather than the endpoint.
///
/// Anything else - a closed or unusable listener, descriptor exhaustion - is
/// persistent: retrying it would spin, so [`serve`] surfaces it as fatal.
fn is_transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted
    )
}

/// Authenticates one peer, then serves its framed requests until it goes away.
///
/// The verified [`fpsmaxxing_ipc::PeerIdentity`] is used for the ACL check and
/// then dropped. Under the interim same-uid ACL every authorized peer is the
/// same identity, so journaling the peer uid and pid against each lifecycle, and
/// authenticating the client-supplied owner label against them, only carry their
/// weight once split-privilege ACLs arrive; both are tracked as follow-up work
/// `fpsm-broker-splitacl`.
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
        let read = tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, read_frame(&mut stream)).await;
        let Ok(read) = read else {
            return Ok(());
        };
        let frame = match read {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(FrameError::Empty) => {
                // The body was zero bytes, so the stream is still at a frame
                // boundary and this peer can keep talking.
                let response =
                    BrokerResponse::error(BrokerErrorKind::Malformed, "frame body is empty");
                write_response(&mut stream, &response).await?;
                continue;
            }
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

/// Writes one response, giving the peer [`CONNECTION_IDLE_TIMEOUT`] to take it.
///
/// A peer that pipelines requests and never reads fills the socket's send buffer
/// and would otherwise park this task in `write_all` for good, holding its
/// connection slot; the timeout turns that into an error that ends the
/// connection and returns the slot.
async fn write_response<W>(writer: &mut W, response: &BrokerResponse) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(response).map_err(io::Error::other)?;
    tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, write_frame(writer, &bytes))
        .await
        .map_err(|_| {
            FrameError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "peer did not accept the response within the idle timeout",
            ))
        })?
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::num::NonZeroU64;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use fpsmaxxing_contracts::ipc::{BrokerErrorKind, BrokerOutcome, BrokerRequest};
    use fpsmaxxing_contracts::{ChangeRequest, ProviderManifest, StateSnapshot};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{Accepted, FrameError, PeerAuthorizer, PeerIdentity, SameUidAuthorizer};
    use fpsmaxxing_mock_provider::MockProvider;
    use fpsmaxxing_provider_sdk::{Provider, ProviderError};
    use serde_json::json;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::mpsc;

    use super::{
        BrokerHandle, BrokerService, CONNECTION_IDLE_TIMEOUT, OwnershipLedger, handle_connection,
    };

    /// Host detail a provider error might carry; it must not reach the wire.
    const HOST_DETAIL: &str = "/var/lib/fpsmaxxing/private/journal.sqlite";

    /// A provider whose apply fails with an error carrying host detail.
    struct LeakyProvider;

    impl Provider for LeakyProvider {
        fn manifest(&self) -> ProviderManifest {
            MockProvider::new(0).manifest()
        }

        fn snapshot(&self) -> Result<StateSnapshot, ProviderError> {
            MockProvider::new(0).snapshot()
        }

        fn preview(&self, request: &ChangeRequest) -> Result<String, ProviderError> {
            MockProvider::new(0).preview(request)
        }

        fn apply(&mut self, _request: &ChangeRequest) -> Result<(), ProviderError> {
            Err(ProviderError::Unavailable(HOST_DETAIL.to_owned()))
        }

        fn verify(&self, _request: &ChangeRequest) -> Result<bool, ProviderError> {
            Ok(true)
        }

        fn rollback(&mut self, _snapshot: &StateSnapshot) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn service() -> (BrokerService, Arc<OwnershipLedger>) {
        service_with(Box::new(MockProvider::new(0)))
    }

    fn service_with(provider: Box<dyn Provider>) -> (BrokerService, Arc<OwnershipLedger>) {
        let plane = ControlPlane::open(provider, ":memory:").expect("control plane should open");
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
        let message = &response
            .error
            .expect("response should carry an error")
            .message;
        assert_eq!(
            message,
            "policy denied request: mock.value is bounded to 0..=100"
        );
    }

    #[test]
    fn an_internal_failure_does_not_cross_the_boundary_verbatim() {
        let (service, _ledger) = service_with(Box::new(LeakyProvider));
        let response = service.handle(BrokerRequest::run_lifecycle(
            "gateway",
            change("mock.value", 42, 30),
        ));
        assert_eq!(error_kind(&response), BrokerErrorKind::LifecycleFailed);
        let message = response
            .error
            .expect("response should carry an error")
            .message;
        assert!(
            !message.contains(HOST_DETAIL),
            "host detail leaked to the client: {message}"
        );
        assert_eq!(message, "lifecycle failed: provider");
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

    /// The uid both the stalled peer and its authorizer are built around.
    const TEST_UID: u32 = 4242;

    /// A peer that sends one frame and then never reads and never writes.
    ///
    /// Writes park forever, standing in for a socket whose send buffer a peer
    /// has stopped draining.
    struct StalledPeer {
        request: Vec<u8>,
        sent: usize,
    }

    impl AsyncRead for StalledPeer {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let peer = self.get_mut();
            let remaining = &peer.request[peer.sent..];
            if remaining.is_empty() {
                return Poll::Pending;
            }
            let take = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..take]);
            peer.sent += take;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for StalledPeer {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_reads_its_response_cannot_pin_the_connection() {
        // The worker is never reached: an undecodable body is answered inline,
        // so the connection only has the stalled write left to block on.
        let (jobs, _worker) = mpsc::channel(1);
        let mut request = 4u32.to_be_bytes().to_vec();
        request.extend_from_slice(b"junk");
        let accepted = Accepted {
            stream: StalledPeer { request, sent: 0 },
            peer: PeerIdentity {
                uid: TEST_UID,
                pid: None,
            },
        };
        let authorizer: Arc<dyn PeerAuthorizer> = Arc::new(SameUidAuthorizer::new(TEST_UID));

        // The outer bound turns a regression into a failure rather than a hang:
        // with the write unbounded it is the only timer left to fire.
        let started = tokio::time::Instant::now();
        let error = tokio::time::timeout(
            CONNECTION_IDLE_TIMEOUT * 4,
            handle_connection(accepted, BrokerHandle { jobs }, authorizer),
        )
        .await
        .expect("the connection must not outlive the idle timeout")
        .expect_err("a peer that never reads must not hold the connection");

        assert!(
            matches!(&error, FrameError::Io(io) if io.kind() == io::ErrorKind::TimedOut),
            "unexpected connection outcome: {error}"
        );
        assert!(
            started.elapsed() >= CONNECTION_IDLE_TIMEOUT,
            "the connection ended before the idle timeout"
        );
    }
}
