//! Synthetic DriveWire guest that echoes any bytes the host writes to it.
//!
//! Connects to a running `dw serve --tcp ADDR` instance pretending to be
//! a CoCo, performs DWINIT, then polls SERREAD. Whenever data is reported
//! ready on a channel, it issues SERREADM to read it and immediately
//! echoes the bytes back via OP_FASTWRITE on the same channel.
//!
//! Combined with `dw attach <ch>` from another terminal, this exercises
//! the full vserial round-trip end-to-end without needing NitrOS-9.
//!
//! Usage:
//!   cargo run --release --example echo-guest -- 127.0.0.1:65504

use std::env;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

const OP_DWINIT: u8 = 0x5A;
const OP_SERREAD: u8 = 0x43;
const OP_SERREADM: u8 = 0x63;
const OP_FASTWRITE_BASE: u8 = 0x80;

#[tokio::main]
async fn main() -> Result<()> {
    let addr = env::args().nth(1).unwrap_or_else(|| "127.0.0.1:65504".into());
    eprintln!("[echo-guest] connecting to {addr}");
    let mut stream = TcpStream::connect(&addr).await?;

    // Pretend to be HDB-DOS-class driver: send DWINIT + a driver id, read 1 byte.
    stream.write_all(&[OP_DWINIT, 0x42]).await?;
    let mut srv_caps = [0u8; 1];
    stream.read_exact(&mut srv_caps).await?;
    eprintln!("[echo-guest] DWINIT exchanged, server caps = {:#04x}", srv_caps[0]);

    loop {
        // Poll: send SERREAD, expect 2-byte response.
        stream.write_all(&[OP_SERREAD]).await?;
        let mut resp = [0u8; 2];
        stream.read_exact(&mut resp).await?;

        match resp[0] {
            0x00 => {
                // No data anywhere. Brief sleep to avoid hot-loop.
                sleep(Duration::from_millis(30)).await;
            }
            code @ 0x01..=0x0F => {
                let channel = code - 0x01;
                let byte = resp[1];
                eprintln!("[echo-guest] single byte ch={channel} {:#04x}", byte);
                echo_back(&mut stream, channel, &[byte]).await?;
            }
            0x10 => {
                eprintln!("[echo-guest] channel {} closing", resp[1]);
            }
            code @ 0x11..=0x1F => {
                let channel = code - 0x11;
                let count = resp[1];
                stream.write_all(&[OP_SERREADM, channel, count]).await?;
                let mut buf = vec![0u8; count as usize];
                stream.read_exact(&mut buf).await?;
                let preview: String = buf
                    .iter()
                    .map(|b| {
                        if b.is_ascii_graphic() || *b == b' ' {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                eprintln!("[echo-guest] burst ch={channel} {count} bytes: {preview:?}");
                echo_back(&mut stream, channel, &buf).await?;
            }
            other => {
                eprintln!("[echo-guest] unhandled SERREAD code {other:#04x}");
            }
        }
    }
}

async fn echo_back(stream: &mut TcpStream, channel: u8, bytes: &[u8]) -> Result<()> {
    let op = OP_FASTWRITE_BASE + channel;
    let mut frame = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        frame.push(op);
        frame.push(b);
    }
    stream.write_all(&frame).await?;
    Ok(())
}
