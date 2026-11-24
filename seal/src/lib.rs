//! Sealing: AES-CCM, as the secure core does it.
//!
//! CCM (NIST SP 800-38C, RFC 3610) is counter mode for secrecy and CBC-MAC
//! for integrity, both under one key. Here the nonce is 12 bytes, the tag 16,
//! and there is no associated data. Nothing in this crate holds a key: every
//! function takes the block cipher as a closure, and the secure core's runs
//! AES-NI with a key that lives in a register. So this is the part that can
//! be tested on the build host, and the part that cannot is a few lines of
//! `asm!` in the kernel.
//!
//! Why CCM and not counter mode plus a separate MAC: it is a standard
//! authenticated mode with one key, so the secure core derives one key per
//! holder, not two; and its MAC needs no subkeys of its own, which CMAC's do
//! -- those would be key material in memory, where nothing else here is.

#![no_std]

/// Bytes of nonce at the front of a sealed message.
pub const NONCE: usize = 12;
/// Bytes of tag after it.
pub const TAG: usize = 16;
/// What sealing adds: the nonce and the tag.
pub const OVERHEAD: usize = NONCE + TAG;

/// Bytes CCM can count with the 3-byte length a 12-byte nonce leaves.
pub const LIMIT: usize = 1 << 24;

/// `B0`'s flags: no associated data, a 16-byte tag, a 3-byte length.
const FLAGS_MAC: u8 = (((TAG - 2) / 2) << 3) as u8 | (15 - NONCE - 1) as u8;
/// A counter block's flags: the 3-byte length again.
const FLAGS_CTR: u8 = (15 - NONCE - 1) as u8;

/// Encrypt `data` in place and return its tag. `encrypt` is one AES block
/// under the key. A nonce must never be used twice with one key.
///
/// # Panics
/// If `data` is [`LIMIT`] bytes or more.
pub fn seal(encrypt: &mut impl FnMut(&mut [u8; 16]), nonce: &[u8; NONCE], data: &mut [u8]) -> [u8; TAG] {
    assert!(data.len() < LIMIT);
    let mut tag = mac(encrypt, nonce, data);
    xor(&mut tag, &keystream(encrypt, nonce, 0));
    counter(encrypt, nonce, data);
    tag
}

/// Decrypt `data` in place if `tag` is right for it, and say whether it was.
/// When it is not, `data` comes back zeroed: what a forged message decrypts to
/// is never handed out.
///
/// # Panics
/// If `data` is [`LIMIT`] bytes or more.
pub fn open(
    encrypt: &mut impl FnMut(&mut [u8; 16]),
    nonce: &[u8; NONCE],
    data: &mut [u8],
    tag: &[u8; TAG],
) -> bool {
    assert!(data.len() < LIMIT);
    counter(encrypt, nonce, data);
    let mut expected = mac(encrypt, nonce, data);
    xor(&mut expected, &keystream(encrypt, nonce, 0));
    // Every byte compared whatever the first difference, so the time taken
    // says nothing about how much of a guessed tag was right.
    let difference = expected.iter().zip(tag).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if difference != 0 {
        data.fill(0);
        return false;
    }
    true
}

/// CBC-MAC over `B0` and then `data`, its last block padded with zeros.
fn mac(encrypt: &mut impl FnMut(&mut [u8; 16]), nonce: &[u8; NONCE], data: &[u8]) -> [u8; 16] {
    let mut state = [0u8; 16];
    state[0] = FLAGS_MAC;
    state[1..1 + NONCE].copy_from_slice(nonce);
    state[13..].copy_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    encrypt(&mut state);
    for chunk in data.chunks(16) {
        xor(&mut state, chunk);
        encrypt(&mut state);
    }
    state
}

/// XOR `data` with the keystream from counter 1 on.
fn counter(encrypt: &mut impl FnMut(&mut [u8; 16]), nonce: &[u8; NONCE], data: &mut [u8]) {
    for (index, chunk) in (1u32..).zip(data.chunks_mut(16)) {
        let block = keystream(encrypt, nonce, index);
        for (byte, key) in chunk.iter_mut().zip(block) {
            *byte ^= key;
        }
    }
}

/// Counter block `index`, encrypted.
fn keystream(encrypt: &mut impl FnMut(&mut [u8; 16]), nonce: &[u8; NONCE], index: u32) -> [u8; 16] {
    let mut block = [0u8; 16];
    block[0] = FLAGS_CTR;
    block[1..1 + NONCE].copy_from_slice(nonce);
    block[13..].copy_from_slice(&index.to_be_bytes()[1..]);
    encrypt(&mut block);
    block
}

fn xor(into: &mut [u8; 16], from: &[u8]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a ^= b;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    /// AES-128 on the build host, by AES-NI. The answers below came from
    /// Python's `cryptography` (`AESCCM(key, tag_length=16)`), a separate
    /// implementation, so the two agreeing is the test.
    fn aes(key: [u8; 16]) -> impl FnMut(&mut [u8; 16]) {
        assert!(std::is_x86_feature_detected!("aes"), "the build host needs AES-NI");
        move |block| {
            // SAFETY: AES-NI is present, checked above.
            unsafe { aes_block(&key, block) }
        }
    }

    #[target_feature(enable = "aes,sse2")]
    unsafe fn aes_block(key: &[u8; 16], block: &mut [u8; 16]) {
        use core::arch::x86_64::*;
        unsafe fn next<const RCON: i32>(key: __m128i) -> __m128i {
            unsafe {
                let assist = _mm_shuffle_epi32(_mm_aeskeygenassist_si128(key, RCON), 0xff);
                let mut key = key;
                key = _mm_xor_si128(key, _mm_slli_si128(key, 4));
                key = _mm_xor_si128(key, _mm_slli_si128(key, 4));
                key = _mm_xor_si128(key, _mm_slli_si128(key, 4));
                _mm_xor_si128(key, assist)
            }
        }
        unsafe {
            let mut round = _mm_loadu_si128(key.as_ptr().cast());
            let mut state = _mm_xor_si128(_mm_loadu_si128(block.as_ptr().cast()), round);
            macro_rules! step {
                ($($rcon:literal),*) => {$(
                    round = next::<$rcon>(round);
                    state = _mm_aesenc_si128(state, round);
                )*};
            }
            step!(0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b);
            round = next::<0x36>(round);
            state = _mm_aesenclast_si128(state, round);
            _mm_storeu_si128(block.as_mut_ptr().cast(), state);
        }
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn tag(text: &str) -> [u8; TAG] {
        hex(text).try_into().unwrap()
    }

    const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const NONCE_BYTES: [u8; NONCE] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
    ];

    fn message(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn aes_is_fips_197() {
        let mut block = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        aes(KEY)(&mut block);
        assert_eq!(block.to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
    }

    #[test]
    fn seal_matches_another_implementation() {
        let cases = [
            (0, "", "cc9b10e8593f83fb891535f9706e8492"),
            (1, "20", "58a5dfb33ac0121e03a4f4dac97935b5"),
            (15, "20bfa8b85dd2008631ee3690233abe", "0486eaaec0f26f5abffaeee039b12af3"),
            (16, "20bfa8b85dd2008631ee3690233abeea", "b26e87f6ad61a32bc961dc207ff07dc2"),
            (17, "20bfa8b85dd2008631ee3690233abeea08", "c172dd0a7dda658d7debfcb7df7873a7"),
            (
                33,
                "20bfa8b85dd2008631ee3690233abeea08cd533fbad1978f5d5a1e9014ebb858be",
                "00effafec1f93c5cae4752e2a8895853",
            ),
            (
                100,
                "20bfa8b85dd2008631ee3690233abeea08cd533fbad1978f5d5a1e9014ebb858bef95d921c9a32a701c66bcec800fd8cce95325847ae7c871657ca034d41b98cb1457b80d85678eb7665087f27fe5be3183fade2a9100fc1a7486ba45ed1222b8b7ae1f9",
                "65ae71a70537632e952aca574e94f12d",
            ),
        ];
        for (len, cipher, expected) in cases {
            let mut data = message(len);
            let sealed = seal(&mut aes(KEY), &NONCE_BYTES, &mut data);
            assert_eq!(data, hex(cipher), "{len} bytes");
            assert_eq!(sealed, tag(expected), "{len} bytes");
            assert!(open(&mut aes(KEY), &NONCE_BYTES, &mut data, &sealed));
            assert_eq!(data, message(len));
        }
    }

    #[test]
    fn the_largest_message_the_mailbox_takes() {
        let mut data = message(4004);
        let sealed = seal(&mut aes(KEY), &NONCE_BYTES, &mut data);
        assert_eq!(sealed, tag("b4e4b0d832dbf5cdc80a3e1c0f81d30a"));
        assert_eq!(data[2048..2056], hex("116ee39d40445015")[..]);
        assert_eq!(data[3996..], hex("b71b9ba0205a2966")[..]);
        assert!(open(&mut aes(KEY), &NONCE_BYTES, &mut data, &sealed));
        assert_eq!(data, message(4004));
    }

    #[test]
    fn any_change_is_refused_and_nothing_comes_out() {
        let original = message(40);
        let mut sealed = original.clone();
        let good = seal(&mut aes(KEY), &NONCE_BYTES, &mut sealed);

        for bit in 0..TAG * 8 {
            let mut data = sealed.clone();
            let mut bad = good;
            bad[bit / 8] ^= 1 << (bit % 8);
            assert!(!open(&mut aes(KEY), &NONCE_BYTES, &mut data, &bad));
            assert!(data.iter().all(|&b| b == 0));
        }
        for bit in 0..sealed.len() * 8 {
            let mut data = sealed.clone();
            data[bit / 8] ^= 1 << (bit % 8);
            assert!(!open(&mut aes(KEY), &NONCE_BYTES, &mut data, &good));
            assert!(data.iter().all(|&b| b == 0));
        }
        let mut other = NONCE_BYTES;
        other[11] ^= 1;
        let mut data = sealed.clone();
        assert!(!open(&mut aes(KEY), &other, &mut data, &good));
        let mut key = KEY;
        key[0] ^= 1;
        let mut data = sealed.clone();
        assert!(!open(&mut aes(key), &NONCE_BYTES, &mut data, &good));
        // A message cut short is a different length, which B0 binds.
        let mut data = sealed[..39].to_vec();
        assert!(!open(&mut aes(KEY), &NONCE_BYTES, &mut data, &good));
        let mut data = sealed.clone();
        assert!(open(&mut aes(KEY), &NONCE_BYTES, &mut data, &good));
        assert_eq!(data, original);
    }
}
