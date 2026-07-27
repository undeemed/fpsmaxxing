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
    use std::sync::{Mutex, PoisonError};

    use rustix::fs::Mode;
    use tokio::net::{UnixListener, UnixStream};

    use super::{Accepted, LocalTransport};
    use crate::auth::PeerIdentity;

    /// Umask held across the bind so the socket is created owner-accessible only.
    ///
    /// Masking these bits out of the `0o777` a bind requests leaves mode `0o600`.
    const SOCKET_UMASK: Mode = Mode::XUSR.union(Mode::RWXG).union(Mode::RWXO);

    /// Serializes the process-global umask window that [`bind_owner_only`] opens.
    ///
    /// Two concurrent binds would otherwise interleave their save and restore and
    /// leave the whole process masked at [`SOCKET_UMASK`].
    static BIND_UMASK: Mutex<()> = Mutex::new(());

    /// A Unix domain socket implementation of [`LocalTransport`].
    ///
    /// The socket file is removed when the transport is dropped so a restarted
    /// broker can rebind cleanly. Only an entry that is still a socket is
    /// unlinked, so whatever else may have taken the path survives.
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
        /// The socket is created mode `0o600` so the filesystem denies other
        /// users a `connect()` regardless of the inherited umask; the
        /// peer-credential ACL remains the authoritative check.
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
            let listener = bind_owner_only(&path)?;
            Ok(Self { listener, path })
        }

        /// Returns the bound socket path.
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    /// Binds a listener whose socket file is created owner-accessible only.
    ///
    /// The mode is masked in at creation rather than applied afterwards. A
    /// by-name `chmod` follows symlinks, so a peer able to write the parent
    /// directory could swap the fresh socket for a link and make a privileged
    /// broker restrict an unrelated file, and until it landed the socket would
    /// still carry the inherited umask. `fchmod` on the listener is not an
    /// alternative: it reaches the sockfs inode, not the bound path.
    fn bind_owner_only(path: &Path) -> io::Result<UnixListener> {
        let _serialized = BIND_UMASK.lock().unwrap_or_else(PoisonError::into_inner);
        let restore = rustix::process::umask(SOCKET_UMASK);
        let listener = UnixListener::bind(path);
        rustix::process::umask(restore);
        listener
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
        use std::os::unix::fs::PermissionsExt;

        use super::UnixSocketTransport;

        /// The mode the socket must carry from the moment it exists.
        const OWNER_ONLY: u32 = 0o600;

        #[tokio::test]
        async fn bind_restricts_the_socket_to_its_owner() {
            let dir = tempfile::tempdir().expect("temporary directory should exist");
            let path = dir.path().join("broker.sock");
            let transport = UnixSocketTransport::bind(&path).expect("socket should bind");
            let mode = std::fs::metadata(transport.path())
                .expect("socket metadata should read")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, OWNER_ONLY);
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
