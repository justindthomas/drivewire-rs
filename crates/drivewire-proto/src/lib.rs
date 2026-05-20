//! DriveWire 3/4 wire-protocol primitives.
//!
//! Pure (no I/O). Opcode decoding, 24-bit LSN, 16-bit additive checksum,
//! and the single-byte error response codes.

#![forbid(unsafe_code)]

pub mod checksum;
pub mod error;
pub mod lsn;
pub mod opcode;

pub use checksum::checksum16;
pub use error::DwError;
pub use lsn::Lsn;
pub use opcode::{Decoded, Opcode};
