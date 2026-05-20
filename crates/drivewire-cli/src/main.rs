//! `dw` — DriveWire 3/4 server command-line.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use drivewire_server::Server;
use drivewire_vdisk::{DskFile, VDisk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing_subscriber::EnvFilter;

const DEFAULT_ATTACH_SOCKET: &str = "/tmp/drivewire.sock";

#[derive(Parser)]
#[command(name = "dw", version, about = "DriveWire 3/4 server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the DriveWire server on a serial port or TCP listener.
    Serve(ServeArgs),
    /// Attach a terminal to a virtual serial channel on a running daemon.
    Attach(AttachArgs),
    /// Mount a disk image into a drive slot on a running daemon.
    Mount {
        slot: u8,
        #[arg(value_name = "PATH")]
        path: PathBuf,
        #[arg(long)]
        read_only: bool,
    },
}

#[derive(Args)]
struct ServeArgs {
    /// Serial device (e.g. /dev/tty.usbserial-XYZ on macOS).
    #[arg(long, conflicts_with = "tcp", value_name = "DEVICE")]
    serial: Option<PathBuf>,

    /// Baud rate when using --serial.
    #[arg(long, default_value_t = 57_600)]
    baud: u32,

    /// TCP bind address for Becker-port clients (e.g. 0.0.0.0:65504).
    #[arg(long, value_name = "ADDR")]
    tcp: Option<String>,

    /// Optional disk image mounted to slot 0 on startup.
    #[arg(long, value_name = "PATH")]
    disk0: Option<PathBuf>,

    /// Bind a Unix-domain socket for `dw attach` clients. Default path:
    /// /tmp/drivewire.sock. Pass --no-attach-socket to disable.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_ATTACH_SOCKET, conflicts_with = "no_attach_socket")]
    attach_socket: PathBuf,

    /// Skip binding the attach socket.
    #[arg(long)]
    no_attach_socket: bool,
}

#[derive(Args)]
struct AttachArgs {
    /// Vserial channel to attach to (0..=14).
    channel: u8,

    /// Daemon control socket path.
    #[arg(long, default_value = DEFAULT_ATTACH_SOCKET, value_name = "PATH")]
    socket: PathBuf,

    /// Skip CR -> CRLF translation on bytes coming from the guest. By
    /// default, bare CRs (NitrOS-9's line terminator) are upgraded to
    /// CRLF so they render correctly in a raw-mode terminal.
    #[arg(long)]
    raw: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve(args) => serve(args).await,
        Cmd::Attach(args) => attach(args).await,
        Cmd::Mount { .. } => {
            anyhow::bail!("`dw mount` requires the daemon control socket — not yet implemented");
        }
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let server = Arc::new(Server::new());

    if let Some(path) = args.disk0 {
        let disk = DskFile::open(&path, false).await?;
        tracing::info!(
            path = %path.display(),
            sectors = disk.sector_count(),
            "mounted slot 0"
        );
        server.mount(0, disk).await;
    }

    if !args.no_attach_socket {
        let listener_server = Arc::clone(&server);
        let socket_path = args.attach_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = listener_server.run_attach_listener(&socket_path).await {
                tracing::error!(?e, path = %socket_path.display(), "attach listener exited");
            }
        });
    }

    match (args.serial, args.tcp) {
        (Some(dev), None) => {
            let port = drivewire_transport::open_serial(&dev, args.baud)?;
            tracing::info!(device = %dev.display(), baud = args.baud, "serial transport open");
            Arc::clone(&server).run(port).await?;
        }
        (None, Some(addr)) => {
            let listener = drivewire_transport::bind_tcp(&addr).await?;
            tracing::info!(%addr, "tcp listener bound");
            loop {
                let stream = drivewire_transport::accept_tcp(&listener).await?;
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    if let Err(e) = server.run(stream).await {
                        tracing::warn!(?e, "session ended");
                    }
                });
            }
        }
        (None, None) => anyhow::bail!("must pass --serial DEVICE or --tcp ADDR"),
        (Some(_), Some(_)) => unreachable!("clap enforces conflicts_with"),
    }
    Ok(())
}

async fn attach(args: AttachArgs) -> Result<()> {
    let mut stream = UnixStream::connect(&args.socket)
        .await
        .with_context(|| format!("connect to {}", args.socket.display()))?;
    stream.write_all(&[args.channel]).await?;

    let _raw = RawMode::enable()?;
    eprintln!("[dw] attached to channel {} (Ctrl-C exits)\r", args.channel);

    let (mut sock_r, mut sock_w) = tokio::io::split(stream);
    let stdin_to_sock = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        tokio::io::copy(&mut stdin, &mut sock_w).await
    });
    let translate = !args.raw;
    let sock_to_stdout = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 1024];
        let mut had_cr = false;
        loop {
            let n = match sock_r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let payload: Vec<u8> = if translate {
                normalize_line_endings(&buf[..n], &mut had_cr)
            } else {
                buf[..n].to_vec()
            };
            if stdout.write_all(&payload).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    tokio::select! {
        _ = stdin_to_sock => {}
        _ = sock_to_stdout => {}
    }
    Ok(())
}

/// CR-only and LF-only line endings → CRLF (raw mode needs both bytes
/// to advance the cursor down AND back to column 0). State persists
/// across calls via `had_cr` so a CR that lands at a chunk boundary
/// still pairs correctly with whatever follows.
fn normalize_line_endings(input: &[u8], had_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + 8);
    for &b in input {
        match b {
            b'\r' => {
                if *had_cr {
                    out.extend_from_slice(b"\r\n");
                }
                *had_cr = true;
            }
            b'\n' => {
                out.extend_from_slice(b"\r\n");
                *had_cr = false;
            }
            other => {
                if *had_cr {
                    out.extend_from_slice(b"\r\n");
                }
                out.push(other);
                *had_cr = false;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize_line_endings;

    fn norm(s: &[u8]) -> Vec<u8> {
        let mut had_cr = false;
        normalize_line_endings(s, &mut had_cr)
    }

    #[test]
    fn bare_cr_becomes_crlf() {
        assert_eq!(norm(b"hi\rthere"), b"hi\r\nthere");
    }

    #[test]
    fn existing_crlf_preserved() {
        assert_eq!(norm(b"hi\r\nthere"), b"hi\r\nthere");
    }

    #[test]
    fn bare_lf_becomes_crlf() {
        assert_eq!(norm(b"hi\nthere"), b"hi\r\nthere");
    }

    #[test]
    fn multiple_crs_each_become_crlf() {
        // NitrOS-9 sometimes sends `\r\r\r\n` — every CR should yield a line.
        assert_eq!(norm(b"a\r\r\r\nb"), b"a\r\n\r\n\r\nb");
    }

    #[test]
    fn state_persists_across_chunks() {
        let mut had_cr = false;
        let a = normalize_line_endings(b"end-of-chunk\r", &mut had_cr);
        assert_eq!(a, b"end-of-chunk");
        assert!(had_cr);
        let b = normalize_line_endings(b"next", &mut had_cr);
        assert_eq!(b, b"\r\nnext");
        assert!(!had_cr);
    }
}

/// RAII wrapper that restores cooked-mode terminal on Drop.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
