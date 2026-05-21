# drivewire-rs

A cross-platform [DriveWire 3/4][spec] server, written in Rust. Hosts virtual
disks and virtual serial channels for a Tandy/TRS-80 Color Computer or Dragon
connected over real serial or a TCP "Becker port".

## What it does

DriveWire is a protocol that lets a vintage 6809/6309 machine treat a modern
host as its mass-storage, printer, and modem. Your CoCo issues sector reads
and writes over a serial cable; the host turns them into reads/writes on a
`.dsk` file. The DW4 extension adds **virtual serial channels** — the CoCo
can open `/N1`, `/N2`, ..., and the host bridges those channels to PTYs,
sockets, or anything else.

`drivewire-rs` is a clean-room implementation of the server side, validated
against the real HDB-DOS and NitrOS-9 6809 drivers. Plug it into [XRoar][xroar]
or a real CoCo, mount a disk image, and you get a working filesystem. With
`dw attach`, you also get a **bidirectional terminal pipe** from your host
shell into a NitrOS-9 login over the same wire — SSH into the host, run
`dw attach 1`, and you're in `Shell+` on the CoCo.

## Status

| | |
|---|---|
| **DW3 disk I/O** | `OP_READ`, `OP_READEX`, `OP_REREAD`, `OP_REREADEX`, `OP_WRITE`, `OP_REWRITE` — full bidirectional checksum |
| **DW4 system** | `OP_DWINIT`, `OP_TIME` (local clock), `OP_INIT`, `OP_TERM`, `OP_GETSTAT`, `OP_SETSTAT`, `OP_PRINT`/`OP_PRINTFLUSH`, `OP_NOP`, `OP_RESET1/2/3` |
| **DW4 vserial** | `OP_SERINIT`, `OP_SERTERM`, `OP_SERREAD`, `OP_SERREADM`, `OP_SERWRITE`, `OP_SERWRITEM`, `OP_SERSETSTAT` (incl. SS.Open / SS.Close / SS.ComSt 26-byte payload), `OP_SERGETSTAT`, `OP_FASTWRITE0..15` |
| **Transports** | TCP (Becker port, default `:65504`) and serial — own raw-`termios` blocking-thread backend, 8-N-1 |
| **Tests** | 45 unit + integration tests passing (proto, server, CLI) |
| **Emulator validation** | XRoar + HDB-DOS Becker (DIR + DSKINI full-disk format) and NitrOS-9 6809 L2 v3.3.0 (boots to interactive `Shell+`) |
| **Real-hardware validation** | NitrOS-9 6809 L2 v3.3.0 boots to an interactive shell on a CoCo3FPGA (Altera DE-1) over a physical USB-serial link |

### Not yet implemented

- `OP_WIREBUG_MODE` (the WireBug remote 6809/6309 debugger), `OP_PLAYSOUND`
  / `OP_PLYSNDSTP`, and DW4 named-object mounts. None are required by
  HDB-DOS or NitrOS-9 in their current configurations — these are niche
  features awaiting a use case to drive their wire-format details out.
- "Carrier-detect"–style modem signaling on vserial. `tsmon` doesn't
  probe for carrier (it `I$ReadLn`s and waits for a wake byte), so the
  SSH-console path works without it. Some modem-emulating apps (BBSes,
  XMODEM utilities) may need it; will add `OP_SERGETSTAT` synthetic
  responses when one of them surfaces.

## Workspace layout

| crate | role |
|---|---|
| `drivewire-proto` | Pure opcode / LSN / checksum types, no I/O |
| `drivewire-vdisk` | `VDisk` trait + flat `.dsk` backend |
| `drivewire-transport` | Serial + TCP transport helpers |
| `drivewire-server` | Opcode-dispatched protocol state machine, attach-socket multiplexer |
| `drivewire-cli` | The `dw` binary (`serve`, `attach`, `mount`, `unmount`, `status`, `probe`) |

## Quick start

```bash
cargo build --release
```

### Boot a disk over TCP (emulator)

```bash
# Start the server with a NitrOS-9 boot disk:
target/release/dw serve \
  --tcp 127.0.0.1:65504 \
  --disk0 path/to/nos96809l2v030300coco3_becker.dsk

# In XRoar (Becker port matches our defaults):
xroar -machine coco3 -romlist 'rsdos_becker=hdbdw3bc3' \
      -cart becker -cart-autorun -type 'DOS\r'
```

### Real CoCo3 over USB-serial

```bash
target/release/dw serve \
  --serial /dev/cu.usbserial-XYZ \
  --baud 57600 \
  --disk0 path/to/disk.dsk
```

The serial backend is our own — raw `libc::open` + `termios` setup +
blocking reader/writer threads bridged into async with `tokio::sync::mpsc`.
We do *not* use `tokio-serial` / `mio-serial`: real-hardware testing
found that stack delivered corrupt bytes against PL2303-class USB
adapters on macOS, where `screen` and `pyserial` read the same wire
cleanly. Our backend reads byte-for-byte the way they do.

`dw serve --serial` drains stale RX bytes for `--drain-ms` (default 250)
after open, so a half-packet from a previous session can't desync the
first opcode. On macOS use the `/dev/cu.*` device node, not `/dev/tty.*`.

**Cable wiring** (4-pin DIN bitbanger on CoCo to USB-TTL or DB-9 RS-232):

| CoCo DIN pin | Direction | Host |
|---|---|---|
| 4 (CD, also used as TX) | → | host RX |
| 2 (RXD) | ← | host TX |
| 3 (GND) | — | host GND |
| 1 (carrier sense) | (unused) | — |

For real RS-232 you need a level-shifter (e.g. MAX232) between the CoCo's
TTL pins and the DB-9; for a USB-TTL adapter (FTDI cable, CP2102 board)
you can wire DIN-pin-4 → adapter RXD directly.

**ROM on the CoCo3** depends on the cable:

- Bitbanger serial (this section): `hdbdw3cc3.rom` (or `hdbdw4cc3.rom` for
  the DW4 vserial-aware variant). Get them from the Toolshed
  `hdbdos-toolshed-2.1.zip` release.
- Becker port: `hdbdw3bck.rom` / `hdbdw3bc3.rom` — **not for bitbanger
  serial**, only for emulators or the CoCo3FPGA.

If you have a **CoCoSDC**, recent firmware also offers a high-speed UART
mode that bypasses the bitbanger; the wire protocol is identical, you
just get higher baud rates.

### Probe a connection without booting an OS

Bringing up serial for the first time? `dw probe` opens the line,
drains, and listens for the first opcode from the guest — no disks, no
daemon, just "does the handshake work?".

```bash
target/release/dw probe --serial /dev/cu.usbserial-XYZ --baud 115200
# then reset / power-cycle the CoCo to make it talk
```

Success looks like:

```
[probe] opening /dev/cu.usbserial-XYZ at 115200 baud
[probe] waiting up to 10s for a byte from the guest...
[probe] first byte: 0x5a
[probe] OP_DWINIT driver=0x42 — sending DW4 response 0x04
[probe] handshake complete. Cable + ROM + baud are good.
```

Failures get diagnostic guidance (baud mismatch, wrong ROM, cable
wiring). Also works against TCP guests for emulator triage:
`dw probe --tcp 0.0.0.0:65504`.

> **macOS USB-serial note:** baud-rate handling differs wildly by
> chipset. FTDI and CP2102 adapters work out of the box; PL2303 clones
> are unreliable. If `dw probe` shows the same garbage byte at *every*
> `--baud` you try, that's the tell-tale sign of a chipset whose driver
> ignores the rate — switch to an FTDI-based adapter.

### Attach to a vserial channel (SSH-console)

After NitrOS-9 boots and `tsmon /N1&` is running on the guest:

```bash
target/release/dw attach 1
```

Your terminal goes into raw mode. Press Enter to wake `tsmon` and log in.
**Exit with `Ctrl-A q`**; to send a literal Ctrl-A to the guest, type it
twice (`Ctrl-A Ctrl-A`).

See `examples/nitros9-multi-tty.sh` for a script that patches a NitrOS-9
boot disk so `/N1`, `/N2`, and `/N3` all have `tsmon` listeners auto-started.

### Manage drives on a running daemon

```bash
dw status                                   # list drives + open vserial channels
dw mount 1 path/to/games.dsk                # mount in slot 1
dw unmount 1
```

These connect to `/tmp/drivewire-ctl.sock` (the daemon's control socket;
override with `--socket`). The protocol is plain text, so you can also
poke it with `nc -U /tmp/drivewire-ctl.sock` and type `STATUS<Enter>`.

## Testing without a real CoCo

`crates/drivewire-cli/examples/echo-guest.rs` is a synthetic CoCo that
connects over TCP, performs the `DWINIT` handshake, polls `SERREAD`, and
echoes anything it receives back via `OP_FASTWRITE`. Combined with a
`dw attach` client, it exercises the full vserial round-trip with no
emulator or real hardware:

```bash
# Terminal 1
target/release/dw serve --tcp 127.0.0.1:65504

# Terminal 2
cargo run --release --example echo-guest -- 127.0.0.1:65504

# Terminal 3
target/release/dw attach 1
# type → see your bytes come back through echo-guest
```

## Acknowledgements

- The DriveWire protocol spec by Boisy Pitre and contributors:
  <https://github.com/DrPitre/DriveWire>
- Aaron Wolfe's [DriveWire 4 Server][dw4] (Java) — the canonical reference
  implementation.
- Mike Furman's [pyDriveWire][pydw] (Python) — invaluable for clarifying the
  exact response encoding of `OP_SERREAD` / `OP_SERREADM` / `OP_SERSETSTAT`.
- The [NitrOS-9 project][nitros9] and [Toolshed][toolshed] — source of the
  HDB-DOS / `scdwv.dr` assembler that resolved several "what does the wire
  actually do" questions.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

[spec]: https://github.com/DrPitre/DriveWire/wiki/DriveWire-Specification
[xroar]: https://www.6809.org.uk/xroar/
[dw4]: https://sourceforge.net/projects/drivewireserver/
[pydw]: https://github.com/n6il/pyDriveWire
[nitros9]: https://github.com/nitros9project/nitros9
[toolshed]: https://github.com/boisy/toolshed
