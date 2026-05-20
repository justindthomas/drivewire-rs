//! 16-bit additive checksum used by OP_READ / OP_WRITE.

/// Sum of bytes (each promoted to `u16`) with wrapping addition.
pub fn checksum16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |acc, &b| acc.wrapping_add(b as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(checksum16(&[]), 0);
    }

    #[test]
    fn sums_bytes() {
        assert_eq!(checksum16(&[1, 2, 3]), 6);
    }

    #[test]
    fn full_sector_of_aa() {
        let data = [0xAAu8; 256];
        assert_eq!(checksum16(&data), 0xAA00);
    }

    #[test]
    fn wraps_on_overflow() {
        let mut bytes = vec![0xFFu8; 257];
        bytes.push(0x01);
        // 257 * 0xFF + 1 = 0x10000 -> wraps to 0
        assert_eq!(checksum16(&bytes), 0);
    }
}
