//! Linux-safe end-to-end coverage for the broker's authenticated IPC boundary.
//!
//! A gateway-side [`BrokerClient`] connects over a real Unix domain socket and
//! drives a full mock-provider lifecycle through the broker, and the durable
//! journal is inspected to prove the transaction landed. The remaining tests
//! prove the fail-closed boundaries: a foreign peer is refused, a second owner
//! of a held knob is refused, malformed frames are rejected without taking the
//! broker down, and a broker that loses its control-plane worker shuts down
//! instead of serving on without one.
//!
//! The Unix domain socket transport is the only one implemented, so these tests
//! compile only on Unix.
#![cfg(unix)]

use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fpsmaxxing_broker::{BrokerService, MAX_CONNECTIONS, OwnershipLedger, serve, spawn_service};
use fpsmaxxing_contracts::ipc::{BrokerErrorKind, BrokerOutcome, BrokerRequest, BrokerResponse};
use fpsmaxxing_contracts::{ChangeRequest, ProviderManifest, StateSnapshot};
use fpsmaxxing_control_plane::ControlPlane;
use fpsmaxxing_ipc::{
    BrokerClient, MAX_FRAME_BYTES, PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport,
    read_frame, write_frame,
};
use fpsmaxxing_mock_provider::MockProvider;
use fpsmaxxing_provider_sdk::{Provider, ProviderError};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

/// A provider that panics mid-lifecycle, standing in for a worker-thread fault.
struct PanickingProvider;

impl Provider for PanickingProvider {
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
        panic!("provider faulted while applying");
    }

    fn verify(&self, _request: &ChangeRequest) -> Result<bool, ProviderError> {
        unreachable!("apply panics first");
    }

    fn rollback(&mut self, _snapshot: &StateSnapshot) -> Result<(), ProviderError> {
        unreachable!("apply panics first");
    }
}

/// A running broker bound to a temporary socket and journal.
struct TestBroker {
    socket: PathBuf,
    journal: PathBuf,
    ledger: Arc<OwnershipLedger>,
    serve_task: JoinHandle<io::Result<()>>,
    _dir: TempDir,
}

impl Drop for TestBroker {
    fn drop(&mut self) {
        self.serve_task.abort();
    }
}

/// Starts a broker; `trusted_uid` overrides the ACL for the foreign-peer test.
async fn start(trusted_uid: Option<u32>) -> TestBroker {
    start_with(trusted_uid, || Box::new(MockProvider::new(0))).await
}

async fn start_with<F>(trusted_uid: Option<u32>, provider: F) -> TestBroker
where
    F: FnOnce() -> Box<dyn Provider> + Send + 'static,
{
    let dir = tempfile::tempdir().expect("temporary directory should exist");
    let socket = dir.path().join("broker.sock");
    let journal = dir.path().join("journal.sqlite");

    let ledger = Arc::new(OwnershipLedger::new());
    let build_ledger = Arc::clone(&ledger);
    let build_journal = journal.clone();
    let broker = spawn_service(move || {
        let plane = ControlPlane::open(provider(), &build_journal).map_err(io::Error::other)?;
        Ok(BrokerService::new(plane, build_ledger))
    })
    .await
    .expect("broker service should start");

    let transport = UnixSocketTransport::bind(&socket).expect("socket should bind");
    let authorizer: Arc<dyn PeerAuthorizer> = match trusted_uid {
        Some(uid) => Arc::new(SameUidAuthorizer::new(uid)),
        None => Arc::new(SameUidAuthorizer::for_current_process()),
    };
    let serve_task = tokio::spawn(serve(transport, broker, authorizer));

    TestBroker {
        socket,
        journal,
        ledger,
        serve_task,
        _dir: dir,
    }
}

fn change(value: u64) -> ChangeRequest {
    ChangeRequest {
        capability_id: "mock.value".to_owned(),
        parameters: json!({ "value": value }),
        lease_seconds: NonZeroU64::new(30).expect("lease is non-zero"),
    }
}

fn journal_stages(journal: &Path) -> Vec<String> {
    let connection = Connection::open(journal).expect("journal should open");
    connection
        .prepare("SELECT stage FROM experiment_journal ORDER BY sequence")
        .expect("query should prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query should execute")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should read")
}

fn expect_error(response: &BrokerResponse) -> BrokerErrorKind {
    match response {
        BrokerResponse::Error { error } => error.kind,
        other => panic!("expected a typed error, got {other:?}"),
    }
}

fn expect_capabilities(response: BrokerResponse) -> ProviderManifest {
    match response {
        BrokerResponse::Capabilities { capabilities } => capabilities,
        other => panic!("expected the capability catalog, got {other:?}"),
    }
}

#[tokio::test]
async fn client_runs_a_journaled_lifecycle_over_the_socket() {
    let broker = start(None).await;
    let mut client = BrokerClient::connect(&broker.socket)
        .await
        .expect("client should connect");

    let discover = client
        .request(&BrokerRequest::discover())
        .await
        .expect("discover should respond");
    let manifest = expect_capabilities(discover);
    assert!(manifest.capabilities.iter().any(|c| c.id == "mock.value"));

    let lifecycle = client
        .request(&BrokerRequest::run_lifecycle("gateway", change(42)))
        .await
        .expect("lifecycle should respond");
    let BrokerResponse::Lifecycle { lifecycle } = lifecycle else {
        panic!("a completed lifecycle should answer with its report");
    };
    assert_eq!(lifecycle.provider_id, "mock");
    assert!(lifecycle.verified && lifecycle.rolled_back);

    assert_eq!(
        journal_stages(&broker.journal),
        [
            "snapshot",
            "preview",
            "apply-intent",
            "apply",
            "verify",
            "rollback",
            "rollback-verify",
            "completed"
        ]
    );
}

#[tokio::test]
async fn foreign_peer_is_refused() {
    // Trust an impossible uid so the connecting peer (our own uid) is foreign.
    let broker = start(Some(u32::MAX)).await;
    let mut stream = UnixStream::connect(&broker.socket)
        .await
        .expect("client should connect");

    let frame = read_frame(&mut stream)
        .await
        .expect("a rejection frame should read")
        .expect("the broker should proactively reject");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(expect_error(&response), BrokerErrorKind::Unauthenticated);

    // No lifecycle ever reached the journal.
    assert!(journal_stages(&broker.journal).is_empty());
}

#[tokio::test]
async fn second_owner_of_a_held_knob_is_refused() {
    let broker = start(None).await;
    let guard = broker
        .ledger
        .acquire("mock.value", "owner-a")
        .expect("owner-a should hold the knob");

    let mut client = BrokerClient::connect(&broker.socket)
        .await
        .expect("client should connect");
    let denied = client
        .request(&BrokerRequest::run_lifecycle("owner-b", change(42)))
        .await
        .expect("conflict should respond");
    assert_eq!(expect_error(&denied), BrokerErrorKind::OwnerConflict);

    // Releasing the knob lets the next owner through over the same connection.
    drop(guard);
    let granted = client
        .request(&BrokerRequest::run_lifecycle("owner-b", change(7)))
        .await
        .expect("released knob should respond");
    assert_eq!(granted.outcome(), BrokerOutcome::Lifecycle);
}

#[tokio::test]
async fn malformed_frames_are_rejected_without_crashing_the_broker() {
    let broker = start(None).await;

    // Valid framing, non-JSON body: rejected, and the connection keeps serving.
    let mut stream = UnixStream::connect(&broker.socket)
        .await
        .expect("client should connect");
    write_frame(&mut stream, b"this is not json")
        .await
        .expect("garbage should send");
    let frame = read_frame(&mut stream)
        .await
        .expect("a response should read")
        .expect("the broker should answer");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(expect_error(&response), BrokerErrorKind::Malformed);

    let discover = serde_json::to_vec(&BrokerRequest::discover()).expect("discover should encode");
    write_frame(&mut stream, &discover)
        .await
        .expect("discover should send");
    let frame = read_frame(&mut stream)
        .await
        .expect("a response should read")
        .expect("the connection should survive one bad frame");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(response.outcome(), BrokerOutcome::Capabilities);

    // An oversized declared length is refused without allocation.
    let mut stream = UnixStream::connect(&broker.socket)
        .await
        .expect("client should connect");
    stream
        .write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes())
        .await
        .expect("oversized length should send");
    stream.flush().await.expect("flush should succeed");
    let frame = read_frame(&mut stream)
        .await
        .expect("a response should read")
        .expect("the broker should answer");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(expect_error(&response), BrokerErrorKind::Malformed);

    // A zero-length frame is answered and the same connection keeps serving.
    let mut stream = UnixStream::connect(&broker.socket)
        .await
        .expect("client should connect");
    stream
        .write_all(&0u32.to_be_bytes())
        .await
        .expect("empty frame should send");
    stream.flush().await.expect("flush should succeed");
    let frame = read_frame(&mut stream)
        .await
        .expect("a response should read")
        .expect("the broker should answer an empty frame");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(expect_error(&response), BrokerErrorKind::Malformed);
    write_frame(&mut stream, &discover)
        .await
        .expect("discover should send");
    let frame = read_frame(&mut stream)
        .await
        .expect("a response should read")
        .expect("the connection should survive an empty frame");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(response.outcome(), BrokerOutcome::Capabilities);

    // A brand-new client still works, proving the broker never crashed.
    let mut client = BrokerClient::connect(&broker.socket)
        .await
        .expect("client should reconnect");
    let discover = client
        .request(&BrokerRequest::discover())
        .await
        .expect("discover should respond");
    assert_eq!(discover.outcome(), BrokerOutcome::Capabilities);
}

#[tokio::test]
async fn concurrent_connections_are_capped_and_the_slot_is_returned() {
    let broker = start(None).await;

    // Fill every slot with a peer that connects and then says nothing.
    let mut idle = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        idle.push(
            BrokerClient::connect(&broker.socket)
                .await
                .expect("an in-cap client should connect"),
        );
    }
    // Drive one of them so the broker has demonstrably accepted the whole batch.
    let served = idle[0]
        .request(&BrokerRequest::discover())
        .await
        .expect("an in-cap client should be served");
    assert_eq!(served.outcome(), BrokerOutcome::Capabilities);

    // The kernel completes this connect, but the broker must not serve it yet.
    // The request is written once so the retry below cannot desynchronize it.
    let mut queued = UnixStream::connect(&broker.socket)
        .await
        .expect("an over-cap client should still reach the backlog");
    let discover = serde_json::to_vec(&BrokerRequest::discover()).expect("discover should encode");
    write_frame(&mut queued, &discover)
        .await
        .expect("discover should send");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), read_frame(&mut queued))
            .await
            .is_err(),
        "an over-cap connection must wait for a free slot"
    );

    // Releasing a slot lets the queued peer through.
    idle.truncate(MAX_CONNECTIONS - 1);
    let frame = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut queued))
        .await
        .expect("a freed slot should admit the queued peer")
        .expect("a response should read")
        .expect("the queued peer should be served");
    let response: BrokerResponse = serde_json::from_slice(&frame).expect("response should be JSON");
    assert_eq!(response.outcome(), BrokerOutcome::Capabilities);
}

#[tokio::test]
async fn a_dead_control_plane_worker_shuts_the_broker_down() {
    let mut broker = start_with(None, || Box::new(PanickingProvider)).await;
    let mut client = BrokerClient::connect(&broker.socket)
        .await
        .expect("client should connect");

    // The panic unwinds out of the worker thread; this request is answered as an
    // internal fault rather than hanging.
    let faulted = client
        .request(&BrokerRequest::run_lifecycle("gateway", change(42)))
        .await
        .expect("the faulted request should still be answered");
    assert_eq!(expect_error(&faulted), BrokerErrorKind::Internal);

    // The broker must not stay up without a control plane: serve reports the
    // loss so the process exits and a supervisor restarts it.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), &mut broker.serve_task)
        .await
        .expect("serve should stop once the worker is gone")
        .expect("the serve task should not itself panic");
    let error = outcome.expect_err("serve must report the lost worker");
    assert!(
        error.to_string().contains("worker stopped"),
        "unexpected shutdown reason: {error}"
    );
}
