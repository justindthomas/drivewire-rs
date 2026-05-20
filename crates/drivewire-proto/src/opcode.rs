//! DriveWire opcode definitions.
//!
//! Values match the DriveWire Specification on the DrPitre/DriveWire wiki.

use core::convert::TryFrom;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    // DW3
    Nop = 0x00,
    Time = 0x23,
    PrintFlush = 0x46,
    GetStat = 0x47,
    Init = 0x49,
    Print = 0x50,
    Read = 0x52,
    SetStat = 0x53,
    Term = 0x54,
    Write = 0x57,
    Reread = 0x72,
    Rewrite = 0x77,
    ReadEx = 0xD2,
    RereadEx = 0xF2,
    Reset2 = 0xFE,
    Reset1 = 0xFF,

    // DW4
    WirebugMode = 0x42,
    SerRead = 0x43,
    SerGetStat = 0x44,
    SerInit = 0x45,
    DwInit = 0x5A,
    SerReadM = 0x63,
    SerWriteM = 0x64,
    SerWrite = 0xC3,
    SerSetStat = 0xC4,
    SerTerm = 0xC5,
    Reset3 = 0xF8,
}

/// One decoded opcode byte. `FastWrite` collapses the 0x80..=0x8F range
/// (OP_FASTWRITE_BASE + channel) into a single variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    Op(Opcode),
    FastWrite { channel: u8 },
    Unknown(u8),
}

impl Decoded {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x80..=0x8F => Decoded::FastWrite { channel: b - 0x80 },
            _ => match Opcode::try_from(b) {
                Ok(op) => Decoded::Op(op),
                Err(_) => Decoded::Unknown(b),
            },
        }
    }
}

impl TryFrom<u8> for Opcode {
    type Error = u8;

    fn try_from(b: u8) -> Result<Self, Self::Error> {
        Ok(match b {
            0x00 => Opcode::Nop,
            0x23 => Opcode::Time,
            0x42 => Opcode::WirebugMode,
            0x43 => Opcode::SerRead,
            0x44 => Opcode::SerGetStat,
            0x45 => Opcode::SerInit,
            0x46 => Opcode::PrintFlush,
            0x47 => Opcode::GetStat,
            0x49 => Opcode::Init,
            0x50 => Opcode::Print,
            0x52 => Opcode::Read,
            0x53 => Opcode::SetStat,
            0x54 => Opcode::Term,
            0x57 => Opcode::Write,
            0x5A => Opcode::DwInit,
            0x63 => Opcode::SerReadM,
            0x64 => Opcode::SerWriteM,
            0x72 => Opcode::Reread,
            0x77 => Opcode::Rewrite,
            0xC3 => Opcode::SerWrite,
            0xC4 => Opcode::SerSetStat,
            0xC5 => Opcode::SerTerm,
            0xD2 => Opcode::ReadEx,
            0xF2 => Opcode::RereadEx,
            0xF8 => Opcode::Reset3,
            0xFE => Opcode::Reset2,
            0xFF => Opcode::Reset1,
            other => return Err(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_opcodes() {
        assert_eq!(Decoded::from_byte(0x52), Decoded::Op(Opcode::Read));
        assert_eq!(Decoded::from_byte(0x57), Decoded::Op(Opcode::Write));
        assert_eq!(Decoded::from_byte(0x5A), Decoded::Op(Opcode::DwInit));
        assert_eq!(Decoded::from_byte(0xFF), Decoded::Op(Opcode::Reset1));
    }

    #[test]
    fn decodes_fastwrite_range() {
        assert_eq!(Decoded::from_byte(0x80), Decoded::FastWrite { channel: 0 });
        assert_eq!(Decoded::from_byte(0x8F), Decoded::FastWrite { channel: 15 });
    }

    #[test]
    fn unknown_byte_is_unknown() {
        assert_eq!(Decoded::from_byte(0x01), Decoded::Unknown(0x01));
    }
}
