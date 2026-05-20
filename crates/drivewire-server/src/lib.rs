//! DriveWire 3/4 server state machine.
//!
//! Reads opcodes from a transport, dispatches to the mounted virtual
//! drives. Virtual serial channels, printing, time, and DWINIT are
//! stubbed but not yet routed to host-side endpoints.
//!
//! Note: the wire protocol has no framing — a misread opcode desyncs the
//! stream until the guest issues OP_RESET. Each opcode arm therefore must
//! consume exactly its payload.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Number of vserial channels DriveWire 4 defines (0..=14 plus the
/// fast-write base sentinel — 15 usable channels in practice).
pub const VSERIAL_CHANNELS: usize = 16;

use drivewire_proto::opcode::{Decoded, Opcode};
use drivewire_proto::{checksum16, DwError, Lsn};
use drivewire_vdisk::{DskFile, VDisk, VDiskError, SECTOR_SIZE};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, RwLock};

/// Byte returned in response to OP_DWINIT. Identifies us as a DW4-class
/// server. There is no formally-standardised value; refine once we have
/// real handshakes with NitrOS-9 / HDB-DOS to compare against.
const DWINIT_RESPONSE: u8 = 0x04;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("transport closed")]
    Eof,
    #[error("unknown opcode {0:#04x}; stream cannot recover")]
    UnknownOpcode(u8),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// OS-9 SetStat / GetStat codes used over OP_SERSETSTAT / OP_SERGETSTAT.
mod ss {
    pub const COMST: u8 = 0x28;
    pub const OPEN: u8 = 0x29;
    pub const CLOSE: u8 = 0x2A;
    /// OS-9 ComSt payload size (dev_t-style status block).
    pub const COMST_PAYLOAD_LEN: usize = 26;
}

pub struct Server {
    drives: RwLock<HashMap<u8, Arc<dyn VDisk>>>,
    print_buffer: Mutex<Vec<u8>>,
    /// Bytes the guest has written into each vserial channel, waiting to
    /// be picked up by the host side (PTY, attach socket, etc.).
    vserial_inbox: Mutex<Vec<VecDeque<u8>>>,
    /// Bytes the host has produced for each vserial channel, waiting to
    /// be polled out by the guest's SERREAD / SERREADM loop.
    vserial_outbox: Mutex<Vec<VecDeque<u8>>>,
    /// Whether each channel has been opened by the guest.
    vserial_open: Mutex<Vec<bool>>,
    /// One Notify per channel; signaled when push_vserial appends a byte
    /// so an attached host reader can wake instead of polling.
    vserial_inbox_notify: Vec<Notify>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            drives: RwLock::default(),
            print_buffer: Mutex::default(),
            vserial_inbox: Mutex::new(
                (0..VSERIAL_CHANNELS).map(|_| VecDeque::new()).collect(),
            ),
            vserial_outbox: Mutex::new(
                (0..VSERIAL_CHANNELS).map(|_| VecDeque::new()).collect(),
            ),
            vserial_open: Mutex::new(vec![false; VSERIAL_CHANNELS]),
            vserial_inbox_notify: (0..VSERIAL_CHANNELS).map(|_| Notify::new()).collect(),
        }
    }
}

impl Server {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn mount(&self, slot: u8, disk: Arc<dyn VDisk>) {
        self.drives.write().await.insert(slot, disk);
    }

    pub async fn unmount(&self, slot: u8) {
        self.drives.write().await.remove(&slot);
    }

    /// Drive the protocol on a single transport until the peer disconnects.
    pub async fn run<T>(self: Arc<Self>, mut t: T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        loop {
            let mut byte = [0u8; 1];
            match t.read_exact(&mut byte).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(ServerError::Eof);
                }
                Err(e) => return Err(e.into()),
            }
            let decoded = Decoded::from_byte(byte[0]);
            tracing::trace!(?decoded, "opcode");

            match decoded {
                Decoded::Op(Opcode::Nop) => {}
                Decoded::Op(Opcode::Reset1)
                | Decoded::Op(Opcode::Reset2)
                | Decoded::Op(Opcode::Reset3) => {
                    tracing::info!("guest reset");
                }
                Decoded::Op(Opcode::Read) | Decoded::Op(Opcode::Reread) => {
                    self.handle_read(&mut t, false).await?;
                }
                Decoded::Op(Opcode::ReadEx) | Decoded::Op(Opcode::RereadEx) => {
                    self.handle_read(&mut t, true).await?;
                }
                Decoded::Op(Opcode::Write) | Decoded::Op(Opcode::Rewrite) => {
                    self.handle_write(&mut t).await?;
                }
                Decoded::Op(Opcode::DwInit) => {
                    self.handle_dwinit(&mut t).await?;
                }
                Decoded::Op(Opcode::Time) => {
                    self.handle_time(&mut t).await?;
                }
                Decoded::Op(Opcode::Init) => {
                    tracing::info!("guest INIT");
                }
                Decoded::Op(Opcode::Term) => {
                    tracing::info!("guest TERM");
                }
                Decoded::Op(Opcode::GetStat) => {
                    self.handle_stat(&mut t, "GETSTAT").await?;
                }
                Decoded::Op(Opcode::SetStat) => {
                    self.handle_stat(&mut t, "SETSTAT").await?;
                }
                Decoded::Op(Opcode::Print) => {
                    self.handle_print(&mut t).await?;
                }
                Decoded::Op(Opcode::PrintFlush) => {
                    self.handle_print_flush().await;
                }
                Decoded::Op(Opcode::SerRead) => {
                    self.handle_serread(&mut t).await?;
                }
                Decoded::Op(Opcode::SerReadM) => {
                    self.handle_serreadm(&mut t).await?;
                }
                Decoded::Op(Opcode::SerInit) => {
                    let mut ch = [0u8; 1];
                    t.read_exact(&mut ch).await?;
                    self.set_channel_open(ch[0], true).await;
                    tracing::info!(channel = ch[0], "vserial init");
                }
                Decoded::Op(Opcode::SerTerm) => {
                    let mut ch = [0u8; 1];
                    t.read_exact(&mut ch).await?;
                    self.set_channel_open(ch[0], false).await;
                    tracing::info!(channel = ch[0], "vserial term");
                }
                Decoded::Op(Opcode::SerWrite) => {
                    // 2 bytes: [channel, data]. Buffer into per-channel inbox.
                    let mut p = [0u8; 2];
                    t.read_exact(&mut p).await?;
                    self.push_vserial(p[0], p[1]).await;
                }
                Decoded::Op(Opcode::SerWriteM) => {
                    self.handle_serwritem(&mut t).await?;
                }
                Decoded::Op(Opcode::SerSetStat) => {
                    self.handle_sersetstat(&mut t).await?;
                }
                Decoded::Op(Opcode::SerGetStat) => {
                    // 2 bytes: [channel, code]. No response.
                    let mut p = [0u8; 2];
                    t.read_exact(&mut p).await?;
                    tracing::debug!(channel = p[0], code = format_args!("{:#04x}", p[1]), "SERGETSTAT");
                }
                Decoded::FastWrite { channel } => {
                    self.handle_fastwrite(&mut t, channel).await?;
                }
                Decoded::Unknown(b) => {
                    tracing::error!(opcode = format_args!("{b:#04x}"), "unknown opcode — closing session");
                    return Err(ServerError::UnknownOpcode(b));
                }
                Decoded::Op(other) => {
                    tracing::error!(?other, "opcode known but unimplemented — closing session to avoid desync");
                    return Err(ServerError::UnknownOpcode(other as u8));
                }
            }
        }
    }

    async fn handle_read<T>(&self, t: &mut T, extended: bool) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut hdr = [0u8; 4];
        t.read_exact(&mut hdr).await?;
        let slot = hdr[0];
        let lsn = Lsn::from_be3([hdr[1], hdr[2], hdr[3]]);

        let disk = self.drives.read().await.get(&slot).cloned();
        let sector: [u8; SECTOR_SIZE] = match disk {
            Some(d) => d.read(lsn).await.unwrap_or_else(|e| {
                tracing::warn!(?e, ?slot, ?lsn, "disk read failed");
                [0u8; SECTOR_SIZE]
            }),
            None => {
                tracing::warn!(slot, ?lsn, "read on empty slot");
                [0u8; SECTOR_SIZE]
            }
        };

        t.write_all(&sector).await?;
        let host_sum = checksum16(&sector);

        if extended {
            // OP_READEX / OP_REREADEX (per hdbdos.asm HREAD): host sends
            // data only, guest sends its 2-byte checksum, host replies
            // with 1 status byte (0x00 OK, 0xF3 = E$CRC = retry).
            let mut guest_sum_bytes = [0u8; 2];
            t.read_exact(&mut guest_sum_bytes).await?;
            let guest_sum = u16::from_be_bytes(guest_sum_bytes);
            let status = if guest_sum == host_sum {
                0x00
            } else {
                tracing::warn!(
                    ?slot, ?lsn, host_sum, guest_sum, "READEX checksum mismatch"
                );
                DwError::Crc as u8
            };
            t.write_all(&[status]).await?;
        } else {
            // OP_READ / OP_REREAD: host appends its own checksum; guest
            // verifies locally and retries by sending OP_REREAD on mismatch.
            t.write_all(&host_sum.to_be_bytes()).await?;
        }
        Ok(())
    }

    async fn handle_write<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut hdr = [0u8; 4];
        t.read_exact(&mut hdr).await?;
        let slot = hdr[0];
        let lsn = Lsn::from_be3([hdr[1], hdr[2], hdr[3]]);

        let mut sector = [0u8; SECTOR_SIZE];
        t.read_exact(&mut sector).await?;

        let mut sum_bytes = [0u8; 2];
        t.read_exact(&mut sum_bytes).await?;
        let guest_sum = u16::from_be_bytes(sum_bytes);
        let host_sum = checksum16(&sector);

        if guest_sum != host_sum {
            tracing::warn!(slot, ?lsn, guest_sum, host_sum, "write checksum mismatch");
            t.write_all(&[DwError::Crc as u8]).await?;
            return Ok(());
        }

        let disk = self.drives.read().await.get(&slot).cloned();
        let status = match disk {
            Some(d) => match d.write(lsn, &sector).await {
                Ok(()) => 0u8,
                Err(e) => {
                    tracing::warn!(?e, ?slot, ?lsn, "disk write failed");
                    DwError::Write as u8
                }
            },
            None => DwError::NotReady as u8,
        };
        t.write_all(&[status]).await?;
        Ok(())
    }

    async fn handle_dwinit<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut driver = [0u8; 1];
        t.read_exact(&mut driver).await?;
        tracing::info!(driver = driver[0], "DWINIT");
        t.write_all(&[DWINIT_RESPONSE]).await?;
        Ok(())
    }

    async fn handle_stat<T>(&self, t: &mut T, kind: &'static str) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut p = [0u8; 2];
        t.read_exact(&mut p).await?;
        tracing::debug!(kind, drive = p[0], code = format_args!("{:#04x}", p[1]), "stat");
        Ok(())
    }

    async fn handle_print<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut b = [0u8; 1];
        t.read_exact(&mut b).await?;
        self.print_buffer.lock().await.push(b[0]);
        Ok(())
    }

    async fn handle_print_flush(&self) {
        let mut buf = self.print_buffer.lock().await;
        if buf.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut *buf);
        let text = String::from_utf8_lossy(&bytes);
        tracing::info!(len = bytes.len(), text = %text, "printer flush");
    }

    async fn handle_fastwrite<T>(&self, t: &mut T, channel: u8) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut b = [0u8; 1];
        t.read_exact(&mut b).await?;
        self.push_vserial(channel, b[0]).await;
        Ok(())
    }

    /// Buffer one guest-originated byte into the named vserial channel.
    /// Out-of-range channels are dropped with a warning rather than
    /// panicking — the wire is untrusted input.
    pub async fn push_vserial(&self, channel: u8, byte: u8) {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            tracing::warn!(channel, "vserial write to invalid channel index");
            return;
        }
        {
            let mut inbox = self.vserial_inbox.lock().await;
            inbox[idx].push_back(byte);
        }
        self.vserial_inbox_notify[idx].notify_one();
        tracing::trace!(channel, byte = format_args!("{:#04x}", byte), "vserial in");
    }

    /// Drain accumulated bytes the guest has written to a channel.
    /// Intended for the host-side attach path (PTY / Unix socket) and
    /// for tests.
    pub async fn drain_vserial(&self, channel: u8) -> Vec<u8> {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            return Vec::new();
        }
        let mut inbox = self.vserial_inbox.lock().await;
        inbox[idx].drain(..).collect()
    }

    /// Queue host-originated bytes for delivery to the guest on a vserial
    /// channel. The guest will pick these up via OP_SERREAD / OP_SERREADM.
    pub async fn send_to_vserial(&self, channel: u8, bytes: &[u8]) {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            tracing::warn!(channel, "send_to_vserial on invalid channel index");
            return;
        }
        let mut out = self.vserial_outbox.lock().await;
        out[idx].extend(bytes.iter().copied());
    }

    /// Number of bytes queued for the guest on a channel. Useful for
    /// monitoring / tests.
    pub async fn vserial_outbox_len(&self, channel: u8) -> usize {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            return 0;
        }
        self.vserial_outbox.lock().await[idx].len()
    }

    /// Wait until `push_vserial` deposits new bytes for `channel`.
    pub async fn wait_vserial_inbox(&self, channel: u8) {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            // Wait forever on an invalid channel — caller bug.
            std::future::pending::<()>().await;
            return;
        }
        self.vserial_inbox_notify[idx].notified().await;
    }

    /// Per-drive status row: slot, human name (usually file path), sector count.
    pub async fn drive_summary(&self) -> Vec<(u8, String, u32)> {
        let drives = self.drives.read().await;
        let mut rows: Vec<_> = drives
            .iter()
            .map(|(&slot, d)| (slot, d.name(), d.sector_count()))
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    }

    /// Per-channel vserial status row: channel, is-open, inbox-len, outbox-len.
    pub async fn vserial_summary(&self) -> Vec<(u8, bool, usize, usize)> {
        let opens = self.vserial_open.lock().await.clone();
        let inboxes = self.vserial_inbox.lock().await;
        let outboxes = self.vserial_outbox.lock().await;
        (0..VSERIAL_CHANNELS)
            .map(|i| (i as u8, opens[i], inboxes[i].len(), outboxes[i].len()))
            .collect()
    }

    /// Open a `.dsk` file and mount it on `slot`. Convenience over the
    /// generic `mount(...)` so the control socket can install disks
    /// without the cli having to ship the VDisk trait knowledge.
    pub async fn mount_dsk(
        &self,
        slot: u8,
        path: &std::path::Path,
        read_only: bool,
    ) -> Result<(u32, String), VDiskError> {
        let disk = DskFile::open(path, read_only).await?;
        let sectors = disk.sector_count();
        let name = disk.name();
        self.mount(slot, disk).await;
        Ok((sectors, name))
    }

    /// Bind a Unix-domain socket and serve a line-based control protocol:
    ///     MOUNT <slot> <path>
    ///     UNMOUNT <slot>
    ///     STATUS
    ///     QUIT
    /// Each command yields `OK\n`, `ERROR <msg>\n`, or for STATUS a series
    /// of `drive`/`vserial` lines terminated by `OK\n`.
    pub async fn run_control_listener(
        self: Arc<Self>,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let path = path.as_ref();
        let _ = tokio::fs::remove_file(path).await;
        let listener = tokio::net::UnixListener::bind(path)?;
        tracing::info!(path = %path.display(), "control socket bound");
        loop {
            let (stream, _addr) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = server.handle_control(stream).await {
                    tracing::warn!(?e, "control session ended");
                }
            });
        }
    }

    /// Drive a single control-socket session.
    pub async fn handle_control<S>(self: Arc<Self>, stream: S) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response = self.handle_control_command(trimmed).await;
            w.write_all(response.as_bytes()).await?;
            if response.starts_with("BYE") {
                break;
            }
        }
        Ok(())
    }

    async fn handle_control_command(&self, line: &str) -> String {
        let mut it = line.splitn(3, char::is_whitespace);
        let verb = it.next().unwrap_or("").to_ascii_uppercase();
        match verb.as_str() {
            "MOUNT" => match (it.next(), it.next()) {
                (Some(slot), Some(path)) => match slot.parse::<u8>() {
                    Ok(slot) => match self
                        .mount_dsk(slot, std::path::Path::new(path), false)
                        .await
                    {
                        Ok((sectors, name)) => {
                            format!("OK mounted slot {slot} sectors={sectors} path={name}\n")
                        }
                        Err(e) => format!("ERROR mount failed: {e}\n"),
                    },
                    Err(_) => format!("ERROR slot must be a u8 (got {slot:?})\n"),
                },
                _ => "ERROR usage: MOUNT <slot> <path>\n".into(),
            },
            "UNMOUNT" => match it.next() {
                Some(slot) => match slot.parse::<u8>() {
                    Ok(slot) => {
                        self.unmount(slot).await;
                        format!("OK unmounted slot {slot}\n")
                    }
                    Err(_) => format!("ERROR slot must be a u8 (got {slot:?})\n"),
                },
                None => "ERROR usage: UNMOUNT <slot>\n".into(),
            },
            "STATUS" => {
                let mut out = String::new();
                for (slot, name, sectors) in self.drive_summary().await {
                    out.push_str(&format!("drive {slot} {sectors} {name}\n"));
                }
                for (ch, open, inbox, outbox) in self.vserial_summary().await {
                    if open || inbox > 0 || outbox > 0 {
                        let state = if open { "open" } else { "closed" };
                        out.push_str(&format!(
                            "vserial {ch} {state} inbox={inbox} outbox={outbox}\n"
                        ));
                    }
                }
                out.push_str("OK\n");
                out
            }
            "QUIT" => "BYE\n".into(),
            other => format!("ERROR unknown verb: {other}\n"),
        }
    }

    /// Bind a Unix-domain socket at `path` and accept `dw attach` clients.
    /// Each connection sends a single channel-id byte, then becomes the
    /// bidirectional pipe for that channel (via `handle_attach`).
    pub async fn run_attach_listener(
        self: Arc<Self>,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let path = path.as_ref();
        let _ = tokio::fs::remove_file(path).await; // best-effort stale-socket cleanup
        let listener = tokio::net::UnixListener::bind(path)?;
        tracing::info!(path = %path.display(), "attach socket bound");
        loop {
            let (mut stream, _addr) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let mut ch_byte = [0u8; 1];
                if stream.read_exact(&mut ch_byte).await.is_err() {
                    return;
                }
                tracing::info!(channel = ch_byte[0], "attach client connected");
                let _ = server.handle_attach(stream, ch_byte[0]).await;
                tracing::info!(channel = ch_byte[0], "attach client disconnected");
            });
        }
    }

    /// Pipe an attached host-side reader/writer to a vserial channel.
    /// Bytes read from the stream are forwarded to the guest via the
    /// outbox; bytes the guest writes into the inbox are written out to
    /// the stream. Returns when either direction closes.
    pub async fn handle_attach<S>(
        self: Arc<Self>,
        stream: S,
        channel: u8,
    ) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);

        let server_in = Arc::clone(&self);
        let inbound = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => server_in.send_to_vserial(channel, &buf[..n]).await,
                    Err(e) => {
                        tracing::debug!(?e, channel, "attach reader closed");
                        break;
                    }
                }
            }
        });

        let server_out = Arc::clone(&self);
        let outbound = tokio::spawn(async move {
            loop {
                let bytes = server_out.drain_vserial(channel).await;
                if !bytes.is_empty() {
                    if let Err(e) = writer.write_all(&bytes).await {
                        tracing::debug!(?e, channel, "attach writer closed");
                        break;
                    }
                    continue;
                }
                server_out.wait_vserial_inbox(channel).await;
            }
        });

        tokio::select! {
            _ = inbound => {}
            _ = outbound => {}
        }
        Ok(())
    }

    /// SERREAD response encoding follows pyDriveWire dwserver.py
    /// `cmdSerRead`: 2-byte reply where byte 1 is a status nibble + channel
    /// id, byte 2 is either the data byte (single-byte ready) or the count
    /// of bytes waiting (multi-byte ready, guest must follow up with
    /// SERREADM).
    async fn handle_serread<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut out = self.vserial_outbox.lock().await;
        for (idx, q) in out.iter_mut().enumerate() {
            if q.is_empty() {
                continue;
            }
            if q.len() < 3 {
                // 1-2 bytes ready: deliver one byte inline.
                let byte = q.pop_front().expect("non-empty");
                t.write_all(&[0x01 + idx as u8, byte]).await?;
                return Ok(());
            }
            // 3+ bytes ready: tell guest to issue SERREADM.
            let count = q.len().min(255) as u8;
            t.write_all(&[0x11 + idx as u8, count]).await?;
            return Ok(());
        }
        // No data on any channel.
        t.write_all(&[0x00, 0x00]).await?;
        Ok(())
    }

    /// SERWRITEM: guest sends [channel, count, ...count bytes]. Bytes go
    /// into the channel's host-bound inbox.
    async fn handle_serwritem<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut hdr = [0u8; 2];
        t.read_exact(&mut hdr).await?;
        let channel = hdr[0];
        let count = hdr[1] as usize;
        let mut buf = vec![0u8; count];
        t.read_exact(&mut buf).await?;
        let idx = channel as usize;
        if idx < VSERIAL_CHANNELS {
            let mut inbox = self.vserial_inbox.lock().await;
            inbox[idx].extend(buf.iter().copied());
        } else {
            tracing::warn!(channel, count, "SERWRITEM to invalid channel");
        }
        Ok(())
    }

    /// SERSETSTAT: 2 bytes [channel, code]. SS.Open / SS.Close have no
    /// payload; SS.ComSt has a 26-byte OS-9 dev_t-style payload that we
    /// must consume to stay in sync (we don't need its contents yet).
    async fn handle_sersetstat<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut hdr = [0u8; 2];
        t.read_exact(&mut hdr).await?;
        let channel = hdr[0];
        let code = hdr[1];
        match code {
            ss::OPEN => {
                self.set_channel_open(channel, true).await;
                tracing::info!(channel, "SS.Open");
            }
            ss::CLOSE => {
                self.set_channel_open(channel, false).await;
                tracing::info!(channel, "SS.Close");
            }
            ss::COMST => {
                let mut payload = [0u8; ss::COMST_PAYLOAD_LEN];
                t.read_exact(&mut payload).await?;
                tracing::debug!(channel, "SS.ComSt (26-byte status consumed)");
            }
            other => {
                tracing::debug!(channel, code = format_args!("{other:#04x}"), "SERSETSTAT");
            }
        }
        Ok(())
    }

    async fn set_channel_open(&self, channel: u8, open: bool) {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            return;
        }
        let mut state = self.vserial_open.lock().await;
        state[idx] = open;
    }

    /// Whether the named channel has been opened by the guest.
    pub async fn is_channel_open(&self, channel: u8) -> bool {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            return false;
        }
        *self
            .vserial_open
            .lock()
            .await
            .get(idx)
            .unwrap_or(&false)
    }

    /// SERREADM: guest sends [channel, count] then expects `count` bytes.
    /// Out-of-range counts are zero-padded; an unknown channel is treated
    /// as empty.
    async fn handle_serreadm<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut p = [0u8; 2];
        t.read_exact(&mut p).await?;
        let channel = p[0] as usize;
        let count = p[1] as usize;
        let mut buf = vec![0u8; count];
        if channel < VSERIAL_CHANNELS {
            let mut out = self.vserial_outbox.lock().await;
            for slot in buf.iter_mut().take(count) {
                *slot = out[channel].pop_front().unwrap_or(0);
            }
        }
        t.write_all(&buf).await?;
        Ok(())
    }

    async fn handle_time<T>(&self, t: &mut T) -> Result<(), ServerError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // 6 bytes: year-1900, month (1..=12), day (1..=31), hour, minute,
        // second. Local time when available, UTC otherwise.
        let now = time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let year_byte = (now.year() - 1900).clamp(0, 255) as u8;
        let resp = [
            year_byte,
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        ];
        t.write_all(&resp).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn dwinit_replies_with_one_byte() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0x5A, 0x42]).await.unwrap();

        let mut resp = [0u8; 1];
        client.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [DWINIT_RESPONSE]);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn time_replies_with_six_plausible_bytes() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0x23]).await.unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]), "month {}", resp[1]);
        assert!((1..=31).contains(&resp[2]), "day {}", resp[2]);
        assert!(resp[3] <= 23, "hour {}", resp[3]);
        assert!(resp[4] <= 59, "minute {}", resp[4]);
        assert!(resp[5] <= 60, "second {}", resp[5]);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn print_and_flush_do_not_desync_stream() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // PRINT 'H', PRINT 'i', PRINTFLUSH, then TIME as a sentinel.
        client
            .write_all(&[0x50, b'H', 0x50, b'i', 0x46, 0x23])
            .await
            .unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn init_term_getstat_setstat_consume_payload() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // INIT, GETSTAT(drv=0, code=0x05), SETSTAT(drv=0, code=0x06), TERM,
        // then TIME as a sentinel proving none of the above desynced.
        client
            .write_all(&[0x49, 0x47, 0x00, 0x05, 0x53, 0x00, 0x06, 0x54, 0x23])
            .await
            .unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn unknown_opcode_closes_session() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0x01]).await.unwrap();
        drop(client);

        let res = task.await.unwrap();
        assert!(matches!(res, Err(ServerError::UnknownOpcode(0x01))));
    }

    #[tokio::test]
    async fn serread_replies_no_data_when_outboxes_empty() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0x43]).await.unwrap();

        let mut resp = [0xFFu8; 2];
        client.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x00, 0x00]);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn serread_delivers_single_byte_inline() {
        let server = Arc::new(Server::new());
        server.send_to_vserial(3, b"Q").await; // 1 byte queued for ch 3

        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0x43]).await.unwrap();
        let mut resp = [0u8; 2];
        client.read_exact(&mut resp).await.unwrap();
        // 1-2 bytes ready encoding: byte1 = 0x01 + channel
        assert_eq!(resp, [0x01 + 3, b'Q']);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn serread_then_serreadm_for_multibyte_burst() {
        let server = Arc::new(Server::new());
        server.send_to_vserial(2, b"hello").await; // 5 bytes >= 3

        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // First poll: expect multi-byte indicator [0x13 (=0x11+2), 5].
        client.write_all(&[0x43]).await.unwrap();
        let mut resp = [0u8; 2];
        client.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x11 + 2, 5]);

        // Follow-up SERREADM (0x63) with [channel=2, count=5] → 5 bytes.
        client.write_all(&[0x63, 0x02, 0x05]).await.unwrap();
        let mut payload = [0u8; 5];
        client.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"hello");

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn serinit_serterm_consume_channel_byte() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // SERINIT ch=2, SERTERM ch=2, then TIME sentinel.
        client.write_all(&[0x45, 0x02, 0xC5, 0x02, 0x23]).await.unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn fastwrite_buffers_byte_into_channel_inbox() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let server_for_run = Arc::clone(&server);
        let task = tokio::spawn(async move { server_for_run.run(server_side).await });

        // FASTWRITE ch 0 byte 'A', ch 0 byte 'B', ch 1 byte 'X', then TIME.
        client
            .write_all(&[0x80, b'A', 0x80, b'B', 0x81, b'X', 0x23])
            .await
            .unwrap();
        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;

        assert_eq!(server.drain_vserial(0).await, b"AB");
        assert_eq!(server.drain_vserial(1).await, b"X");
        assert!(server.drain_vserial(2).await.is_empty());
    }

    #[tokio::test]
    async fn control_status_lists_drives_and_open_channels() {
        let server = Arc::new(Server::new());
        server.set_channel_open(2, true).await;
        server.send_to_vserial(2, b"buffered").await;

        let resp = server.handle_control_command("STATUS").await;
        assert!(resp.contains("vserial 2 open"), "got: {resp}");
        assert!(resp.contains("outbox=8"), "got: {resp}");
        assert!(resp.ends_with("OK\n"), "got: {resp}");
    }

    #[tokio::test]
    async fn control_mount_errors_on_missing_path() {
        let server = Arc::new(Server::new());
        let resp = server
            .handle_control_command("MOUNT 0 /definitely/not/a/real/path.dsk")
            .await;
        assert!(resp.starts_with("ERROR mount failed"), "got: {resp}");
    }

    #[tokio::test]
    async fn control_unmount_is_idempotent() {
        let server = Arc::new(Server::new());
        let resp = server.handle_control_command("UNMOUNT 0").await;
        assert!(resp.starts_with("OK unmounted"), "got: {resp}");
    }

    #[tokio::test]
    async fn control_rejects_bad_slot() {
        let server = Arc::new(Server::new());
        let resp = server.handle_control_command("MOUNT abc /x").await;
        assert!(resp.starts_with("ERROR slot must be"), "got: {resp}");
    }

    #[tokio::test]
    async fn control_unknown_verb_returns_error() {
        let server = Arc::new(Server::new());
        let resp = server.handle_control_command("FOOBAR").await;
        assert!(resp.starts_with("ERROR unknown verb"), "got: {resp}");
    }

    #[tokio::test]
    async fn control_quit_returns_bye() {
        let server = Arc::new(Server::new());
        let resp = server.handle_control_command("QUIT").await;
        assert_eq!(resp, "BYE\n");
    }

    #[tokio::test]
    async fn attach_forwards_client_writes_into_outbox() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(256);
        let s = Arc::clone(&server);
        let task = tokio::spawn(async move { s.handle_attach(server_side, 1).await });

        client.write_all(b"hello").await.unwrap();

        // Spin briefly for the inbound task to drain the duplex.
        for _ in 0..50 {
            if server.vserial_outbox_len(1).await == 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(server.vserial_outbox_len(1).await, 5);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn attach_forwards_inbox_bytes_to_client() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(256);
        let s = Arc::clone(&server);
        let task = tokio::spawn(async move { s.handle_attach(server_side, 2).await });

        // Simulate the guest writing into the inbox.
        for b in b"hi" {
            server.push_vserial(2, *b).await;
        }

        let mut buf = [0u8; 2];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn sersetstat_open_close_tracks_channel_state() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let server_for_run = Arc::clone(&server);
        let task = tokio::spawn(async move { server_for_run.run(server_side).await });

        // SERSETSTAT ch=4 code=SS.Open(0x29), then SERSETSTAT ch=4 SS.Close(0x2A),
        // followed by TIME so we can assert no desync.
        client.write_all(&[0xC4, 0x04, 0x29]).await.unwrap();
        client.write_all(&[0x23]).await.unwrap();
        let mut t1 = [0u8; 6];
        client.read_exact(&mut t1).await.unwrap();
        assert!((1..=12).contains(&t1[1]));
        assert!(server.is_channel_open(4).await);

        client.write_all(&[0xC4, 0x04, 0x2A, 0x23]).await.unwrap();
        let mut t2 = [0u8; 6];
        client.read_exact(&mut t2).await.unwrap();
        assert!((1..=12).contains(&t2[1]));
        assert!(!server.is_channel_open(4).await);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn sersetstat_comst_consumes_26_byte_payload() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let server_for_run = Arc::clone(&server);
        let task = tokio::spawn(async move { server_for_run.run(server_side).await });

        // SERSETSTAT ch=0 code=SS.ComSt(0x28) + 26 payload bytes, then TIME.
        let mut frame = vec![0xC4, 0x00, 0x28];
        frame.extend(std::iter::repeat(0xAB).take(26));
        frame.push(0x23);
        client.write_all(&frame).await.unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn serwritem_buffers_burst_into_channel_inbox() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let server_for_run = Arc::clone(&server);
        let task = tokio::spawn(async move { server_for_run.run(server_side).await });

        // SERWRITEM (0x64) ch=3 count=5 bytes=b"hello", then TIME sentinel.
        let mut frame = vec![0x64, 0x03, 0x05];
        frame.extend_from_slice(b"hello");
        frame.push(0x23);
        client.write_all(&frame).await.unwrap();

        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;

        assert_eq!(server.drain_vserial(3).await, b"hello");
    }

    #[tokio::test]
    async fn serwrite_buffers_byte_into_channel_inbox() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(64);
        let server_for_run = Arc::clone(&server);
        let task = tokio::spawn(async move { server_for_run.run(server_side).await });

        // SERWRITE (0xC3) ch=2 byte='Z', then TIME sentinel.
        client.write_all(&[0xC3, 0x02, b'Z', 0x23]).await.unwrap();
        let mut resp = [0u8; 6];
        client.read_exact(&mut resp).await.unwrap();
        assert!((1..=12).contains(&resp[1]));

        drop(client);
        let _ = task.await;

        assert_eq!(server.drain_vserial(2).await, b"Z");
    }

    #[tokio::test]
    async fn read_on_empty_slot_returns_zero_sector_with_checksum() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(4096);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // OP_READ + drive=0 + LSN=0
        client.write_all(&[0x52, 0x00, 0x00, 0x00, 0x00]).await.unwrap();

        let mut sector = [0xFFu8; 256];
        client.read_exact(&mut sector).await.unwrap();
        assert!(sector.iter().all(|&b| b == 0));

        let mut cksum = [0u8; 2];
        client.read_exact(&mut cksum).await.unwrap();
        assert_eq!(u16::from_be_bytes(cksum), 0);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn readex_does_bidirectional_checksum_handshake() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(4096);
        let task = tokio::spawn(async move { server.run(server_side).await });

        // OP_READEX + drive=0 + LSN=0
        client.write_all(&[0xD2, 0x00, 0x00, 0x00, 0x00]).await.unwrap();

        // Host should send 256 data bytes (no checksum on the wire here).
        let mut sector = [0xFFu8; 256];
        client.read_exact(&mut sector).await.unwrap();
        assert!(sector.iter().all(|&b| b == 0));

        // Guest sends its 2-byte checksum (matches: zero sector → zero sum).
        client.write_all(&[0x00, 0x00]).await.unwrap();

        // Host replies with 1 status byte: 0x00 = OK.
        let mut status = [0xFFu8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], 0x00);

        drop(client);
        let _ = task.await;
    }

    #[tokio::test]
    async fn readex_reports_crc_when_guest_checksum_wrong() {
        let server = Arc::new(Server::new());
        let (mut client, server_side) = duplex(4096);
        let task = tokio::spawn(async move { server.run(server_side).await });

        client.write_all(&[0xD2, 0x00, 0x00, 0x00, 0x00]).await.unwrap();

        let mut sector = [0u8; 256];
        client.read_exact(&mut sector).await.unwrap();

        // Lie about the checksum.
        client.write_all(&[0xDE, 0xAD]).await.unwrap();

        let mut status = [0u8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], 0xF3); // DwError::Crc

        drop(client);
        let _ = task.await;
    }
}
