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
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Accepted, LocalTransport};
    use crate::auth::PeerIdentity;

    /// Which socket file a bound transport is responsible for unlinking.
    ///
    /// A path is not an identity: another instance may have replaced the entry
    /// since the bind, and unlinking that one would leave a live broker no
    /// client can reach. The device and inode pin the entry this transport
    /// actually created.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct SocketIdentity {
        dev: u64,
        ino: u64,
    }

    impl SocketIdentity {
        /// Reads the identity of the socket entry currently at `path`.
        fn of(path: &Path) -> io::Result<Self> {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| named(path, "inspected", &error))?;
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is no longer a socket", path.display()),
                ));
            }
            Ok(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
    }

    /// A Unix domain socket implementation of [`LocalTransport`].
    ///
    /// The socket file is removed when the transport is dropped so a restarted
    /// broker can rebind cleanly. Only the entry this transport bound is
    /// unlinked - matched by device and inode, not by name - so neither an
    /// unrelated entry nor a successor instance's live endpoint is destroyed.
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
        identity: SocketIdentity,
    }

    impl UnixSocketTransport {
        /// Binds a fresh socket at `path`, replacing a stale socket file.
        ///
        /// A socket file outlives the process that bound it, so a socket
        /// already at `path` is unlinked and rebound - it is the file a crashed
        /// broker leaves behind. Nothing here has to tell that file apart from
        /// a live endpoint, because this bind is not what keeps a second broker
        /// out: the caller takes an exclusive lock in its own private directory
        /// before it reaches this code (see the broker's single-instance lock),
        /// so a second instance has already been refused by the time this runs.
        /// A probe would be the weaker guard anyway - stat, probe, unlink, and
        /// bind are four steps, and two brokers can both find the same path
        /// stale.
        ///
        /// A regular file, a directory, or a symlink already at `path` fails
        /// closed with [`io::ErrorKind::AlreadyExists`] rather than being
        /// deleted, so a mistyped `--socket` on a privileged broker cannot
        /// destroy an unrelated file.
        ///
        /// Every failure names `path`. The broker has three configurable paths,
        /// and `std`'s io errors carry none of them.
        ///
        /// # Errors
        ///
        /// Returns an error if `path` holds a non-socket entry, cannot be
        /// inspected, cannot be removed, or the socket cannot be bound.
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            let path = path.as_ref().to_path_buf();
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(named(&path, "removed", &error)),
                    }
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(named(&path, "inspected", &error)),
            }
            let listener =
                UnixListener::bind(&path).map_err(|error| named(&path, "bound", &error))?;
            let identity = SocketIdentity::of(&path).inspect_err(|_| {
                let _ = std::fs::remove_file(&path);
            })?;
            Ok(Self {
                listener,
                path,
                identity,
            })
        }

        /// Returns the bound socket path.
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    /// Names `path` in `error`, which `std`'s io errors never carry themselves.
    ///
    /// The socket is one of three paths the broker can be pointed at, so a bare
    /// `Permission denied` leaves an operator no way to tell which of them the
    /// start-up failed on.
    fn named(path: &Path, action: &str, error: &io::Error) -> io::Error {
        io::Error::new(
            error.kind(),
            format!("{} cannot be {action}: {error}", path.display()),
        )
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
            if SocketIdentity::of(&self.path).is_ok_and(|identity| identity == self.identity) {
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
        async fn a_bind_failure_names_the_socket_path() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("absent").join("broker.sock");
            let error = UnixSocketTransport::bind(&path)
                .expect_err("no socket can be bound under a directory that is not there");
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "the broker takes three configurable paths, so a failure must say which: {error}"
            );
        }

        #[tokio::test]
        async fn drop_spares_a_socket_this_transport_did_not_bind() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("broker.sock");
            let transport = UnixSocketTransport::bind(&path).expect("socket should bind");

            // A successor takes the path over, as a manual rebind would.
            std::fs::remove_file(&path).expect("the bound socket should be removed");
            let successor =
                std::os::unix::net::UnixListener::bind(&path).expect("the successor should bind");

            drop(transport);
            assert!(
                std::fs::symlink_metadata(&path).is_ok(),
                "an older instance must not unlink a live successor's endpoint"
            );
            drop(successor);
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
