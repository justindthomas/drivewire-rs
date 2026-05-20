//! Transport helpers for DriveWire: serial port or TCP (Becker-style) socket.
//!
//! The server operates on anything that is `AsyncRead + AsyncWrite + Unpin
//! + Send`, so these constructors just produce concrete transports from
//! user-facing config.

#![deny(unsafe_code)]

use std::path::Path;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

mod blocking_serial;

pub use blocking_serial::BlockingSerial;

/// Default Becker-port TCP listen port (matches DW4).
pub const DEFAULT_TCP_PORT: u16 = 65504;

/// Default drain window after opening a serial port — long enough to
/// catch a stale half-packet from the previous session, short enough
/// not to delay startup against a freshly-booted CoCo.
pub const DEFAULT_DRAIN: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Open a serial port for DriveWire (8-N-1, no flow control, DTR+RTS
/// high). Uses our own blocking-thread implementation rather than
/// tokio-serial / mio-serial because the latter delivered corrupt bytes
/// against PL2303-class USB adapters in real-world testing.
pub fn open_serial(path: &Path, baud: u32) -> Result<BlockingSerial, TransportError> {
    Ok(blocking_serial::open(path, baud)?)
}

/// No-op kept for CLI compatibility — the macOS USB-serial latency
/// timer ioctl was helpful with the tokio-serial path but is moot now
/// that we read with blocking syscalls in our own thread.
pub fn set_low_latency(_port: &BlockingSerial, _latency_ms: u64) {
    tracing::debug!("set_low_latency is a no-op with the blocking serial backend");
}

/// Read and discard whatever is sitting in the port's RX buffer at
/// startup — stale bytes from a previous session, half a desynced
/// packet, etc. Returns the number of bytes drained.
pub async fn drain_serial(port: &mut BlockingSerial, window: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + window;
    let mut buf = [0u8; 1024];
    let mut total = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, port.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => total += n,
            Ok(Err(_)) | Err(_) => break,
        }
    }
    if total > 0 {
        tracing::info!(bytes = total, "drained stale serial bytes at open");
    }
    total
}

/// Bind a TCP listener for Becker-port clients.
pub async fn bind_tcp(addr: &str) -> Result<TcpListener, TransportError> {
    Ok(TcpListener::bind(addr).await?)
}

/// Accept the next incoming TCP connection.
pub async fn accept_tcp(listener: &TcpListener) -> Result<TcpStream, TransportError> {
    let (stream, peer) = listener.accept().await?;
    tracing::info!(%peer, "tcp client connected");
    Ok(stream)
}
