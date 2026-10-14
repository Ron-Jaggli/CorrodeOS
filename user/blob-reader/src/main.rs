//! Read a large file the kernel was handed at boot, and say what was in it.
//!
//! This is the shape of a GPU driver's first step -- map the firmware it was
//! given, and check it is the firmware it expects -- without the GPU. It
//! holds three capabilities: the blob, read-only, and endpoints to report on
//! and log to.

#![no_std]
#![no_main]

use corrode_rt::abi::blob::{
    BLOB, FNV_OFFSET, FNV_PRIME, LABEL_PHYSICAL, LABEL_READ, LOG, RECORD, RECORDS, REPORT,
};
use corrode_rt::{map, physical, println, send, status, Message};

corrode_rt::entry!(main);

/// Record `n` as the build wrote it: fifteen digits and a newline.
fn record(n: usize) -> [u8; RECORD] {
    let mut out = [b'0'; RECORD];
    out[RECORD - 1] = b'\n';
    let mut n = n;
    for digit in out[..RECORD - 1].iter_mut().rev() {
        *digit = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out
}

fn main() {
    corrode_rt::log::set(LOG);

    let mapped = map(BLOB);
    let (len, hash, ends) = match mapped {
        Ok(mapping) => {
            // SAFETY: the kernel mapped `len` bytes here, readable, for the
            // life of this task, and nobody writes them: a blob is read-only
            // to every holder.
            let bytes = unsafe { core::slice::from_raw_parts(mapping.address, mapping.len) };
            let mut hash = FNV_OFFSET;
            for byte in bytes {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            let ends = bytes.len() == RECORD * RECORDS
                && bytes[..RECORD] == record(0)
                && bytes[bytes.len() - RECORD..] == record(RECORDS - 1);
            (mapping.len, hash, ends)
        }
        Err(error) => {
            println!("could not map the blob: {:?}", error);
            (0, 0, false)
        }
    };
    let _ = send(
        REPORT,
        Message::new(LABEL_READ, [status(&mapped), len as u64, hash, ends as u64]),
    );

    // A device could be told this address, and then write it.
    let asked = physical(BLOB, 0);
    let _ = send(
        REPORT,
        Message::new(LABEL_PHYSICAL, [status(&asked), 0, 0, 0]),
    );

    println!("{} MiB read, hash {:#018x}", len >> 20, hash);
}
