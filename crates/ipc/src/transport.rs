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
    use std::fs::Permissions;
    use std::io;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Accepted, LocalTransport};
    use crate::auth::PeerIdentity;

    /// Filesystem mode applied to the listening socket: owner access only.
    const SOCKET_MODE: u32 = 0o600;

    /// A Unix domain socket implementation of [`LocalTransport`].
    ///
    /// The socket file is removed when the transport is dropped so a restarted
    /// broker can rebind cleanly.
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
        /// The bound socket is restricted to [`SOCKET_MODE`] so the filesystem
        /// denies other users a `connect()` regardless of the inherited umask;
        /// the peer-credential ACL remains the authoritative check.
        ///
        /// # Errors
        ///
        /// Returns an error if `path` holds a non-socket entry, an existing
        /// stale socket cannot be removed, the socket cannot be bound, or its
        /// mode cannot be restricted.
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
            let transport = Self { listener, path };
            std::fs::set_permissions(transport.path(), Permissions::from_mode(SOCKET_MODE))?;
            Ok(transport)
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

    #[cfg(test)]
    mod tests {
        use std::io;
        use std::os::unix::fs::PermissionsExt;

        use super::{SOCKET_MODE, UnixSocketTransport};

        #[tokio::test]
        async fn bind_restricts_the_socket_to_its_owner() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("broker.sock");
            let transport = UnixSocketTransport::bind(&path).expect("socket should bind");
            let mode = std::fs::metadata(transport.path())
                .expect("socket metadata should read")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, SOCKET_MODE);
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
