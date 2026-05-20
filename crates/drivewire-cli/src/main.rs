//! `dw` — DriveWire 3/4 server command-line.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use drivewire_server::Server;
use drivewire_vdisk::{DskFile, VDisk};
use tracing_subscriber::EnvFilter;

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
    Attach { channel: u8 },
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
        Cmd::Attach { channel } => {
            anyhow::bail!(
                "`dw attach {channel}` not yet implemented \
                 (vserial channels are scaffolded but not wired up)"
            );
        }
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
