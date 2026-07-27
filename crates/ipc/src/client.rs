//! The unprivileged-side client for the broker IPC boundary.

use std::io;
use std::path::Path;

use fpsmaxxing_contracts::ipc::{BrokerRequest, BrokerResponse};
use thiserror::Error;
use tokio::net::UnixStream;

use crate::frame::{FrameError, read_frame, write_frame};

/// A failure while talking to the broker.
#[derive(Debug, Error)]
pub enum ClientError {
    /// A frame could not be read or written.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The connection could not be established.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A request or response could not be encoded or decoded.
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
    /// The broker closed the connection before answering.
    #[error("broker closed the connection before responding")]
    Closed,
}

/// A client for the broker's authenticated local IPC boundary.
///
/// The unprivileged gateway uses this to reach the privileged broker without
/// linking against the control plane. It speaks the same length-delimited
/// framing and typed [`BrokerRequest`]/[`BrokerResponse`] contract that a
/// Windows named-pipe client would reuse unchanged.
pub struct BrokerClient {
    stream: UnixStream,
}

impl BrokerClient {
    /// Connects to the broker socket at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be reached.
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path).await?,
        })
    }

    /// Sends one request and reads the broker's response.
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be encoded or sent, or the broker
    /// closes the connection or returns an undecodable response.
    pub async fn request(
        &mut self,
        request: &BrokerRequest,
    ) -> Result<BrokerResponse, ClientError> {
        let bytes = serde_json::to_vec(request)?;
        write_frame(&mut self.stream, &bytes).await?;
        let frame = read_frame(&mut self.stream)
            .await?
            .ok_or(ClientError::Closed)?;
        Ok(serde_json::from_slice(&frame)?)
    }
}
