//! The lock screen's passcode, kept as a salted, deliberately slow hash.
//!
//! desktop.conf can say `passcode = 2468`, in the clear, or
//! `passcode = pbkdf2-sha256$<iterations>$<salt>$<hash>`, salt and hash in
//! hex. `./corrode passcode-hash` turns the first into the second. The
//! kernel hashes a plain one as soon as it reads it, so either way it only
//! ever keeps a hash.
//!
//! PBKDF2 with HMAC-SHA-256 (RFC 8018), written here because Corrode takes
//! nothing from crates.io. A passcode of 4 to 8 digits has at most 10^8
//! values, so no hash makes one safe from someone who has the file: the
//! iterations make each guess cost something, and the salt stops one table
//! of guesses serving every machine. The lock screen's limit on tries is
//! what protects it from someone at the keyboard.

use core::fmt;

/// What `./corrode passcode-hash` uses: a few tens of milliseconds a try in
/// the kernel under QEMU.
pub const ITERATIONS: u32 = 50_000;
/// Fewer is too cheap to be worth calling slow; more would hold the desktop
/// up for seconds on every try.
pub const MIN_ITERATIONS: u32 = 1_000;
pub const MAX_ITERATIONS: u32 = 1_000_000;

const PREFIX: &str = "pbkdf2-sha256$";

/// A passcode as desktop.conf gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// 4 to 8 ASCII digits, NUL-padded: an old config, or a hand-written one.
    Plain([u8; 8]),
    Hashed(Hashed),
}

/// A salted PBKDF2-HMAC-SHA-256 of the digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hashed {
    pub iterations: u32,
    pub salt: [u8; 16],
    pub hash: [u8; 32],
}

impl Hashed {
    /// Hash `code`, digits NUL-padded as the lock screen sends them.
    pub fn new(code: [u8; 8], salt: [u8; 16], iterations: u32) -> Self {
        Self { iterations, salt, hash: pbkdf2(digits(&code), &salt, iterations) }
    }

    /// Is `code` the passcode? Every byte of the hash is compared, whatever
    /// the first difference.
    pub fn matches(&self, code: [u8; 8]) -> bool {
        let tried = pbkdf2(digits(&code), &self.salt, self.iterations);
        tried.iter().zip(self.hash).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    }

    /// From the part of a `passcode =` line after the `=`.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.strip_prefix(PREFIX)?.split('$');
        let iterations: u32 = parts.next()?.parse().ok()?;
        let salt = unhex(parts.next()?)?;
        let hash = unhex(parts.next()?)?;
        if parts.next().is_some() || !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return None;
        }
        Some(Self { iterations, salt, hash })
    }
}

/// The value of a `passcode =` line: `pbkdf2-sha256$50000$<salt>$<hash>`.
impl fmt::Display for Hashed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}$", self.iterations)?;
        self.salt.iter().try_for_each(|byte| write!(f, "{byte:02x}"))?;
        f.write_str("$")?;
        self.hash.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The digits of a NUL-padded code.
fn digits(code: &[u8; 8]) -> &[u8] {
    let len = code.iter().position(|&byte| byte == 0).unwrap_or(code.len());
    &code[..len]
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * at..2 * at + 2], 16).ok()?;
    }
    Some(out)
}

/// The first 32 bytes of PBKDF2-HMAC-SHA-256: one block, so one chain.
pub fn pbkdf2(password: &[u8], salt: &[u8; 16], iterations: u32) -> [u8; 32] {
    let key = HmacKey::new(password);
    let mut first = [0u8; 20];
    first[..16].copy_from_slice(salt);
    first[16..].copy_from_slice(&1u32.to_be_bytes());
    let mut u = key.mac(&first);
    let mut out = u;
    for _ in 1..iterations {
        u = key.mac(&u);
        for (o, b) in out.iter_mut().zip(u) {
            *o ^= b;
        }
    }
    out
}

/// HMAC-SHA-256 with its padded key's two states worked out once: each
/// PBKDF2 round is then two compressions on top of them, not four.
struct HmacKey {
    inner: Sha256,
    outer: Sha256,
}

impl HmacKey {
    fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 64];
        if key.len() > 64 {
            block[..32].copy_from_slice(&sha256(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha256::new();
        let mut outer = Sha256::new();
        inner.update(&block.map(|byte| byte ^ 0x36));
        outer.update(&block.map(|byte| byte ^ 0x5c));
        Self { inner, outer }
    }

    fn mac(&self, message: &[u8]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        inner.update(message);
        let mut outer = self.outer.clone();
        outer.update(&inner.finish());
        outer.finish()
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish()
}

/// FIPS 180-4's SHA-256.
#[derive(Clone)]
struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    length: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            block: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                self.compress();
                self.filled = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.length * 8;
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *chunk = word.to_be_bytes();
        }
        out
    }

    fn compress(&mut self) {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(self.block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choose).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha256_matches_fips_180() {
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // A million a's, fed in uneven pieces.
        let mut hash = Sha256::new();
        let piece = [b'a'; 997];
        let mut left = 1_000_000;
        while left > 0 {
            let take = left.min(piece.len());
            hash.update(&piece[..take]);
            left -= take;
        }
        assert_eq!(hex(&hash.finish()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let mac = HmacKey::new(b"Jefe").mac(b"what do ya want for nothing?");
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // A key longer than a block is hashed first.
        let mac = HmacKey::new(&[0xaa; 131]).mac(b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(hex(&mac), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn pbkdf2_matches_rfc_7914() {
        // RFC 7914, section 11: the first 32 of its 64 bytes.
        let salt = |text: &[u8]| {
            let mut salt = [0u8; 16];
            salt[..text.len()].copy_from_slice(text);
            salt
        };
        // Its salts are shorter than ours, so these go through HMAC by hand.
        let key = HmacKey::new(b"passwd");
        assert_eq!(
            hex(&key.mac(b"salt\0\0\0\x01")),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
        let password = HmacKey::new(b"Password");
        let mut u = password.mac(b"NaCl\0\0\0\x01");
        let mut out = u;
        for _ in 1..80_000 {
            u = password.mac(&u);
            out.iter_mut().zip(u).for_each(|(o, b)| *o ^= b);
        }
        assert_eq!(hex(&out), "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56");
        // And the same loop as `pbkdf2`, on a full-length salt.
        let full = salt(b"0123456789abcdef");
        assert_eq!(pbkdf2(b"2468", &full, 1), HmacKey::new(b"2468").mac(b"0123456789abcdef\0\0\0\x01"));
    }

    #[test]
    fn the_right_code_matches_and_no_other() {
        let salt = *b"a salt, sixteen!";
        let stored = Hashed::new(*b"2468\0\0\0\0", salt, 1_000);
        assert!(stored.matches(*b"2468\0\0\0\0"));
        for wrong in [*b"2469\0\0\0\0", *b"24680\0\0\0", *b"246\0\0\0\0\0", [0; 8], *b"86422468"] {
            assert!(!stored.matches(wrong), "{wrong:?}");
        }
        // Another salt, another hash for the same digits.
        assert_ne!(Hashed::new(*b"2468\0\0\0\0", *b"another salt!!!!", 1_000).hash, stored.hash);
    }

    #[test]
    fn a_hashed_line_reads_back_as_written() {
        let stored = Hashed::new(*b"87654321", [7; 16], 2_000);
        let line = stored.to_string();
        assert!(line.starts_with("pbkdf2-sha256$2000$0707"), "{line}");
        assert_eq!(Hashed::parse(&line), Some(stored));
        for bad in [
            "pbkdf2-sha256$2000$0707",
            "pbkdf2-sha256$999$07070707070707070707070707070707$".to_string().as_str(),
            "sha1$2000$07070707070707070707070707070707$00",
            &line.replace("pbkdf2-sha256$2000", "pbkdf2-sha256$2000000"),
            &format!("{line}$extra"),
            &line[..line.len() - 1],
            &line.replace('0', "g"),
        ] {
            assert_eq!(Hashed::parse(bad), None, "{bad:?}");
        }
    }
}
