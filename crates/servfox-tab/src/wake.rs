//! `EventLoopWaker` over a non-blocking self-pipe.
//!
//! Servo wakes the embedder from engine threads; the main thread `poll()`s the
//! read end together with stdin. Shared between the `--spike` mode (task 4) and
//! the full IPC runtime (task 5).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use servo::EventLoopWaker;

/// An [`EventLoopWaker`] that writes one byte into a non-blocking pipe.
pub struct PipeWaker {
    tx: Arc<OwnedFd>,
}

/// A readable wake-end that also drains the pipe.
pub struct WakeReader {
    fd: OwnedFd,
}

/// Create a `(waker, wake_reader)` pair backed by a fresh `pipe(2)`.
pub fn make_self_pipe() -> std::io::Result<(PipeWaker, WakeReader)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: writing two ints into an array of two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() returned two fresh descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: pipe() returned two fresh descriptors.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
        // SAFETY: F_GETFL/F_SETFL on valid descriptors.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok((
        PipeWaker {
            tx: Arc::new(write_fd),
        },
        WakeReader { fd: read_fd },
    ))
}

impl EventLoopWaker for PipeWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(PipeWaker {
            tx: self.tx.clone(),
        })
    }

    fn wake(&self) {
        let byte = [1u8];
        // SAFETY: writing one byte to a valid, non-blocking pipe write-end.
        unsafe {
            libc::write(
                self.tx.as_raw_fd(),
                byte.as_ptr() as *const libc::c_void,
                1,
            );
        }
    }
}

impl WakeReader {
    /// The raw fd of the wake pipe's read end (for adding to a `poll()` set).
    pub fn fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    /// Wait up to `timeout` for a wake byte, then drain the pipe.
    pub fn wait_and_drain(&self, timeout: Duration) {
        let mut pfd = [libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: single valid pollfd entry.
        let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 1, timeout.as_millis() as i32) };
        if ready <= 0 {
            return;
        }
        self.drain();
    }

    /// Non-blocking drain of all pending wake bytes.
    pub fn drain(&self) {
        let mut buf = [0u8; 256];
        loop {
            // SAFETY: reading into a valid buffer from a valid descriptor.
            let n = unsafe {
                libc::read(
                    self.fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }
    }
}
