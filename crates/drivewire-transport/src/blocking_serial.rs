//! Blocking-thread-backed serial transport.
//!
//! tokio-serial / mio-serial / serialport-rs delivered corrupt bytes
//! against a PL2303-class USB adapter on macOS — where `screen` and
//! `pyserial` both reliably saw the CoCo's `OP_INIT` + `OP_TIME` (0x49
//! 0x23), our path returned `0xFF` or `0x1F`. The diagnosis ultimately
//! pointed at mio's poll-based readiness interacting with the tty
//! driver in an unfortunate way (or to a sequencing issue with
//! `O_NONBLOCK` flipped after `open()`).
//!
//! This module sidesteps the whole stack: open the device with raw
//! `libc::open`, configure termios ourselves, then run blocking
//! reads/writes in dedicated `std::thread` workers. Bytes are funnelled
//! into the async runtime via `tokio::sync::mpsc` and exposed via the
//! `AsyncRead + AsyncWrite` impls on `BlockingSerial`.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// Serial port wrapped as an async stream. Owns two blocking worker
/// threads that talk to the kernel.
pub struct BlockingSerial {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    leftover: Vec<u8>,
}

/// Open `path` at `baud`, 8-N-1, no flow control, with DTR+RTS asserted.
pub fn open(path: &Path, baud: u32) -> io::Result<BlockingSerial> {
    let s = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 device path"))?;
    let cpath = CString::new(s)?;

    // O_RDWR | O_NOCTTY — same flags `screen` uses on macOS. No
    // O_NONBLOCK here; we want blocking reads in the worker thread.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    if let Err(e) = configure_termios(fd, baud) {
        unsafe { libc::close(fd) };
        return Err(e);
    }

    // Assert DTR + RTS together — these are sticky for the life of the
    // open. Some guests only emit DriveWire when DTR is high.
    let flags: libc::c_int = libc::TIOCM_DTR | libc::TIOCM_RTS;
    let r = unsafe { libc::ioctl(fd, libc::TIOCMBIS as _, &flags) };
    if r < 0 {
        tracing::debug!("TIOCMBIS to assert DTR/RTS failed (continuing)");
    }

    // Dup the fd so reader and writer threads can be torn down
    // independently — closing the original fd doesn't close the dup.
    let write_fd = unsafe { libc::dup(fd) };
    if write_fd < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let (read_tx, read_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    thread::Builder::new()
        .name("dw-serial-rx".into())
        .spawn(move || reader_loop(fd, read_tx))
        .expect("spawn rx thread");

    let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    thread::Builder::new()
        .name("dw-serial-tx".into())
        .spawn(move || writer_loop(write_fd, write_rx))
        .expect("spawn tx thread");

    Ok(BlockingSerial {
        rx: read_rx,
        tx: write_tx,
        leftover: Vec::new(),
    })
}

fn reader_loop(fd: RawFd, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            tracing::warn!(?err, "serial read errored — closing reader");
            break;
        }
        if n == 0 {
            tracing::debug!("serial read returned 0 — EOF / port closed");
            break;
        }
        if tx.send(buf[..n as usize].to_vec()).is_err() {
            tracing::debug!("serial rx channel closed — exiting reader thread");
            break;
        }
    }
    unsafe { libc::close(fd) };
}

fn writer_loop(fd: RawFd, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(chunk) = rx.blocking_recv() {
        let mut remaining = &chunk[..];
        while !remaining.is_empty() {
            let n =
                unsafe { libc::write(fd, remaining.as_ptr() as *const _, remaining.len()) };
            if n <= 0 {
                let err = io::Error::last_os_error();
                tracing::warn!(?err, "serial write errored — dropping {} bytes", remaining.len());
                break;
            }
            remaining = &remaining[n as usize..];
        }
    }
    unsafe { libc::close(fd) };
}

fn configure_termios(fd: RawFd, baud: u32) -> io::Result<()> {
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut tio) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Raw mode (no line discipline, no echo, no signals, no canonical).
    unsafe { libc::cfmakeraw(&mut tio) };
    // 8-N-1, no hardware flow, receiver enabled, ignore modem control.
    tio.c_cflag &= !libc::CSIZE;
    tio.c_cflag |= libc::CS8;
    tio.c_cflag &= !libc::PARENB;
    tio.c_cflag &= !libc::CSTOPB;
    tio.c_cflag &= !crtscts_flag();
    tio.c_cflag |= libc::CREAD | libc::CLOCAL;
    // Blocking read returning whenever any byte is available.
    tio.c_cc[libc::VMIN] = 1;
    tio.c_cc[libc::VTIME] = 0;
    // Baud.
    let speed = baud_to_speed(baud);
    if unsafe { libc::cfsetspeed(&mut tio, speed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) } != 0 {
        return Err(io::Error::last_os_error());
    }
    tracing::info!(baud, "serial port configured (raw termios)");
    Ok(())
}

#[cfg(target_os = "macos")]
fn crtscts_flag() -> libc::tcflag_t {
    libc::CRTSCTS
}

#[cfg(not(target_os = "macos"))]
fn crtscts_flag() -> libc::tcflag_t {
    libc::CRTSCTS as libc::tcflag_t
}

fn baud_to_speed(baud: u32) -> libc::speed_t {
    match baud {
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        _ => libc::B115200,
    }
}

impl AsyncRead for BlockingSerial {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Serve any leftover bytes from a previous partial read first.
        if !self.leftover.is_empty() {
            let n = self.leftover.len().min(buf.remaining());
            buf.put_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.leftover.extend_from_slice(&chunk[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for BlockingSerial {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.tx.send(buf.to_vec()).is_err() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer thread closed",
            )));
        }
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
