//! 24-bit Logical Sector Number used by OP_READ / OP_WRITE.

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lsn(pub u32);

impl Lsn {
    pub const MAX: Lsn = Lsn(0x00FF_FFFF);

    /// Build from the 3-byte big-endian LSN field in a READ/WRITE packet.
    pub fn from_be3(bytes: [u8; 3]) -> Self {
        Lsn(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    pub fn to_be3(self) -> [u8; 3] {
        let bytes = self.0.to_be_bytes();
        [bytes[1], bytes[2], bytes[3]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for v in [0u32, 1, 0xFF, 0x100, 0xFFFF, 0x12_3456, 0xFF_FFFF] {
            let lsn = Lsn(v);
            assert_eq!(Lsn::from_be3(lsn.to_be3()), lsn);
        }
    }

    #[test]
    fn parses_big_endian() {
        assert_eq!(Lsn::from_be3([0x12, 0x34, 0x56]), Lsn(0x12_3456));
    }
}
