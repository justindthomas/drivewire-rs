//! `dw` — DriveWire 3/4 server command-line.

use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use drivewire_proto::Opcode;
use drivewire_server::Server;
use drivewire_vdisk::{DskFile, VDisk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing_subscriber::EnvFilter;

const DEFAULT_ATTACH_SOCKET: &str = "/tmp/drivewire.sock";
const DEFAULT_CONTROL_SOCKET: &str = "/tmp/drivewire-ctl.sock";

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
    Mount(MountArgs),
    /// Unmount a drive slot on a running daemon.
    Unmount(UnmountArgs),
    /// Show drive + vserial status from a running daemon.
    Status(StatusArgs),
    /// Open a serial / TCP transport, wait for a DWINIT from a guest, and
    /// report whether the handshake works. Useful for bringing up a
    /// physical CoCo over USB-serial.
    Probe(ProbeArgs),
}

#[derive(Args)]
struct MountArgs {
    /// Drive slot to mount into.
    slot: u8,
    /// Path to the .dsk image.
    #[arg(value_name = "PATH")]
    path: PathBuf,
    /// Daemon control socket path.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET, value_name = "PATH")]
    socket: PathBuf,
}

#[derive(Args)]
struct UnmountArgs {
    /// Drive slot to unmount.
    slot: u8,
    /// Daemon control socket path.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET, value_name = "PATH")]
    socket: PathBuf,
}

#[derive(Args)]
struct StatusArgs {
    /// Daemon control socket path.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET, value_name = "PATH")]
    socket: PathBuf,
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

    /// Skip the USB-serial low-latency timer ioctl (macOS only, default
    /// on for --serial). Set this if your adapter rejects IOSSDATALAT.
    #[arg(long)]
    no_low_latency: bool,

    /// Drain stale bytes from the serial RX buffer for this many ms
    /// after open. Set to 0 to skip.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    drain_ms: u64,

    /// Bind a Unix-domain socket for `dw attach` clients. Default path:
    /// /tmp/drivewire.sock. Pass --no-attach-socket to disable.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_ATTACH_SOCKET, conflicts_with = "no_attach_socket")]
    attach_socket: PathBuf,

    /// Skip binding the attach socket.
    #[arg(long)]
    no_attach_socket: bool,

    /// Bind a Unix-domain socket for control commands (`dw mount`,
    /// `dw unmount`, `dw status`). Pass --no-control-socket to disable.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_CONTROL_SOCKET, conflicts_with = "no_control_socket")]
    control_socket: PathBuf,

    /// Skip binding the control socket.
    #[arg(long)]
    no_control_socket: bool,
}

#[derive(Args)]
struct ProbeArgs {
    /// Serial device.
    #[arg(long, conflicts_with = "tcp", value_name = "DEVICE")]
    serial: Option<PathBuf>,

    /// Baud rate when probing serial.
    #[arg(long, default_value_t = 57_600)]
    baud: u32,

    /// TCP address to connect to (e.g. an emulator's Becker listener).
    #[arg(long, value_name = "ADDR")]
    tcp: Option<String>,

    /// Seconds to wait for an inbound byte from the guest.
    #[arg(long, default_value_t = 10)]
    timeout: u64,

    /// Skip the macOS USB-serial low-latency timer ioctl.
    #[arg(long)]
    no_low_latency: bool,
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
        Cmd::Mount(args) => mount(args).await,
        Cmd::Unmount(args) => unmount(args).await,
        Cmd::Status(args) => status(args).await,
        Cmd::Probe(args) => probe(args).await,
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

    if !args.no_control_socket {
        let listener_server = Arc::clone(&server);
        let socket_path = args.control_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = listener_server.run_control_listener(&socket_path).await {
                tracing::error!(?e, path = %socket_path.display(), "control listener exited");
            }
        });
    }

    match (args.serial, args.tcp) {
        (Some(dev), None) => {
            let mut port = drivewire_transport::open_serial(&dev, args.baud)?;
            tracing::info!(device = %dev.display(), baud = args.baud, "serial transport open");
            if !args.no_low_latency {
                drivewire_transport::set_low_latency(&port, 1);
            }
            if args.drain_ms > 0 {
                drivewire_transport::drain_serial(&mut port, Duration::from_millis(args.drain_ms))
                    .await;
            }
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
    eprintln!(
        "[dw] attached to channel {} (Ctrl-A q exits, Ctrl-A Ctrl-A sends literal Ctrl-A)\r",
        args.channel
    );

    let (mut sock_r, mut sock_w) = tokio::io::split(stream);
    let stdin_to_sock = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 256];
        let mut escape_armed = false;
        loop {
            let n = match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut out = Vec::with_capacity(n);
            let mut exit = false;
            for &b in &buf[..n] {
                if process_input_byte(b, &mut escape_armed, &mut out) == EscapeAction::Exit {
                    exit = true;
                    break;
                }
            }
            if !out.is_empty() && sock_w.write_all(&out).await.is_err() {
                break;
            }
            if exit {
                break;
            }
        }
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

async fn probe(args: ProbeArgs) -> Result<()> {
    use tokio::io::{AsyncRead, AsyncWrite};
    let mut transport: Box<dyn AsyncRead + Unpin + Send>;
    let mut writer: Box<dyn AsyncWrite + Unpin + Send>;

    match (args.serial.clone(), args.tcp.clone()) {
        (Some(dev), None) => {
            eprintln!("[probe] opening {} at {} baud", dev.display(), args.baud);
            let mut port = drivewire_transport::open_serial(&dev, args.baud)?;
            if !args.no_low_latency {
                drivewire_transport::set_low_latency(&port, 1);
            }
            let drained = drivewire_transport::drain_serial(&mut port, Duration::from_millis(250))
                .await;
            if drained > 0 {
                eprintln!("[probe] drained {drained} stale byte(s) from RX buffer");
            }
            let (r, w) = tokio::io::split(port);
            transport = Box::new(r);
            writer = Box::new(w);
        }
        (None, Some(addr)) => {
            eprintln!("[probe] listening on {addr} — connect your client (e.g. XRoar) now");
            let listener = drivewire_transport::bind_tcp(&addr).await?;
            let stream = drivewire_transport::accept_tcp(&listener).await?;
            let (r, w) = tokio::io::split(stream);
            transport = Box::new(r);
            writer = Box::new(w);
        }
        (None, None) => anyhow::bail!("pass --serial DEVICE or --tcp ADDR"),
        (Some(_), Some(_)) => unreachable!("clap enforces conflicts_with"),
    }

    eprintln!(
        "[probe] waiting up to {}s for a byte from the guest (reset the CoCo if it's idle)...",
        args.timeout
    );
    let mut first = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_secs(args.timeout), transport.read_exact(&mut first)).await;
    match r {
        Err(_) => {
            eprintln!("[probe] no bytes received — common causes:");
            eprintln!("        - baud rate mismatch (CoCo3 HDB-DOS bitbanger is usually 57600)");
            eprintln!("        - wrong ROM on CoCo (need hdbdw3cc3.rom for serial, not hdbdw3bck.rom)");
            eprintln!("        - cable wiring: CoCo 4-pin DIN pin 4 (TX) → host RX, pin 2 (RX) ← host TX, pin 3 (GND)");
            anyhow::bail!("silent guest");
        }
        Ok(Err(e)) => anyhow::bail!("read error: {e}"),
        Ok(Ok(_)) => {}
    }
    let b = first[0];
    eprintln!("[probe] first byte: {b:#04x}");
    if b == 0x5A {
        let mut driver = [0u8; 1];
        transport.read_exact(&mut driver).await?;
        eprintln!(
            "[probe] OP_DWINIT driver={:#04x} — sending DW4 response 0x04",
            driver[0]
        );
        writer.write_all(&[0x04]).await?;
        eprintln!("[probe] handshake complete. Cable + ROM + baud are good.");
        Ok(())
    } else if let Ok(op) = Opcode::try_from(b) {
        eprintln!("[probe] decoded as {op:?} — guest is talking, but DWINIT is the canonical first byte; this server is mid-session or the CoCo skipped DWINIT.");
        Ok(())
    } else {
        eprintln!(
            "[probe] {b:#04x} is not a known DriveWire opcode. Most likely cause: wrong baud rate."
        );
        anyhow::bail!("non-DW data on the wire");
    }
}

/// Outcome of feeding one stdin byte through the escape-sequence state
/// machine. `Continue` keeps processing; `Exit` tells the attach loop to
/// quit cleanly (restoring the cooked terminal via `RawMode`'s Drop).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EscapeAction {
    Continue,
    Exit,
}

/// Process one input byte. Ctrl-A is the escape prefix. After Ctrl-A,
/// `q` or `Q` exits; another Ctrl-A sends a literal Ctrl-A to the guest;
/// any other byte passes through unchanged (preceded by the held Ctrl-A
/// so behaviour is non-destructive).
fn process_input_byte(byte: u8, escape_armed: &mut bool, out: &mut Vec<u8>) -> EscapeAction {
    const CTRL_A: u8 = 0x01;
    if *escape_armed {
        *escape_armed = false;
        match byte {
            CTRL_A => {
                out.push(CTRL_A);
                EscapeAction::Continue
            }
            b'q' | b'Q' => EscapeAction::Exit,
            other => {
                out.push(CTRL_A);
                out.push(other);
                EscapeAction::Continue
            }
        }
    } else if byte == CTRL_A {
        *escape_armed = true;
        EscapeAction::Continue
    } else {
        out.push(byte);
        EscapeAction::Continue
    }
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
    use super::{normalize_line_endings, process_input_byte, EscapeAction};

    fn norm(s: &[u8]) -> Vec<u8> {
        let mut had_cr = false;
        normalize_line_endings(s, &mut had_cr)
    }

    fn feed(bytes: &[u8]) -> (Vec<u8>, bool, bool) {
        let mut armed = false;
        let mut out = Vec::new();
        let mut exit = false;
        for &b in bytes {
            if process_input_byte(b, &mut armed, &mut out) == EscapeAction::Exit {
                exit = true;
                break;
            }
        }
        (out, armed, exit)
    }

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(feed(b"hello"), (b"hello".to_vec(), false, false));
    }

    #[test]
    fn ctrl_a_q_exits_and_consumes_both_bytes() {
        let (out, armed, exit) = feed(&[0x01, b'q']);
        assert!(exit);
        assert!(!armed);
        assert!(out.is_empty());
    }

    #[test]
    fn ctrl_a_capital_q_also_exits() {
        let (_out, _armed, exit) = feed(&[0x01, b'Q']);
        assert!(exit);
    }

    #[test]
    fn double_ctrl_a_sends_literal_ctrl_a() {
        let (out, armed, exit) = feed(&[0x01, 0x01]);
        assert_eq!(out, vec![0x01]);
        assert!(!armed);
        assert!(!exit);
    }

    #[test]
    fn ctrl_a_then_other_byte_is_non_destructive() {
        // The held Ctrl-A is emitted before the unrelated byte.
        let (out, armed, exit) = feed(&[0x01, b'x']);
        assert_eq!(out, b"\x01x");
        assert!(!armed);
        assert!(!exit);
    }

    #[test]
    fn escape_state_persists_across_reads() {
        let mut armed = false;
        let mut out = Vec::new();
        // First read ends with Ctrl-A only.
        let r1 = process_input_byte(0x01, &mut armed, &mut out);
        assert_eq!(r1, EscapeAction::Continue);
        assert!(armed);
        assert!(out.is_empty());
        // Second read brings the 'q'.
        let r2 = process_input_byte(b'q', &mut armed, &mut out);
        assert_eq!(r2, EscapeAction::Exit);
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

async fn mount(args: MountArgs) -> Result<()> {
    let path_str = args
        .path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
    let cmd = format!("MOUNT {} {}\n", args.slot, path_str);
    let reply = control_request(&args.socket, &cmd).await?;
    print!("{reply}");
    if reply.starts_with("ERROR") {
        anyhow::bail!("mount failed");
    }
    Ok(())
}

async fn unmount(args: UnmountArgs) -> Result<()> {
    let cmd = format!("UNMOUNT {}\n", args.slot);
    let reply = control_request(&args.socket, &cmd).await?;
    print!("{reply}");
    if reply.starts_with("ERROR") {
        anyhow::bail!("unmount failed");
    }
    Ok(())
}

async fn status(args: StatusArgs) -> Result<()> {
    let reply = control_request(&args.socket, "STATUS\n").await?;
    print!("{reply}");
    Ok(())
}

/// Send one command, read until a terminal line (`OK*`, `ERROR*`, or
/// `BYE*`). The server emits multi-line replies for STATUS, so we accept
/// any number of lines and return the whole blob.
async fn control_request(socket: &std::path::Path, cmd: &str) -> Result<String> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let (r, mut w) = tokio::io::split(stream);
    w.write_all(cmd.as_bytes()).await?;
    let mut reader = tokio::io::BufReader::new(r);
    let mut out = String::new();
    use tokio::io::AsyncBufReadExt as _;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        out.push_str(&line);
        let trimmed = line.trim_end();
        if trimmed == "OK"
            || trimmed.starts_with("OK ")
            || trimmed.starts_with("ERROR")
            || trimmed.starts_with("BYE")
        {
            break;
        }
    }
    Ok(out)
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
