//! Per-platform USB-serial latency timer adjustment.
//!
//! macOS ships USB-serial drivers (FTDI, PL2303, CH340, …) with a 16 ms
//! latency timer by default — fine for a terminal emulator, brutal for
//! DriveWire's request/response flow where every disk-sector exchange
//! eats two of those windows. The `IOSSDATALAT` ioctl on the open tty
//! lets us request a 1 ms timer instead.

// This module is the only place we need raw ioctl, so we localize the
// unsafe waiver here instead of opening it up in the crate root.
#![allow(unsafe_code)]

use std::io;

#[cfg(target_os = "macos")]
pub(crate) fn set(port: &tokio_serial::SerialStream, latency_ms: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // IOSSDATALAT = _IOW('T', 0, c_ulong) on macOS.
    //   _IOC(IOC_IN=0x80000000, group='T'=0x54, num=0, sizeof(c_ulong)=8)
    //   = 0x80000000 | (8 << 16) | (0x54 << 8) | 0
    //   = 0x80085400
    const IOSSDATALAT: libc::c_ulong = 0x80085400;

    let fd = port.as_raw_fd();
    let lat: libc::c_ulong = latency_ms as libc::c_ulong;
    // SAFETY: fd is a live tty fd owned by `port`; `lat` lives for the
    // duration of this call; IOSSDATALAT writes (`_IOW`) exactly one
    // c_ulong from the supplied pointer.
    let ret = unsafe { libc::ioctl(fd, IOSSDATALAT, &lat) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set(_port: &tokio_serial::SerialStream, _latency_ms: u64) -> io::Result<()> {
    // Linux equivalent (TIOCSSERIAL with ASYNC_LOW_LATENCY, or the
    // /sys/bus/usb-serial/devices/.../latency_timer sysfs knob) is left
    // for when a real-CoCo Linux user surfaces. Returning Ok keeps the
    // call site clean.
    Ok(())
}
