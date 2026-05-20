//! Single-byte error responses defined by the DriveWire protocol.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DwError {
    Crc = 0xF3,
    Read = 0xF4,
    Write = 0xF5,
    NotReady = 0xF6,
}
