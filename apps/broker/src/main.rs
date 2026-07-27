//! Privileged provider broker process.
//!
//! Owns the control plane and serves capability discovery and bounded provider
//! lifecycles to authenticated local peers over a Unix domain socket. No raw
//! shell, Registry path, or hardware primitive crosses this boundary.
//!
//! Only the Unix domain socket transport is implemented; the Windows named-pipe
//! transport is deliberately out of scope, so the binary refuses to run there.

#[cfg(unix)]
#[tokio::main]
async fn main() {
    if let Err(error) = unix::run().await {
        eprintln!("fpsmaxxing-broker: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "fpsmaxxing-broker: only the Unix domain socket transport is implemented; the Windows named-pipe transport is not yet available"
    );
    std::process::exit(1);
}

#[cfg(unix)]
mod unix {
    use std::env;
    use std::error::Error;
    use std::io;
    use std::sync::Arc;

    use fpsmaxxing_broker::{BrokerService, OwnershipLedger, serve, spawn_service};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport};
    use fpsmaxxing_mock_provider::MockProvider;

    pub async fn run() -> Result<(), Box<dyn Error>> {
        let socket_path = arg_value("--socket")
            .or_else(|| env::var("FPSMAXXING_BROKER_SOCKET").ok())
            .unwrap_or_else(|| "fpsmaxxing-broker.sock".to_owned());
        let journal_path = arg_value("--journal")
            .or_else(|| env::var("FPSMAXXING_JOURNAL_PATH").ok())
            .unwrap_or_else(|| "fpsmaxxing-journal.sqlite".to_owned());

        let ledger = Arc::new(OwnershipLedger::new());
        let broker = spawn_service(move || {
            let provider = Box::new(MockProvider::new(0));
            let plane = ControlPlane::open(provider, &journal_path).map_err(io::Error::other)?;
            Ok(BrokerService::new(plane, ledger))
        })
        .await?;

        let transport = UnixSocketTransport::bind(&socket_path)?;
        let broker_uid = transport.owner_uid()?;
        let authorizer: Arc<dyn PeerAuthorizer> = Arc::new(SameUidAuthorizer::new(broker_uid));
        println!("fpsmaxxing-broker: listening on {socket_path} (same-uid ACL, uid {broker_uid})");
        serve(transport, broker, authorizer).await?;
        Ok(())
    }

    fn arg_value(flag: &str) -> Option<String> {
        env::args()
            .skip(1)
            .collect::<Vec<_>>()
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    }
}
