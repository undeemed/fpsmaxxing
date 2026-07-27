//! Local IPC transport, framing, and peer authentication for the broker.
//!
//! The broker's northbound boundary is a local, authenticated request/response
//! channel. This crate keeps the platform-specific transport and the peer ACL
//! behind traits so the Linux Unix-domain-socket path used today and a future
//! Windows named-pipe path share one contract. The typed wire messages live in
//! [`fpsmaxxing_contracts::ipc`]; this crate only frames and moves them.

pub mod auth;
pub mod frame;
pub mod transport;

#[cfg(unix)]
pub mod client;

pub use auth::{AuthError, PeerAuthorizer, PeerIdentity, SameUidAuthorizer};
pub use frame::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame};
pub use transport::{Accepted, LocalTransport};

#[cfg(unix)]
pub use client::{BrokerClient, ClientError};
#[cfg(unix)]
pub use transport::UnixSocketTransport;
