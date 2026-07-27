//! Length-delimited framing for the local IPC boundary.
//!
//! Every message is a big-endian `u32` byte length followed by that many bytes
//! of UTF-8 JSON. Frames are bounded so a hostile or buggy peer cannot force an
//! unbounded allocation; an over-long or truncated frame fails closed instead
//! of crashing the reader. The framing is transport-agnostic and works over any
//! `tokio` stream, so the Unix socket used today and a future Windows named pipe
//! share it unchanged.

use std::io::{self, ErrorKind};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame body the broker will read or write, in bytes.
pub const MAX_FRAME_BYTES: u32 = 1 << 20;

/// A failure while reading or writing a length-delimited frame.
#[derive(Debug, Error)]
pub enum FrameError {
    /// The underlying transport read or write failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The declared or supplied frame length exceeds [`MAX_FRAME_BYTES`].
    #[error("frame length {length} exceeds the {max} byte maximum", max = MAX_FRAME_BYTES)]
    TooLarge {
        /// The rejected frame length in bytes.
        length: u64,
    },
}

/// Writes one length-delimited frame and flushes it.
///
/// # Errors
///
/// Returns [`FrameError::TooLarge`] when `body` exceeds [`MAX_FRAME_BYTES`], or
/// [`FrameError::Io`] when the transport write fails.
pub async fn write_frame<W>(writer: &mut W, body: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
    if len > u64::from(MAX_FRAME_BYTES) {
        return Err(FrameError::TooLarge { length: len });
    }
    #[allow(clippy::cast_possible_truncation)]
    let len = len as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one length-delimited frame.
///
/// Returns `Ok(None)` when the peer closes the stream cleanly at a frame
/// boundary, so a serve loop can distinguish a graceful disconnect from a fault.
///
/// # Errors
///
/// Returns [`FrameError::TooLarge`] when the declared length exceeds
/// [`MAX_FRAME_BYTES`] (the reader refuses to allocate it), or [`FrameError::Io`]
/// when the transport fails or a frame is truncated mid-stream.
pub async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        let read = reader.read(&mut len_buf[filled..]).await?;
        if read == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(FrameError::Io(io::Error::new(
                ErrorKind::UnexpectedEof,
                "truncated frame length prefix",
            )));
        }
        filled += read;
    }
    let len = u32::from_be_bytes(len_buf);
    if len == 0 {
        return Err(FrameError::Io(io::Error::new(
            ErrorKind::InvalidData,
            "empty frame",
        )));
    }
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            length: u64::from(len),
        });
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame};

    #[tokio::test]
    async fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"hello broker")
            .await
            .expect("write should succeed");
        let mut cursor = std::io::Cursor::new(buffer);
        let body = read_frame(&mut cursor)
            .await
            .expect("read should succeed")
            .expect("a frame should be present");
        assert_eq!(body, b"hello broker");
    }

    #[tokio::test]
    async fn clean_eof_at_boundary_returns_none() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(
            read_frame(&mut cursor)
                .await
                .expect("clean eof should not error")
                .is_none()
        );
    }

    #[tokio::test]
    async fn oversized_declared_length_is_refused_without_allocating() {
        let mut framed = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        framed.extend_from_slice(b"body");
        let mut cursor = std::io::Cursor::new(framed);
        let error = read_frame(&mut cursor)
            .await
            .expect_err("oversized frame should be refused");
        assert!(matches!(error, FrameError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn truncated_length_prefix_is_an_error() {
        let mut cursor = std::io::Cursor::new(vec![0u8, 0u8]);
        let error = read_frame(&mut cursor)
            .await
            .expect_err("truncated prefix should error");
        assert!(matches!(error, FrameError::Io(_)));
    }
}
