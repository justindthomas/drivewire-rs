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
use drivewire_vdisk::{VDisk, SECTOR_SIZE};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};

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

pub struct Server {
    drives: RwLock<HashMap<u8, Arc<dyn VDisk>>>,
    print_buffer: Mutex<Vec<u8>>,
    /// Bytes the guest has written into each vserial channel, waiting to
    /// be picked up by the host side (PTY, attach socket, etc.).
    vserial_inbox: Mutex<Vec<VecDeque<u8>>>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            drives: RwLock::default(),
            print_buffer: Mutex::default(),
            vserial_inbox: Mutex::new(
                (0..VSERIAL_CHANNELS).map(|_| VecDeque::new()).collect(),
            ),
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
                    // DW4 response: 2 bytes. 0x00 = "no data on any channel."
                    // Real host->guest data flow waits on the host-side
                    // outbox (PTY/attach socket) — not implemented yet.
                    t.write_all(&[0x00, 0x00]).await?;
                }
                Decoded::Op(Opcode::SerInit) => {
                    let mut ch = [0u8; 1];
                    t.read_exact(&mut ch).await?;
                    tracing::info!(channel = ch[0], "vserial open");
                }
                Decoded::Op(Opcode::SerTerm) => {
                    let mut ch = [0u8; 1];
                    t.read_exact(&mut ch).await?;
                    tracing::info!(channel = ch[0], "vserial close");
                }
                Decoded::Op(Opcode::SerWrite) => {
                    // 2 bytes: [channel, data]. Buffer into per-channel inbox.
                    let mut p = [0u8; 2];
                    t.read_exact(&mut p).await?;
                    self.push_vserial(p[0], p[1]).await;
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
    async fn push_vserial(&self, channel: u8, byte: u8) {
        let idx = channel as usize;
        if idx >= VSERIAL_CHANNELS {
            tracing::warn!(channel, "vserial write to invalid channel index");
            return;
        }
        let mut inbox = self.vserial_inbox.lock().await;
        inbox[idx].push_back(byte);
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
    async fn serread_replies_no_data() {
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
