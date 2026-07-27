//! Fail-closed peer authentication for the local IPC boundary.
//!
//! The transport resolves each connection's [`PeerIdentity`]; a
//! [`PeerAuthorizer`] then decides, fail-closed, whether that peer may talk to
//! the broker. On Linux the identity comes from `SO_PEERCRED` and
//! [`SameUidAuthorizer`] enforces a same-uid ACL. The trait is the portable
//! contract: a Windows named-pipe transport would resolve a client SID and a
//! SID-ACL authorizer would satisfy the same trait without touching callers.

use thiserror::Error;

/// The local identity of a connected peer, resolved by the transport.
///
/// On Unix this is populated from `SO_PEERCRED`. A Windows named-pipe transport
/// would populate the same structure from the client's token, and a Windows
/// [`PeerAuthorizer`] would authorize it against a SID ACL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    /// The peer's user id from `SO_PEERCRED` on Unix.
    pub uid: u32,
    /// The peer's process id when the platform reports it.
    pub pid: Option<i32>,
}

/// Fail-closed authorization of a connected local peer.
///
/// Implementations must return [`Err`] for any peer that is not explicitly
/// permitted, so an unrecognized or spoofed peer is refused rather than served.
pub trait PeerAuthorizer: Send + Sync {
    /// Returns `Ok(())` only for an explicitly permitted peer.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] for any peer that fails the ACL check.
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), AuthError>;
}

/// Why a peer was refused at the IPC boundary.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The peer's uid does not match the uid the broker runs as.
    #[error("peer uid {actual} does not match the broker uid {expected}")]
    ForeignUid {
        /// The uid the broker trusts.
        expected: u32,
        /// The uid the connecting peer presented.
        actual: u32,
    },
}

/// Authorizes only peers whose uid matches the broker's own uid.
///
/// This is the Linux ACL primitive: the broker trusts exactly the uid it runs
/// as, so a process owned by any other user is refused before any request is
/// read.
///
/// This same-uid ACL is the deliberate *interim* policy for the current
/// single-user, Linux-safe mock path, where the broker and its only client run
/// as the same desktop user. It is not the shipping policy for the privileged
/// split described in `docs/ARCHITECTURE.md`: once the broker runs as a service
/// account, an unprivileged gateway is refused by construction. The real
/// privilege-split ACL arrives with the Windows named-pipe SID implementation of
/// [`PeerAuthorizer`], which is tracked separately; callers reach it through
/// this trait, so no call site changes when it lands.
pub struct SameUidAuthorizer {
    expected_uid: u32,
}

impl SameUidAuthorizer {
    /// Builds an authorizer that trusts exactly `expected_uid`.
    #[must_use]
    pub const fn new(expected_uid: u32) -> Self {
        Self { expected_uid }
    }

    /// Builds an authorizer that trusts the calling process's own effective uid.
    ///
    /// The trust anchor is the broker's own credentials, never a by-name
    /// filesystem lookup of the socket: an attacker who can replace the socket
    /// path must not be able to redirect the ACL onto their own uid.
    #[cfg(unix)]
    #[must_use]
    pub fn for_current_process() -> Self {
        Self::new(rustix::process::geteuid().as_raw())
    }

    /// Returns the uid this authorizer trusts.
    #[must_use]
    pub const fn expected_uid(&self) -> u32 {
        self.expected_uid
    }
}

impl PeerAuthorizer for SameUidAuthorizer {
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), AuthError> {
        if peer.uid == self.expected_uid {
            Ok(())
        } else {
            Err(AuthError::ForeignUid {
                expected: self.expected_uid,
                actual: peer.uid,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthError, PeerAuthorizer, PeerIdentity, SameUidAuthorizer};

    #[test]
    fn same_uid_peer_is_authorized() {
        let authorizer = SameUidAuthorizer::new(1000);
        let peer = PeerIdentity {
            uid: 1000,
            pid: Some(42),
        };
        assert!(authorizer.authorize(&peer).is_ok());
    }

    #[test]
    fn foreign_uid_peer_is_refused() {
        let authorizer = SameUidAuthorizer::new(1000);
        let peer = PeerIdentity {
            uid: 1001,
            pid: Some(42),
        };
        let error = authorizer
            .authorize(&peer)
            .expect_err("a foreign uid must be refused");
        assert!(matches!(
            error,
            AuthError::ForeignUid {
                expected: 1000,
                actual: 1001
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_current_process_is_authorized_by_its_own_credentials() {
        let authorizer = SameUidAuthorizer::for_current_process();
        let peer = PeerIdentity {
            uid: rustix::process::geteuid().as_raw(),
            pid: Some(std::process::id().cast_signed()),
        };
        assert!(authorizer.authorize(&peer).is_ok());
    }
}
