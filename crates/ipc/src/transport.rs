//! The local IPC transport seam.
//!
//! [`LocalTransport`] abstracts how the broker accepts authenticated local
//! connections and resolves each peer's identity. [`UnixSocketTransport`] is the
//! Linux implementation over a Unix domain socket; a Windows named-pipe
//! implementation would satisfy the same trait later, so the broker's serve
//! loop never names a concrete transport.

use std::future::Future;
use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::auth::PeerIdentity;

/// An accepted local connection together with its resolved peer identity.
pub struct Accepted<S> {
    /// The bidirectional byte stream for this connection.
    pub stream: S,
    /// The peer identity the transport resolved at accept time.
    pub peer: PeerIdentity,
}

/// A bound local IPC endpoint that accepts authenticated peer connections.
///
/// The associated [`LocalTransport::Stream`] carries the framed request and
/// response bytes, and [`LocalTransport::accept`] resolves the peer identity so
/// the broker can authorize it before reading any request.
pub trait LocalTransport {
    /// The accepted connection's byte stream.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Accepts the next inbound connection and resolves its peer identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying endpoint cannot accept a connection
    /// or the peer identity cannot be resolved.
    fn accept(&self) -> impl Future<Output = io::Result<Accepted<Self::Stream>>> + Send;
}

#[cfg(unix)]
pub use unix::UnixSocketTransport;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Accepted, LocalTransport};
    use crate::auth::PeerIdentity;

    /// A Unix domain socket implementation of [`LocalTransport`].
    ///
    /// The socket file is removed when the transport is dropped so a restarted
    /// broker can rebind cleanly.
    pub struct UnixSocketTransport {
        listener: UnixListener,
        path: PathBuf,
    }

    impl UnixSocketTransport {
        /// Binds a fresh socket at `path`, replacing a stale socket file.
        ///
        /// # Errors
        ///
        /// Returns an error if an existing non-socket file cannot be removed or
        /// the socket cannot be bound.
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            let path = path.as_ref().to_path_buf();
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&path)?;
            Ok(Self { listener, path })
        }

        /// Returns the uid that owns the bound socket file.
        ///
        /// A freshly created socket is owned by the broker's effective uid, so
        /// this is the uid a [`crate::SameUidAuthorizer`] should trust.
        ///
        /// # Errors
        ///
        /// Returns an error if the socket file metadata cannot be read.
        pub fn owner_uid(&self) -> io::Result<u32> {
            Ok(std::fs::metadata(&self.path)?.uid())
        }

        /// Returns the bound socket path.
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl LocalTransport for UnixSocketTransport {
        type Stream = UnixStream;

        async fn accept(&self) -> io::Result<Accepted<UnixStream>> {
            let (stream, _addr) = self.listener.accept().await?;
            let credentials = stream.peer_cred()?;
            let peer = PeerIdentity {
                uid: credentials.uid(),
                pid: credentials.pid(),
            };
            Ok(Accepted { stream, peer })
        }
    }

    impl Drop for UnixSocketTransport {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
