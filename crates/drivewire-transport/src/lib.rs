//! Transport helpers for DriveWire: serial port or TCP (Becker-style) socket.
//!
//! The server operates on anything that is `AsyncRead + AsyncWrite + Unpin
//! + Send`, so these constructors just produce concrete transports from
//! user-facing config.

#![forbid(unsafe_code)]

use std::path::Path;

use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

/// Default Becker-port TCP listen port (matches DW4).
pub const DEFAULT_TCP_PORT: u16 = 65504;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("serial: {0}")]
    Serial(#[from] tokio_serial::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Open a serial port for DriveWire (8-N-1, no flow control).
pub fn open_serial(path: &Path, baud: u32) -> Result<SerialStream, TransportError> {
    let port = tokio_serial::new(path.to_string_lossy(), baud)
        .data_bits(tokio_serial::DataBits::Eight)
        .parity(tokio_serial::Parity::None)
        .stop_bits(tokio_serial::StopBits::One)
        .flow_control(tokio_serial::FlowControl::None)
        .open_native_async()?;
    Ok(port)
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
