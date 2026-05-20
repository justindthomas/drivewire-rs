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
| **Transports** | TCP (Becker port, default `:65504`) and serial via `tokio-serial` |
| **Tests** | 28 unit + integration tests passing (proto, server, CLI line-ending) |
| **Real-driver validation** | HDB-DOS Becker (DIR + DSKINI full-disk format) and NitrOS-9 6809 L2 v3.3.0 (boots to interactive `Shell+`) |

### Not yet implemented

- `OP_WIREBUG_MODE` (debugger), `OP_PLAYSOUND`, named-object mounts.
- "Carrier-detect"–style modem signaling on vserial (most CoCo software
  doesn't need it — `tsmon` certainly doesn't).
- A `dw attach` escape-sequence (e.g. `Ctrl-A q`) — for now, kill the attach
  process from another terminal.
- `dw mount` / `dw status` over the control socket.

## Workspace layout

| crate | role |
|---|---|
| `drivewire-proto` | Pure opcode / LSN / checksum types, no I/O |
| `drivewire-vdisk` | `VDisk` trait + flat `.dsk` backend |
| `drivewire-transport` | Serial + TCP transport helpers |
| `drivewire-server` | Opcode-dispatched protocol state machine, attach-socket multiplexer |
| `drivewire-cli` | The `dw` binary (`serve`, `attach`) |

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
  --serial /dev/tty.usbserial-XYZ \
  --baud 57600 \
  --disk0 path/to/disk.dsk
```

### Attach to a vserial channel (SSH-console)

After NitrOS-9 boots and `tsmon /N1&` is running on the guest:

```bash
target/release/dw attach 1
```

Your terminal goes into raw mode, presses Enter to wake `tsmon`, and you log
in. See `examples/nitros9-multi-tty.sh` for a script that patches a NitrOS-9
boot disk so `/N1`, `/N2`, and `/N3` all have `tsmon` listeners auto-started.

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
