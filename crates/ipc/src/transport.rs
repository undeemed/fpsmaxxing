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
    use std::os::unix::fs::FileTypeExt;
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Accepted, LocalTransport};
    use crate::auth::PeerIdentity;

    /// A Unix domain socket implementation of [`LocalTransport`].
    ///
    /// The socket file is removed when the transport is dropped so a restarted
    /// broker can rebind cleanly. Only an entry that is still a socket is
    /// unlinked, so whatever else may have taken the path survives.
    ///
    /// Confidentiality is not this type's job. The bind does not touch the
    /// socket file's mode, because the only ways to pin one are a by-name
    /// `chmod` - which follows symlinks, so a peer able to write the parent
    /// directory could redirect it onto an unrelated file - and a window in the
    /// process-global umask, which silently strips bits from every other
    /// thread's file and directory creation for its duration. `fchmod` is not an
    /// alternative: it reaches the sockfs inode, not the bound path. Two checks
    /// carry that weight instead: the caller places the socket in a directory
    /// only its owner may traverse (see the broker's private directory), and the
    /// [`crate::PeerAuthorizer`] gate authenticates every peer from
    /// `SO_PEERCRED` before a request is read.
    #[derive(Debug)]
    pub struct UnixSocketTransport {
        listener: UnixListener,
        path: PathBuf,
    }

    impl UnixSocketTransport {
        /// Binds a fresh socket at `path`, replacing a stale socket file.
        ///
        /// Only an existing socket is replaced. A regular file, a directory, or
        /// a symlink already at `path` fails closed with
        /// [`io::ErrorKind::AlreadyExists`] rather than being deleted, so a
        /// mistyped `--socket` on a privileged broker cannot destroy an
        /// unrelated file.
        ///
        /// # Errors
        ///
        /// Returns an error if `path` holds a non-socket entry, an existing
        /// stale socket cannot be removed, or the socket cannot be bound.
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            let path = path.as_ref().to_path_buf();
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&path)?,
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&path)?;
            Ok(Self { listener, path })
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
            if std::fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io;

        use tokio::net::UnixStream;

        use super::{LocalTransport, UnixSocketTransport};

        #[tokio::test]
        async fn a_bound_socket_accepts_a_local_peer() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("broker.sock");
            let transport = UnixSocketTransport::bind(&path).expect("socket should bind");
            assert_eq!(transport.path(), path);

            let connect = tokio::spawn(async move { UnixStream::connect(&path).await });
            let accepted = transport
                .accept()
                .await
                .expect("the peer should be accepted");
            connect
                .await
                .expect("the connecting task should finish")
                .expect("the peer should connect");
            assert_eq!(
                accepted.peer.uid,
                rustix::process::geteuid().as_raw(),
                "a local peer's credentials should be resolved at accept time"
            );
        }

        #[tokio::test]
        async fn bind_replaces_a_stale_socket() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("broker.sock");
            // std's listener leaves the socket file behind, like a crashed broker.
            drop(std::os::unix::net::UnixListener::bind(&path).expect("stale socket should bind"));
            UnixSocketTransport::bind(&path).expect("a stale socket should be replaced");
        }

        #[tokio::test]
        async fn bind_refuses_to_delete_a_non_socket_path() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            for name in ["regular-file", "directory", "symlink"] {
                let path = dir.path().join(name);
                match name {
                    "directory" => std::fs::create_dir(&path).expect("directory should create"),
                    "symlink" => std::os::unix::fs::symlink("/dev/null", &path)
                        .expect("symlink should create"),
                    _ => std::fs::write(&path, b"not a socket").expect("file should write"),
                }
                let error = UnixSocketTransport::bind(&path)
                    .expect_err("a non-socket path must not be replaced");
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{name}");
                assert!(
                    std::fs::symlink_metadata(&path).is_ok(),
                    "{name} must survive the refused bind"
                );
            }
        }
    }
}
