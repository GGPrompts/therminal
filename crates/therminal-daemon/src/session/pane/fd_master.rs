//! `FdPtyMaster`: a `MasterPty` impl over a raw fd received via SCM_RIGHTS
//! during daemon handoff (unix-only).
//!
//! All `unsafe` blocks here carry SAFETY comments documenting why each
//! libc call is sound (see tn-bkf4).

#![cfg(unix)]

use portable_pty::MasterPty;
use std::os::unix::io::FromRawFd;

/// A `MasterPty` implementation backed by a raw file descriptor received
/// via SCM_RIGHTS during daemon handoff.
///
/// Owns the FD and closes it on drop. Provides reader/writer cloning via
/// `dup()` so the PTY reader thread and writer can operate independently.
pub(super) struct FdPtyMaster {
    fd: std::os::unix::io::RawFd,
    took_writer: std::cell::RefCell<bool>,
}

impl FdPtyMaster {
    pub(super) fn new(fd: std::os::unix::io::RawFd) -> Self {
        Self {
            fd,
            took_writer: std::cell::RefCell::new(false),
        }
    }
}

impl MasterPty for FdPtyMaster {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: `self.fd` is a valid PTY master file descriptor owned by
        // this `FdPtyMaster` for its entire lifetime (closed in `Drop`).
        // `TIOCSWINSZ` is documented to read a `struct winsize` through the
        // third argument; `&ws as *const _` points to a fully-initialised
        // local that lives until `ioctl` returns, so the kernel's read is
        // sound. The return value is checked below and converted to a
        // typed error.
        let ret = unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws as *const _) };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        // SAFETY: `libc::winsize` is a `#[repr(C)]` POD struct of plain
        // integers — the all-zero bit pattern is a valid inhabitant. We
        // immediately overwrite it via TIOCGWINSZ before reading any field.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `self.fd` is a valid PTY master fd owned by this
        // `FdPtyMaster`. `TIOCGWINSZ` writes a `struct winsize` through the
        // third argument; `&mut ws` is a unique mutable borrow of a
        // properly-aligned `winsize`, so the kernel's write is sound. The
        // return value is checked below.
        let ret = unsafe { libc::ioctl(self.fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(portable_pty::PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            })
        }
    }

    fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
        // SAFETY: `self.fd` is a valid PTY master fd owned by this
        // `FdPtyMaster`. `dup(2)` is thread-safe and produces an
        // independently-closeable copy referencing the same open file
        // description. The duplicated fd is checked for the -1 sentinel
        // before being handed to `File::from_raw_fd`.
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: `dup_fd` is a fresh, valid fd we just successfully dup'd
        // and that nothing else owns yet. Transferring it into `File` makes
        // `File` the sole owner, satisfying `from_raw_fd`'s contract.
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        Ok(Box::new(file))
    }

    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
        if *self.took_writer.borrow() {
            anyhow::bail!("cannot take writer more than once");
        }
        *self.took_writer.borrow_mut() = true;
        // SAFETY: `self.fd` is a valid PTY master fd owned by this
        // `FdPtyMaster`. `dup(2)` is thread-safe; we check the result for
        // -1 before consuming it. The `took_writer` flag above prevents
        // duplicate writer handouts at the API level — this dup is for the
        // single permitted writer copy.
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: `dup_fd` is a fresh, valid fd nothing else owns. Handing
        // it to `File::from_raw_fd` makes `File` the sole owner.
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        Ok(Box::new(file))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        // SAFETY: `self.fd` is a valid PTY master fd owned by this
        // `FdPtyMaster`. `tcgetpgrp(3)` only reads from the controlling
        // terminal referenced by the fd and returns a pid (or -1 / 0 to
        // signal "none"); it has no aliasing or lifetime requirements
        // beyond a valid fd.
        match unsafe { libc::tcgetpgrp(self.fd) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        Some(self.fd)
    }

    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

impl Drop for FdPtyMaster {
    fn drop(&mut self) {
        // SAFETY: `self.fd` was set in `FdPtyMaster::new` from an fd owned
        // by this struct (received via SCM_RIGHTS handoff) and has not been
        // closed elsewhere — `FdPtyMaster` does not expose the raw fd for
        // external close, and `try_clone_reader` / `take_writer` always
        // `dup` before transferring ownership. Drop is the unique close
        // site, so there is no double-close hazard.
        unsafe {
            libc::close(self.fd);
        }
    }
}
