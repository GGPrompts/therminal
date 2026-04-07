//! Shared length-prefixed frame I/O helpers.
//!
//! The wire format is: `[4-byte BE length][payload bytes]`.
//! Maximum frame size is `MAX_FRAME_SIZE` (1 MiB) from therminal-protocol.

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use therminal_protocol::daemon::MAX_FRAME_SIZE;

/// Read a single length-prefixed frame from any async reader.
///
/// Returns `Ok(None)` on clean EOF, `Ok(Some(bytes))` with the payload,
/// or `Err` on protocol violations (frame too large) or I/O errors.
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {msg_len} bytes (max {MAX_FRAME_SIZE})");
    }

    let mut payload = vec![0u8; msg_len];
    reader
        .read_exact(&mut payload)
        .await
        .context("failed to read frame payload")?;

    Ok(Some(payload))
}

/// Write a single length-prefixed frame to any async writer.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}
