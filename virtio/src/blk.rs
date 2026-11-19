//! The virtio block device's requests (5.2.6).
//!
//! A request is three buffers in one chain: a 16-byte header the device
//! reads, the sectors themselves (read by the device for a write, written by
//! it for a read), and one status byte it writes last. As with the GPU, the
//! header is written into bytes here and checked by tests.

/// Read sectors from the device.
pub const T_IN: u32 = 0;
/// Write sectors to it.
pub const T_OUT: u32 = 1;

/// What the status byte says.
pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

/// Bytes in a sector, whatever the device's own block size: requests are
/// always counted in these.
pub const SECTOR: usize = 512;
pub const HEADER_LEN: usize = 16;

/// The modern (virtio 1) device id; QEMU gives it with `disable-legacy=on`.
pub const DEVICE_ID: u16 = 0x1042;
/// The transitional id, which QEMU gives by default.
pub const TRANSITIONAL_ID: u16 = 0x1001;

/// Write the header of a request of type `kind` at sector `sector`, and
/// say how long it is.
pub fn header(out: &mut [u8], kind: u32, sector: u64) -> usize {
    out[..HEADER_LEN].fill(0);
    out[0..4].copy_from_slice(&kind.to_le_bytes());
    // Bytes 4..8 are reserved, and stay zero.
    out[8..16].copy_from_slice(&sector.to_le_bytes());
    HEADER_LEN
}

/// The device's capacity, in sectors, from the first eight bytes of its
/// device-specific configuration (5.2.4).
pub fn capacity(config: [u8; 8]) -> u64 {
    u64::from_le_bytes(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_type_then_reserved_then_sector() {
        let mut out = [0xaa; 20];
        assert_eq!(header(&mut out, T_OUT, 0x0102_0304_0506_0708), 16);
        assert_eq!(&out[..4], &[1, 0, 0, 0]);
        assert_eq!(&out[4..8], &[0; 4]);
        assert_eq!(&out[8..16], &[8, 7, 6, 5, 4, 3, 2, 1]);
        // Nothing past the header is touched.
        assert_eq!(&out[16..], &[0xaa; 4]);
    }

    #[test]
    fn a_read_of_sector_zero_is_all_zeroes() {
        let mut out = [0xff; HEADER_LEN];
        header(&mut out, T_IN, 0);
        assert_eq!(out, [0; HEADER_LEN]);
    }

    #[test]
    fn capacity_is_little_endian() {
        assert_eq!(capacity([8, 0, 0, 0, 0, 0, 0, 0]), 8);
        assert_eq!(capacity([0, 0, 1, 0, 0, 0, 0, 0]), 1 << 16);
    }
}
