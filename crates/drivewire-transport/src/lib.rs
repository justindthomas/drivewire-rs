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
use tokio_serial::{SerialPort, SerialPortBuilderExt, SerialStream};

mod low_latency;

/// Default Becker-port TCP listen port (matches DW4).
pub const DEFAULT_TCP_PORT: u16 = 65504;

/// Default drain window after opening a serial port — long enough to
/// catch a stale half-packet from the previous session, short enough
/// not to delay startup against a freshly-booted CoCo.
pub const DEFAULT_DRAIN: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("serial: {0}")]
    Serial(#[from] tokio_serial::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Open a serial port for DriveWire (8-N-1, no flow control).
///
/// Cycles DTR low → 100 ms pause → high on open. Some DriveWire guests
/// (notably the CoCo3FPGA on the DE-1) only initiate DriveWire when they
/// see a fresh DTR rising edge — DTR steady-high doesn't trigger a new
/// session. The cycle simulates "host just connected" so the guest
/// rearms its boot path.
pub fn open_serial(path: &Path, baud: u32) -> Result<SerialStream, TransportError> {
    let mut port = tokio_serial::new(path.to_string_lossy(), baud)
        .data_bits(tokio_serial::DataBits::Eight)
        .parity(tokio_serial::Parity::None)
        .stop_bits(tokio_serial::StopBits::One)
        .flow_control(tokio_serial::FlowControl::None)
        .open_native_async()?;
    // DTR low → high edge; RTS just held high. Errors are best-effort —
    // some adapter chipsets don't expose these as separately controllable.
    let _ = port.write_request_to_send(true);
    if let Err(e) = port.write_data_terminal_ready(false) {
        tracing::debug!(?e, "could not drop DTR");
    } else {
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Err(e) = port.write_data_terminal_ready(true) {
        tracing::debug!(?e, "could not raise DTR (adapter may not support it)");
    }
    Ok(port)
}

/// Best-effort lower the USB-serial latency timer on `port`. On macOS
/// this issues the IOSSDATALAT ioctl (default 16 ms → 1 ms). Other
/// platforms are no-ops for now (no evidence we need them yet).
///
/// Errors are logged but not propagated — a host without an FTDI / PL2303
/// / CH340 underneath will reject the ioctl, and that's fine.
pub fn set_low_latency(port: &SerialStream, latency_ms: u64) {
    match low_latency::set(port, latency_ms) {
        Ok(()) => tracing::info!(latency_ms, "USB-serial latency timer set"),
        Err(e) => {
            tracing::warn!(
                ?e,
                latency_ms,
                "could not lower USB-serial latency timer (continuing with kernel default)"
            )
        }
    }
}

/// Read and discard whatever is sitting in the port's RX buffer at
/// startup — stale bytes from a previous session, half a desynced
/// packet, etc. Returns the number of bytes drained.
pub async fn drain_serial(port: &mut SerialStream, window: Duration) -> usize {
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
