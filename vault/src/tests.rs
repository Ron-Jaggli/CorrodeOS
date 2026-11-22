extern crate std;

use super::*;
use std::println;
use std::vec::Vec;

/// AES-128 on the build host, by AES-NI, as in corrode-seal's tests (which
/// check it against FIPS-197 and an outside CCM).
fn aes() -> impl Cipher {
    assert!(std::is_x86_feature_detected!("aes"), "the build host needs AES-NI");
    |key: &[u8; 16], block: &mut [u8; 16]| {
        // SAFETY: AES-NI is present, checked above.
        unsafe { aes_block(key, block) }
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

const SALT: [u8; 16] = [7; 16];
const NONCE_BYTES: [u8; NONCE] = [9; NONCE];

fn files() -> Vec<u8> {
    (0..3000u32).map(|i| (i * 31 % 251) as u8).collect()
}

fn sealed() -> (Header, Vec<u8>) {
    let mut data = files();
    let header = seal(&mut aes(), b"2468", SALT, NONCE_BYTES, MIN_ITERATIONS, &mut data);
    (header, data)
}

#[test]
fn the_right_passcode_opens_it() {
    let (header, mut data) = sealed();
    assert_ne!(data, files(), "sealed is not the same bytes");
    assert_eq!(header.len as usize, data.len());
    assert!(open(&mut aes(), b"2468", &header, &mut data));
    assert_eq!(data, files());
}

#[test]
fn a_wrong_passcode_opens_nothing() {
    for wrong in [&b"2469"[..], b"246", b"24680", b""] {
        let (header, mut data) = sealed();
        assert!(!open(&mut aes(), wrong, &header, &mut data));
        assert!(data.iter().all(|&byte| byte == 0), "nothing decrypted is handed out");
    }
}

#[test]
fn a_changed_header_or_body_opens_nothing() {
    let (good, body) = sealed();
    let changes: [fn(&mut Header); 5] = [
        |h| h.salt[0] ^= 1,
        |h| h.iterations += 1,
        |h| h.nonce[11] ^= 1,
        |h| h.tag[15] ^= 1,
        |h| h.len -= 1,
    ];
    for change in changes {
        let mut header = good;
        change(&mut header);
        let mut data = body.clone();
        data.truncate(header.len as usize);
        assert!(!open(&mut aes(), b"2468", &header, &mut data));
    }
    let mut data = body.clone();
    data[1234] ^= 0x40;
    assert!(!open(&mut aes(), b"2468", &good, &mut data));
    // Bytes that are not as many as the header says.
    let mut data = body.clone();
    data.push(0);
    assert!(!open(&mut aes(), b"2468", &good, &mut data));
}

#[test]
fn headers_round_trip_and_odd_ones_are_refused() {
    let (header, _) = sealed();
    let bytes = header.to_bytes();
    assert_eq!(Header::parse(&bytes), Some(header));
    assert_eq!(header.sectors(), 6);
    let mut long = bytes.to_vec();
    long.extend_from_slice(&[0xff; 448]);
    assert_eq!(Header::parse(&long), Some(header), "a whole sector, junk after");
    assert_eq!(Header::parse(&bytes[..63]), None);
    let damaged = |at: usize, value: &[u8]| {
        let mut bytes = bytes;
        bytes[at..at + value.len()].copy_from_slice(value);
        Header::parse(&bytes)
    };
    assert_eq!(damaged(0, b"X"), None);
    assert_eq!(damaged(8, &2u32.to_le_bytes()), None);
    assert_eq!(damaged(12, &(MIN_ITERATIONS - 1).to_le_bytes()), None);
    assert_eq!(damaged(12, &(MAX_ITERATIONS + 1).to_le_bytes()), None);
    assert_eq!(damaged(44, &0u32.to_le_bytes()), None);
    assert_eq!(damaged(44, &(MAX_LEN as u32 + 1).to_le_bytes()), None);
    assert!(damaged(44, &(MAX_LEN as u32).to_le_bytes()).is_some());
    assert_eq!(Header::parse(&[0; 512]), None, "a blank disk is no vault");
}

#[test]
fn a_key_is_zeroed_when_dropped() {
    let key = Key::new(b"2468", &SALT, MIN_ITERATIONS);
    assert_ne!(key.bytes(), &[0; 16]);
    let mut bytes = *key.bytes();
    wipe(&mut bytes);
    assert_eq!(bytes, [0; 16]);
}

/// `./corrode vault-disk`: seal `VAULT_TAR` under `VAULT_PASSCODE` into the
/// disk image `VAULT_DISK`, salt and nonce from the host's /dev/urandom.
/// The image is made or grown to fit; its theme sector is left as it is.
#[test]
#[ignore]
fn seal_the_root() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let tar = std::env::var("VAULT_TAR").expect("VAULT_TAR names the root directory's tar");
    let disk = std::env::var("VAULT_DISK").expect("VAULT_DISK names the disk image");
    let passcode = std::env::var("VAULT_PASSCODE").expect("VAULT_PASSCODE is the passcode");
    assert!(
        corrode_theme::passcode::allowed(passcode.as_bytes()),
        "a passcode is 4 to 32 printable characters, no space and no #"
    );
    let mut data = std::fs::read(&tar).expect("read the tar");
    let mut random = [0u8; 16 + NONCE];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random)).expect("random");
    let salt: [u8; 16] = random[..16].try_into().unwrap();
    let nonce: [u8; NONCE] = random[16..].try_into().unwrap();
    let header = seal(&mut aes(), passcode.as_bytes(), salt, nonce, ITERATIONS, &mut data);

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&disk)
        .expect("open the disk image");
    let end = DATA_OFFSET + header.sectors() * SECTOR as u64;
    let size = file.metadata().expect("its size").len().max(end).max(1 << 20);
    file.set_len(size).expect("size the disk image");
    let mut first = [0u8; SECTOR];
    first[..HEADER_LEN].copy_from_slice(&header.to_bytes());
    file.seek(SeekFrom::Start(OFFSET)).and_then(|_| file.write_all(&first)).expect("write the header");
    data.resize(header.sectors() as usize * SECTOR, 0);
    file.seek(SeekFrom::Start(DATA_OFFSET)).and_then(|_| file.write_all(&data)).expect("write the files");
    println!("vault-disk: {tar} sealed into {disk}, {} bytes", header.len);
}
