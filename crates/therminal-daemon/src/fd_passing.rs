//! Unix SCM_RIGHTS FD passing for zero-downtime daemon handoff.
//!
//! Uses `sendmsg`/`recvmsg` with ancillary data to pass PTY master file
//! descriptors from the old daemon process to the new daemon process over
//! a Unix domain socket. The in-band data carries a MessagePack-encoded
//! `HandoffPayload` with session/pane metadata; the out-of-band ancillary
//! data carries the actual file descriptors via `SCM_RIGHTS`.
//!
//! Gated behind `#[cfg(unix)]` — on non-Unix platforms the handoff falls
//! back to graceful restart (sessions are lost).

use std::io;
use std::os::unix::io::RawFd;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use therminal_protocol::daemon::HandoffPayload;

/// Maximum number of PTY FDs we support in a single handoff.
/// Each FD needs space in the cmsg buffer — 128 is more than enough
/// for any reasonable session count.
const MAX_HANDOFF_FDS: usize = 128;

/// Send a handoff payload (metadata) along with PTY master FDs over a Unix socket.
///
/// The `payload` is MessagePack-serialized and sent as the iov data.
/// The `fds` are sent as SCM_RIGHTS ancillary data, one per pane.
///
/// # Safety
///
/// The caller must ensure `fds` are valid, open file descriptors that will
/// remain open until this function returns.
pub fn send_fds(socket_fd: RawFd, payload: &HandoffPayload, fds: &[RawFd]) -> Result<()> {
    if fds.len() > MAX_HANDOFF_FDS {
        anyhow::bail!(
            "too many FDs for handoff: {} (max {})",
            fds.len(),
            MAX_HANDOFF_FDS
        );
    }
    if fds.len() != payload.panes.len() {
        anyhow::bail!(
            "FD count ({}) does not match pane count ({})",
            fds.len(),
            payload.panes.len()
        );
    }

    let data = rmp_serde::to_vec(payload).context("failed to serialize handoff payload")?;

    info!(
        pane_count = fds.len(),
        data_len = data.len(),
        "sending handoff FDs via SCM_RIGHTS"
    );

    // Build iovec for the payload data.
    let iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };

    if fds.is_empty() {
        // No FDs to send, just send the (empty) payload.
        let msg = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &iov as *const _ as *mut _,
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };

        let sent = unsafe { libc::sendmsg(socket_fd, &msg, 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error()).context("sendmsg failed (no FDs)");
        }
        return Ok(());
    }

    // Build the cmsg buffer for SCM_RIGHTS.
    let fds_bytes = std::mem::size_of_val(fds);
    let cmsg_space = unsafe { libc::CMSG_SPACE(fds_bytes as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &iov as *const _ as *mut _,
        msg_iovlen: 1,
        msg_control: cmsg_buf.as_mut_ptr() as *mut libc::c_void,
        msg_controllen: cmsg_space,
        msg_flags: 0,
    };

    // Fill the cmsg header.
    let cmsg: *mut libc::cmsghdr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        anyhow::bail!("CMSG_FIRSTHDR returned null");
    }
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fds_bytes as u32) as usize;

        // Copy FDs into the cmsg data area.
        let cmsg_data = libc::CMSG_DATA(cmsg);
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, cmsg_data, fds_bytes);
    }

    // Update controllen to the actual size needed.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(fds_bytes as u32) } as usize;

    let sent = unsafe { libc::sendmsg(socket_fd, &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error()).context("sendmsg with SCM_RIGHTS failed");
    }

    debug!(bytes_sent = sent, fds = fds.len(), "handoff FDs sent");
    Ok(())
}

/// Receive a handoff payload and PTY master FDs from a Unix socket.
///
/// Returns the deserialized `HandoffPayload` and the received file descriptors.
/// The FDs are in the same order as `payload.panes`.
pub fn recv_fds(socket_fd: RawFd) -> Result<(HandoffPayload, Vec<RawFd>)> {
    // Allocate receive buffer (1 MiB should be plenty for metadata).
    let mut data_buf = vec![0u8; 1024 * 1024];

    let iov = libc::iovec {
        iov_base: data_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: data_buf.len(),
    };

    // Allocate cmsg buffer large enough for MAX_HANDOFF_FDS.
    let max_fds_bytes = MAX_HANDOFF_FDS * std::mem::size_of::<RawFd>();
    let cmsg_space = unsafe { libc::CMSG_SPACE(max_fds_bytes as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &iov as *const _ as *mut _,
        msg_iovlen: 1,
        msg_control: cmsg_buf.as_mut_ptr() as *mut libc::c_void,
        msg_controllen: cmsg_space,
        msg_flags: 0,
    };

    let received = unsafe { libc::recvmsg(socket_fd, &mut msg, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error()).context("recvmsg failed");
    }
    if received == 0 {
        anyhow::bail!("recvmsg returned 0 bytes (peer closed connection)");
    }

    let data_len = received as usize;
    debug!(data_len, "received handoff data");

    // Deserialize the payload from the iov data.
    let payload: HandoffPayload = rmp_serde::from_slice(&data_buf[..data_len])
        .context("failed to deserialize handoff payload")?;

    // Extract FDs from cmsg ancillary data.
    let mut fds = Vec::new();
    let mut cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg_ptr.is_null() {
        let cmsg = unsafe { &*cmsg_ptr };
        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            let data_ptr = unsafe { libc::CMSG_DATA(cmsg_ptr) };
            let data_len = cmsg.cmsg_len - unsafe { libc::CMSG_LEN(0) } as usize;
            let fd_count = data_len / std::mem::size_of::<RawFd>();

            for i in 0..fd_count {
                let fd = unsafe {
                    std::ptr::read_unaligned(
                        data_ptr.add(i * std::mem::size_of::<RawFd>()) as *const RawFd
                    )
                };
                fds.push(fd);
            }
        }
        cmsg_ptr = unsafe { libc::CMSG_NXTHDR(&msg, cmsg_ptr) };
    }

    info!(
        pane_count = payload.panes.len(),
        fd_count = fds.len(),
        "received handoff payload with FDs"
    );

    if fds.len() != payload.panes.len() {
        warn!(
            expected = payload.panes.len(),
            received = fds.len(),
            "FD count mismatch in handoff"
        );
    }

    Ok((payload, fds))
}

/// Serve handoff FDs on a temporary Unix socket.
///
/// Binds a listener at `socket_path`, accepts a single connection, sends the
/// payload + FDs via SCM_RIGHTS, then returns. The caller is responsible for
/// cleaning up the socket file.
pub async fn serve_handoff_fds(
    socket_path: &std::path::Path,
    payload: &HandoffPayload,
    fds: &[RawFd],
) -> Result<()> {
    let listener =
        tokio::net::UnixListener::bind(socket_path).context("failed to bind handoff socket")?;

    info!(
        path = %socket_path.display(),
        pane_count = fds.len(),
        "handoff socket listening, waiting for new daemon"
    );

    // Wait for the new daemon to connect (with a timeout).
    let accept_result =
        tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept()).await;

    let (stream, _) = match accept_result {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => return Err(e).context("accept failed on handoff socket"),
        Err(_) => anyhow::bail!("timeout waiting for new daemon to connect to handoff socket"),
    };

    // Get the raw FD of the accepted connection for sendmsg.
    let stream_fd = {
        use std::os::unix::io::AsRawFd;
        stream.as_raw_fd()
    };

    // Send the FDs over the accepted connection.
    send_fds(stream_fd, payload, fds)?;

    // Give the receiver a moment to process before we drop the connection.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    info!("handoff FDs sent, closing handoff socket");
    Ok(())
}

/// Connect to a handoff socket and receive FDs from the old daemon.
pub async fn receive_handoff_fds(
    socket_path: &std::path::Path,
) -> Result<(HandoffPayload, Vec<RawFd>)> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .with_context(|| {
            format!(
                "failed to connect to handoff socket: {}",
                socket_path.display()
            )
        })?;

    // Get the raw FD for recvmsg.
    let stream_fd = {
        use std::os::unix::io::AsRawFd;
        stream.as_raw_fd()
    };

    // Receive FDs synchronously (the data should already be available).
    let result = recv_fds(stream_fd)?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_recv_empty_payload() {
        // Create a socketpair.
        let mut fds = [0 as RawFd; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed");

        let payload = HandoffPayload { panes: vec![] };

        send_fds(fds[0], &payload, &[]).unwrap();
        let (received_payload, received_fds) = recv_fds(fds[1]).unwrap();

        assert!(received_payload.panes.is_empty());
        assert!(received_fds.is_empty());

        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn send_recv_with_fds() {
        use therminal_protocol::daemon::HandoffPaneMeta;

        // Create a socketpair for the FD passing channel.
        let mut sock_fds = [0 as RawFd; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sock_fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed");

        // Create a pipe to use as the "PTY FD" to transfer.
        let mut pipe_fds = [0 as RawFd; 2];
        let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "pipe failed");

        let payload = HandoffPayload {
            panes: vec![HandoffPaneMeta {
                session_id: 1,
                session_name: Some("test".to_string()),
                pane_id: 42,
                cols: 80,
                rows: 24,
            }],
        };

        // Send the read end of the pipe as the "PTY FD".
        send_fds(sock_fds[0], &payload, &[pipe_fds[0]]).unwrap();
        let (received_payload, received_fds) = recv_fds(sock_fds[1]).unwrap();

        assert_eq!(received_payload.panes.len(), 1);
        assert_eq!(received_payload.panes[0].pane_id, 42);
        assert_eq!(received_fds.len(), 1);

        // The received FD should be valid and different from the original
        // (it's a new FD in this process, but since we're in the same process
        // for testing, it may be different or same depending on kernel).
        assert!(received_fds[0] >= 0);

        // Write through the original write end, read from the received FD.
        let test_data = b"hello from handoff";
        let written = unsafe {
            libc::write(
                pipe_fds[1],
                test_data.as_ptr() as *const libc::c_void,
                test_data.len(),
            )
        };
        assert_eq!(written as usize, test_data.len());

        let mut read_buf = [0u8; 64];
        let read_count = unsafe {
            libc::read(
                received_fds[0],
                read_buf.as_mut_ptr() as *mut libc::c_void,
                read_buf.len(),
            )
        };
        assert_eq!(read_count as usize, test_data.len());
        assert_eq!(&read_buf[..test_data.len()], test_data);

        // Cleanup.
        unsafe {
            libc::close(sock_fds[0]);
            libc::close(sock_fds[1]);
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
            libc::close(received_fds[0]);
        }
    }
}
