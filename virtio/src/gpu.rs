//! The virtio GPU's control commands (5.7.6).
//!
//! Every request starts with the same 24-byte header and so does every
//! response; the response's type says whether it worked. These functions
//! write requests into a byte buffer and read responses out of one, so the
//! layout is checked by tests rather than trusted to a `repr(C)` struct.
//!
//! Only 2D. A resource is a block of pixels the host keeps; the driver
//! gives it backing pages in guest memory, copies rectangles from those
//! into it with a transfer, and shows it by pointing a scanout at it.

pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const CMD_SET_SCANOUT: u32 = 0x0103;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
/// The first error type; every error is at or above it.
pub const RESP_ERR_UNSPEC: u32 = 0x1200;

/// Bytes, little-endian, blue then green then red then unused: the same
/// pixels as a `0x00RRGGBB` word, which is what every other surface in
/// Corrode is.
pub const FORMAT_B8G8R8X8_UNORM: u32 = 2;

pub const HEADER_LEN: usize = 24;
pub const MAX_SCANOUTS: usize = 16;
pub const DISPLAY_INFO_LEN: usize = HEADER_LEN + MAX_SCANOUTS * 24;
pub const CREATE_2D_LEN: usize = HEADER_LEN + 16;
pub const SET_SCANOUT_LEN: usize = HEADER_LEN + 24;
pub const FLUSH_LEN: usize = HEADER_LEN + 24;
pub const TRANSFER_LEN: usize = HEADER_LEN + 32;
pub const ATTACH_LEN: usize = HEADER_LEN + 8;
pub const ENTRY_LEN: usize = 16;

/// A rectangle as the device counts: unsigned, from the top left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn put_rect(out: &mut [u8], at: usize, rect: Rect) {
    put32(out, at, rect.x);
    put32(out, at + 4, rect.y);
    put32(out, at + 8, rect.width);
    put32(out, at + 12, rect.height);
}

/// Start a request: type, then zeroes. No fences, no 3D context.
fn header(out: &mut [u8], kind: u32, len: usize) -> usize {
    out[..len].fill(0);
    put32(out, 0, kind);
    len
}

/// Ask what the displays are. Answered with [`DISPLAY_INFO_LEN`] bytes.
pub fn get_display_info(out: &mut [u8]) -> usize {
    header(out, CMD_GET_DISPLAY_INFO, HEADER_LEN)
}

/// Make resource `id`: `width` by `height` pixels of `format`, held by the
/// host, with no backing yet.
pub fn resource_create_2d(out: &mut [u8], id: u32, format: u32, width: u32, height: u32) -> usize {
    let len = header(out, CMD_RESOURCE_CREATE_2D, CREATE_2D_LEN);
    put32(out, 24, id);
    put32(out, 28, format);
    put32(out, 32, width);
    put32(out, 36, height);
    len
}

/// Give resource `id` its backing: `entries` runs of guest memory, which
/// follow this header in the request as [`backing_entry`]s.
pub fn resource_attach_backing(out: &mut [u8], id: u32, entries: u32) -> usize {
    let len = header(out, CMD_RESOURCE_ATTACH_BACKING, ATTACH_LEN);
    put32(out, 24, id);
    put32(out, 28, entries);
    len
}

/// One run of backing memory: a physical address and a length in bytes.
pub fn backing_entry(out: &mut [u8], address: u64, len: u32) -> usize {
    out[..ENTRY_LEN].fill(0);
    put64(out, 0, address);
    put32(out, 8, len);
    ENTRY_LEN
}

/// Show `rect` of resource `id` on `scanout`.
pub fn set_scanout(out: &mut [u8], scanout: u32, id: u32, rect: Rect) -> usize {
    let len = header(out, CMD_SET_SCANOUT, SET_SCANOUT_LEN);
    put_rect(out, 24, rect);
    put32(out, 40, scanout);
    put32(out, 44, id);
    len
}

/// Copy `rect` of resource `id` from its backing to the host. `offset` is
/// where the rectangle's first pixel is in the backing.
pub fn transfer_to_host_2d(out: &mut [u8], id: u32, rect: Rect, offset: u64) -> usize {
    let len = header(out, CMD_TRANSFER_TO_HOST_2D, TRANSFER_LEN);
    put_rect(out, 24, rect);
    put64(out, 40, offset);
    put32(out, 48, id);
    len
}

/// Say `rect` of resource `id` has changed, so whatever shows it updates.
pub fn resource_flush(out: &mut [u8], id: u32, rect: Rect) -> usize {
    let len = header(out, CMD_RESOURCE_FLUSH, FLUSH_LEN);
    put_rect(out, 24, rect);
    put32(out, 40, id);
    len
}

/// The type of a response: [`RESP_OK_NODATA`], [`RESP_OK_DISPLAY_INFO`],
/// or an error. A response too short to have a header is reported as the
/// unspecified error, which is what it is.
pub fn response_type(bytes: &[u8]) -> u32 {
    if bytes.len() < HEADER_LEN {
        return RESP_ERR_UNSPEC;
    }
    get32(bytes, 0)
}

/// One display, as `GET_DISPLAY_INFO` describes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Display {
    pub rect: Rect,
    pub enabled: bool,
}

/// Read display `index` out of a display-info response.
pub fn display(bytes: &[u8], index: usize) -> Option<Display> {
    if index >= MAX_SCANOUTS || bytes.len() < DISPLAY_INFO_LEN {
        return None;
    }
    if response_type(bytes) != RESP_OK_DISPLAY_INFO {
        return None;
    }
    let at = HEADER_LEN + 24 * index;
    Some(Display {
        rect: Rect {
            x: get32(bytes, at),
            y: get32(bytes, at + 4),
            width: get32(bytes, at + 8),
            height: get32(bytes, at + 12),
        },
        enabled: get32(bytes, at + 16) != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Offsets below are those of the structures in Linux's
    // include/uapi/linux/virtio_gpu.h, written out by hand.

    #[test]
    fn create_2d_is_forty_bytes_in_the_right_order() {
        let mut out = [0xffu8; 64];
        assert_eq!(resource_create_2d(&mut out, 1, 2, 1280, 800), 40);
        assert_eq!(&out[0..4], &0x0101u32.to_le_bytes());
        // flags, fence_id, ctx_id, ring_idx and padding all zero.
        assert!(out[4..24].iter().all(|b| *b == 0));
        assert_eq!(get32(&out, 24), 1);
        assert_eq!(get32(&out, 28), 2);
        assert_eq!(get32(&out, 32), 1280);
        assert_eq!(get32(&out, 36), 800);
    }

    #[test]
    fn transfer_puts_the_offset_before_the_resource() {
        let mut out = [0u8; 64];
        let rect = Rect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        assert_eq!(transfer_to_host_2d(&mut out, 7, rect, 0x1_0000_0004), 56);
        assert_eq!(get32(&out, 0), CMD_TRANSFER_TO_HOST_2D);
        assert_eq!(
            [
                get32(&out, 24),
                get32(&out, 28),
                get32(&out, 32),
                get32(&out, 36)
            ],
            [10, 20, 30, 40]
        );
        assert_eq!(&out[40..48], &0x1_0000_0004u64.to_le_bytes());
        assert_eq!(get32(&out, 48), 7);
        assert_eq!(get32(&out, 52), 0);
    }

    #[test]
    fn scanout_and_flush_share_a_shape_but_not_an_order() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        let mut scanout = [0u8; 48];
        let mut flush = [0u8; 48];
        assert_eq!(set_scanout(&mut scanout, 0, 1, rect), 48);
        assert_eq!(resource_flush(&mut flush, 1, rect), 48);
        // set_scanout: rect, scanout_id, resource_id.
        assert_eq!((get32(&scanout, 40), get32(&scanout, 44)), (0, 1));
        // resource_flush: rect, resource_id, padding.
        assert_eq!((get32(&flush, 40), get32(&flush, 44)), (1, 0));
    }

    #[test]
    fn backing_is_a_header_then_entries() {
        let mut out = [0u8; 32 + 16];
        let len = resource_attach_backing(&mut out, 1, 1);
        assert_eq!(len, 32);
        assert_eq!((get32(&out, 24), get32(&out, 28)), (1, 1));
        assert_eq!(backing_entry(&mut out[32..], 0xdead_b000, 4096), 16);
        assert_eq!(&out[32..40], &0xdead_b000u64.to_le_bytes());
        assert_eq!(get32(&out, 40), 4096);
        assert_eq!(get32(&out, 44), 0);
    }

    #[test]
    fn display_info_is_read_by_index() {
        let mut response = vec![0u8; DISPLAY_INFO_LEN];
        put32(&mut response, 0, RESP_OK_DISPLAY_INFO);
        let at = HEADER_LEN + 24;
        put_rect(
            &mut response,
            at,
            Rect {
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
            },
        );
        put32(&mut response, at + 16, 1);

        assert_eq!(display(&response, 0).map(|d| d.enabled), Some(false));
        let second = display(&response, 1).unwrap();
        assert!(second.enabled);
        assert_eq!((second.rect.width, second.rect.height), (1024, 768));
        assert_eq!(display(&response, MAX_SCANOUTS), None);
        assert_eq!(display(&response[..100], 0), None);

        put32(&mut response, 0, RESP_ERR_UNSPEC);
        assert_eq!(display(&response, 1), None);
    }

    #[test]
    fn a_short_response_is_an_error() {
        assert_eq!(response_type(&[0; 10]), RESP_ERR_UNSPEC);
        let mut ok = [0u8; 24];
        put32(&mut ok, 0, RESP_OK_NODATA);
        assert_eq!(response_type(&ok), RESP_OK_NODATA);
    }
}
