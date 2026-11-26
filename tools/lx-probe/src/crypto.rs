//! Pure-Rust cryptographic primitives used by LX scripts and catalog search adapters.
//!
//! Replaces OpenSSL (`libssl`/`libcrypto`) for MD5, SHA-1, AES-128/192/256 ECB/CBC
//! (with PKCS#7 padding), 1024-bit raw RSA public-key encryption, and OS CSPRNG bytes.

use std::cmp::Ordering;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const AES_RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// PKCS#1 RSA encryption OID `1.2.840.113549.1.1.1`.
const RSA_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];

/// Compute RFC 1321 MD5 digest.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        md5_compress(&mut state, block);
    }

    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let total_blocks = if rem.len() < 56 {
        1
    } else {
        2
    };
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let len_offset = total_blocks * 64 - 8;
    tail[len_offset..len_offset + 8].copy_from_slice(&bit_len.to_le_bytes());

    let (tail_blocks, _) = tail.as_chunks::<64>();
    md5_compress(&mut state, &tail_blocks[0]);
    if total_blocks == 2 {
        md5_compress(&mut state, &tail_blocks[1]);
    }

    let mut out = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// Compute lowercase 32-character hexadecimal MD5 digest.
pub fn md5_hex(data: &[u8]) -> String {
    let digest = md5(data);
    let mut out = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn md5_compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut m = [0u32; 16];
    for (dst, &chunk) in m.iter_mut().zip(block.as_chunks::<4>().0.iter()) {
        *dst = u32::from_le_bytes(chunk);
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | (!b & d), i),
            16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let sum = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(MD5_SHIFTS[i]));
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Compute FIPS 180-4 SHA-1 digest.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        sha1_compress(&mut state, block);
    }

    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let total_blocks = if rem.len() < 56 {
        1
    } else {
        2
    };
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let len_offset = total_blocks * 64 - 8;
    tail[len_offset..len_offset + 8].copy_from_slice(&bit_len.to_be_bytes());

    let (tail_blocks, _) = tail.as_chunks::<64>();
    sha1_compress(&mut state, &tail_blocks[0]);
    if total_blocks == 2 {
        sha1_compress(&mut state, &tail_blocks[1]);
    }

    let mut out = [0u8; 20];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn sha1_compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (dst, &chunk) in w[..16].iter_mut().zip(block.as_chunks::<4>().0.iter()) {
        *dst = u32::from_be_bytes(chunk);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (i, &word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5a82_7999u32),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1u32),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdcu32),
            _ => (b ^ c ^ d, 0xca62_c1d6u32),
        };
        let temp =
            a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

/// Supported AES encryption modes (all use PKCS#7 padding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AesMode {
    Aes128Cbc,
    Aes192Cbc,
    Aes256Cbc,
    Aes128Ecb,
    Aes192Ecb,
    Aes256Ecb,
}

impl AesMode {
    pub fn parse(mode: &str) -> Result<Self, &'static str> {
        if mode.eq_ignore_ascii_case("aes-128-cbc") {
            Ok(Self::Aes128Cbc)
        } else if mode.eq_ignore_ascii_case("aes-192-cbc") {
            Ok(Self::Aes192Cbc)
        } else if mode.eq_ignore_ascii_case("aes-256-cbc") {
            Ok(Self::Aes256Cbc)
        } else if mode.eq_ignore_ascii_case("aes-128-ecb") {
            Ok(Self::Aes128Ecb)
        } else if mode.eq_ignore_ascii_case("aes-192-ecb") {
            Ok(Self::Aes192Ecb)
        } else if mode.eq_ignore_ascii_case("aes-256-ecb") {
            Ok(Self::Aes256Ecb)
        } else {
            Err("unsupported AES mode")
        }
    }

    pub const fn key_len(self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes128Ecb => 16,
            Self::Aes192Cbc | Self::Aes192Ecb => 24,
            Self::Aes256Cbc | Self::Aes256Ecb => 32,
        }
    }

    pub const fn iv_len(self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes192Cbc | Self::Aes256Cbc => 16,
            Self::Aes128Ecb | Self::Aes192Ecb | Self::Aes256Ecb => 0,
        }
    }

    const fn rounds(self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes128Ecb => 10,
            Self::Aes192Cbc | Self::Aes192Ecb => 12,
            Self::Aes256Cbc | Self::Aes256Ecb => 14,
        }
    }
}

/// Encrypt `input` with AES-128/192/256 in ECB or CBC mode using PKCS#7 padding.
pub fn aes_encrypt(
    mode: AesMode,
    key: &[u8],
    iv: &[u8],
    input: &[u8],
) -> Result<Vec<u8>, &'static str> {
    if key.len() != mode.key_len() || iv.len() != mode.iv_len() {
        return Err("invalid AES key or IV length");
    }

    let nr = mode.rounds();
    let round_keys = aes_expand_key(key, nr);
    let pad = 16 - (input.len() % 16);
    let mut output = Vec::with_capacity(input.len() + pad);
    let mut prev = [0u8; 16];
    let is_cbc = mode.iv_len() == 16;
    if is_cbc {
        prev.copy_from_slice(iv);
    }

    let (blocks, rem) = input.as_chunks::<16>();
    for &chunk in blocks {
        let mut block = chunk;
        if is_cbc {
            xor_block(&mut block, &prev);
        }
        aes_encrypt_block(&mut block, &round_keys, nr);
        if is_cbc {
            prev = block;
        }
        output.extend_from_slice(&block);
    }

    let mut final_block = [pad as u8; 16];
    final_block[..rem.len()].copy_from_slice(rem);
    if is_cbc {
        xor_block(&mut final_block, &prev);
    }
    aes_encrypt_block(&mut final_block, &round_keys, nr);
    output.extend_from_slice(&final_block);

    Ok(output)
}

fn aes_expand_key(key: &[u8], nr: usize) -> [u32; 60] {
    let nk = key.len() / 4;
    let total_words = 4 * (nr + 1);
    let mut w = [0u32; 60];
    for (dst, &chunk) in w.iter_mut().zip(key.as_chunks::<4>().0.iter()) {
        *dst = u32::from_be_bytes(chunk);
    }
    for i in nk..total_words {
        let mut temp = w[i - 1];
        if i % nk == 0 {
            temp = sub_word(temp.rotate_left(8)) ^ (u32::from(AES_RCON[i / nk - 1]) << 24);
        } else if nk > 6 && i % nk == 4 {
            temp = sub_word(temp);
        }
        w[i] = w[i - nk] ^ temp;
    }
    w
}

#[inline]
fn sub_word(word: u32) -> u32 {
    let [b0, b1, b2, b3] = word.to_be_bytes();
    u32::from_be_bytes([
        AES_SBOX[usize::from(b0)],
        AES_SBOX[usize::from(b1)],
        AES_SBOX[usize::from(b2)],
        AES_SBOX[usize::from(b3)],
    ])
}

#[inline]
fn xtime(x: u8) -> u8 {
    (x << 1)
        ^ if x & 0x80 != 0 {
            0x1b
        } else {
            0
        }
}

#[inline]
fn xor_block(block: &mut [u8; 16], mask: &[u8; 16]) {
    for (dst, &src) in block.iter_mut().zip(mask.iter()) {
        *dst ^= src;
    }
}

fn add_round_key(state: &mut [u8; 16], round_keys: &[u32; 60], round: usize) {
    for col in 0..4 {
        let bytes = round_keys[round * 4 + col].to_be_bytes();
        let base = col * 4;
        state[base] ^= bytes[0];
        state[base + 1] ^= bytes[1];
        state[base + 2] ^= bytes[2];
        state[base + 3] ^= bytes[3];
    }
}

fn aes_encrypt_block(state: &mut [u8; 16], round_keys: &[u32; 60], nr: usize) {
    add_round_key(state, round_keys, 0);
    for round in 1..=nr {
        for byte in state.iter_mut() {
            *byte = AES_SBOX[usize::from(*byte)];
        }

        // ShiftRows (column-major state layout)
        let t1 = state[1];
        state[1] = state[5];
        state[5] = state[9];
        state[9] = state[13];
        state[13] = t1;

        state.swap(2, 10);
        state.swap(6, 14);

        let t3 = state[15];
        state[15] = state[11];
        state[11] = state[7];
        state[7] = state[3];
        state[3] = t3;

        if round < nr {
            for col in 0..4 {
                let i = col * 4;
                let [s0, s1, s2, s3] = [state[i], state[i + 1], state[i + 2], state[i + 3]];
                let t = s0 ^ s1 ^ s2 ^ s3;
                state[i] = s0 ^ t ^ xtime(s0 ^ s1);
                state[i + 1] = s1 ^ t ^ xtime(s1 ^ s2);
                state[i + 2] = s2 ^ t ^ xtime(s2 ^ s3);
                state[i + 3] = s3 ^ t ^ xtime(s3 ^ s0);
            }
        }

        add_round_key(state, round_keys, round);
    }
}

/// Fill a new buffer of length `len` using the OS CSPRNG.
pub fn random_bytes(len: usize) -> Result<Vec<u8>, &'static str> {
    let mut output = vec![0u8; len];
    if !output.is_empty() {
        getrandom::fill(&mut output).map_err(|_| "random byte generation failed")?;
    }
    Ok(output)
}

/// Raw (no-padding) RSA public-key encryption for 1024-bit keys in SPKI or PKCS#1 PEM format.
///
/// Matches OpenSSL `RSA_public_encrypt` with `Padding::NONE` on a 128-byte left-zero-padded block.
pub fn rsa_public_encrypt_nopad_1024(input: &[u8], pem: &[u8]) -> Result<Vec<u8>, &'static str> {
    let (n_bytes, e_bytes) = parse_rsa_public_key_pem(pem).ok_or("invalid RSA public key")?;
    if input.len() > 128 || n_bytes.len() != 128 {
        return Err("invalid RSA block or key size");
    }

    let n = U1024::from_be_bytes(&n_bytes);
    let m = U1024::from_be_bytes(input);
    if m.cmp(&n) != Ordering::Less {
        return Err("RSA encryption failed");
    }

    Ok(m.pow_mod(&e_bytes, &n).to_be_bytes().to_vec())
}

fn parse_rsa_public_key_pem(pem: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let text = std::str::from_utf8(pem).ok()?;
    if let Some(der) = decode_pem_block(text, "PUBLIC KEY") {
        return parse_spki_rsa(&der);
    }
    if let Some(der) = decode_pem_block(text, "RSA PUBLIC KEY") {
        return parse_pkcs1_rsa(&der);
    }
    None
}

fn decode_pem_block(text: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let rest = &text[start..];
    let finish = rest.find(&end)?;
    let b64: String = rest[..finish].chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if b64.is_empty() {
        return None;
    }
    BASE64.decode(b64).ok()
}

fn read_der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first_len, rest) = rest.split_first()?;
    let (len, rest) = if first_len < 0x80 {
        (usize::from(first_len), rest)
    } else {
        let count = usize::from(first_len & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let mut len = 0usize;
        for &byte in &rest[..count] {
            len = (len << 8) | usize::from(byte);
        }
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

fn read_der_positive_uint(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, mut value, rest) = read_der_tlv(input)?;
    if tag != 0x02 || value.is_empty() || (value[0] & 0x80) != 0 {
        return None;
    }
    while let Some((&0, tail)) = value.split_first() {
        value = tail;
    }
    if value.is_empty() {
        return None;
    }
    Some((value, rest))
}

fn parse_spki_rsa(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (tag, body, trailing) = read_der_tlv(der)?;
    if tag != 0x30 || !trailing.is_empty() {
        return None;
    }
    let (alg_tag, alg_body, rest) = read_der_tlv(body)?;
    if alg_tag != 0x30 {
        return None;
    }
    let (oid_tag, oid_val, alg_rest) = read_der_tlv(alg_body)?;
    if oid_tag != 0x06 || oid_val != RSA_OID {
        return None;
    }
    if !alg_rest.is_empty() {
        let (null_tag, null_val, alg_tail) = read_der_tlv(alg_rest)?;
        if null_tag != 0x05 || !null_val.is_empty() || !alg_tail.is_empty() {
            return None;
        }
    }

    let (bit_tag, bit_val, end) = read_der_tlv(rest)?;
    if bit_tag != 0x03 || !end.is_empty() {
        return None;
    }
    let (&unused_bits, key_der) = bit_val.split_first()?;
    if unused_bits != 0 {
        return None;
    }
    parse_pkcs1_rsa(key_der)
}

fn parse_pkcs1_rsa(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (tag, body, trailing) = read_der_tlv(der)?;
    if tag != 0x30 || !trailing.is_empty() {
        return None;
    }
    let (n, rest) = read_der_positive_uint(body)?;
    let (e, end) = read_der_positive_uint(rest)?;
    if !end.is_empty() {
        return None;
    }
    Some((n.to_vec(), e.to_vec()))
}

/// Fixed-width 1024-bit unsigned integer (little-endian 64-bit limbs) for modular exponentiation.
#[derive(Clone, Copy, PartialEq, Eq)]
struct U1024([u64; 16]);

impl U1024 {
    fn from_be_bytes(bytes: &[u8]) -> Self {
        let mut buf = [0u8; 128];
        buf[128 - bytes.len()..].copy_from_slice(bytes);
        let mut limbs = [0u64; 16];
        for (i, limb) in limbs.iter_mut().enumerate() {
            let offset = (15 - i) * 8;
            *limb = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
        }
        Self(limbs)
    }

    fn to_be_bytes(self) -> [u8; 128] {
        let mut buf = [0u8; 128];
        for (i, &limb) in self.0.iter().enumerate() {
            let offset = (15 - i) * 8;
            buf[offset..offset + 8].copy_from_slice(&limb.to_be_bytes());
        }
        buf
    }

    fn cmp(&self, other: &Self) -> Ordering {
        for i in (0..16).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        Ordering::Equal
    }

    fn add_carrying(&self, other: &Self) -> (Self, bool) {
        let mut out = [0u64; 16];
        let mut carry = 0u128;
        for (dst, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            let sum = u128::from(a) + u128::from(b) + carry;
            *dst = sum as u64;
            carry = sum >> 64;
        }
        (Self(out), carry != 0)
    }

    fn sub_wrapping(&self, other: &Self) -> Self {
        let mut out = [0u64; 16];
        let mut borrow = 0u64;
        for (dst, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            let (d1, b1) = a.overflowing_sub(b);
            let (d2, b2) = d1.overflowing_sub(borrow);
            *dst = d2;
            borrow = u64::from(b1 || b2);
        }
        Self(out)
    }

    /// Requires `self < modulus` and `other < modulus`.
    fn add_mod(&self, other: &Self, modulus: &Self) -> Self {
        let (sum, carry) = self.add_carrying(other);
        if carry || sum.cmp(modulus) != Ordering::Less {
            sum.sub_wrapping(modulus)
        } else {
            sum
        }
    }

    /// Requires `self < modulus` and `other < modulus`.
    fn mul_mod(&self, other: &Self, modulus: &Self) -> Self {
        let mut res = Self([0; 16]);
        let mut cur = *self;
        let Some(top) = other.0.iter().rposition(|&w| w != 0) else {
            return res;
        };
        for limb_idx in 0..=top {
            let mut word = other.0[limb_idx];
            let bits = if limb_idx == top {
                64 - word.leading_zeros()
            } else {
                64
            };
            for _ in 0..bits {
                if word & 1 != 0 {
                    res = res.add_mod(&cur, modulus);
                }
                cur = cur.add_mod(&cur, modulus);
                word >>= 1;
            }
        }
        res
    }

    /// Requires `self < modulus` and `modulus > 1`.
    fn pow_mod(&self, exp_be: &[u8], modulus: &Self) -> Self {
        let mut res = Self([0; 16]);
        res.0[0] = 1;
        for &byte in exp_be {
            for shift in (0..8).rev() {
                res = res.mul_mod(&res, modulus);
                if (byte >> shift) & 1 != 0 {
                    res = res.mul_mod(self, modulus);
                }
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn aes_sbox_matches_gf256_definition() {
        fn gf_mul(mut a: u8, mut b: u8) -> u8 {
            let mut p = 0u8;
            for _ in 0..8 {
                if b & 1 != 0 {
                    p ^= a;
                }
                a = xtime(a);
                b >>= 1;
            }
            p
        }
        fn gf_inv(a: u8) -> u8 {
            if a == 0 {
                return 0;
            }
            let mut acc = 1u8;
            let mut base = a;
            let mut exp = 254u8;
            while exp > 0 {
                if exp & 1 != 0 {
                    acc = gf_mul(acc, base);
                }
                base = gf_mul(base, base);
                exp >>= 1;
            }
            acc
        }

        for x in 0u8..=255 {
            let inv = gf_inv(x);
            let affine = inv
                ^ inv.rotate_left(1)
                ^ inv.rotate_left(2)
                ^ inv.rotate_left(3)
                ^ inv.rotate_left(4)
                ^ 0x63;
            assert_eq!(AES_SBOX[usize::from(x)], affine, "mismatch at SBOX[{x:#04x}]");
        }
    }

    #[test]
    fn md5_matches_rfc1321_and_lx_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(md5_hex("音乐".as_bytes()), "95521bb744463633c858cc50abb44e7d");
        // Exercise 56..63-byte boundary requiring two final padding blocks.
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
    }

    #[test]
    fn sha1_matches_fips180_vectors() {
        assert_eq!(hex_lower(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex_lower(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex_lower(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn aes_matches_fips197_and_lx_vectors() {
        // FIPS-197 Appendix C.1 / C.2 / C.3 plaintext block (first 16 bytes of ECB output).
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let k128: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let c128 = aes_encrypt(AesMode::Aes128Ecb, &k128, &[], &pt).unwrap();
        assert_eq!(c128.len(), 32); // 16-byte input + 16-byte PKCS#7 padding block
        assert_eq!(hex_lower(&c128[..16]), "69c4e0d86a7b0430d8cdb78070b4c55a");

        let k192: [u8; 24] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        ];
        let c192 = aes_encrypt(AesMode::Aes192Ecb, &k192, &[], &pt).unwrap();
        assert_eq!(hex_lower(&c192[..16]), "dda97ca4864cdfe06eaf70a0ec0d7191");

        let k256: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let c256 = aes_encrypt(AesMode::Aes256Ecb, &k256, &[], &pt).unwrap();
        assert_eq!(hex_lower(&c256[..16]), "8ea2b7ca516745bfeafc49904b496089");

        // Exact LX fixture vectors.
        let lx_input = "音乐".as_bytes();
        let lx_key = b"0123456789abcdef";
        let lx_iv = b"abcdef0123456789";
        assert_eq!(
            hex_lower(&aes_encrypt(AesMode::Aes128Cbc, lx_key, lx_iv, lx_input).unwrap()),
            "8c8fad9c5a1b0a87d0aa36de245255f9"
        );
        assert_eq!(
            hex_lower(&aes_encrypt(AesMode::Aes128Ecb, lx_key, &[], lx_input).unwrap()),
            "99e3af56ccd5507cabebf1e10e00ebae"
        );
        assert!(aes_encrypt(AesMode::Aes128Cbc, lx_key, b"short", lx_input).is_err());
        assert!(aes_encrypt(AesMode::Aes128Ecb, lx_key, lx_iv, lx_input).is_err());
    }

    #[test]
    fn rsa_matches_spki_and_pkcs1_fixture_vector() {
        const SPKI_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\n\
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC9XXfDgUmi2IzQbfX7kmnnfcZL\n\
1dwBdomuUkAEta8xmYet0w7hyg1yrCHDSPMUKRWg0rIHTDhp1M4dZj/BkZ/xK2Ah\n\
j+EWG+7gJAjAIUsw0nU12FbvaL7KelSYzz+rQu+xVMRJ8YUqF85dCzO10oekt7hW\n\
3QfpR0mHVO8TYBJM3wIDAQAB\n\
-----END PUBLIC KEY-----";
        const EXPECTED: &str = concat!(
            "9a62394ad5de511b2cc88d2a7595331fe7d0c71798bfdaab01b16e137619427e",
            "0484eb7ea8c44a0a352769fb3845e7a3d3d1a6eba63d5c27cf6e98df77f01d9a",
            "4a77a0d08d9d2992ab9f2885cb97b9d27d1ddf9295e1b4347930d3c8a60e8626",
            "1a14e530bda0ef3437c626656eb7707aec9e8f2ec53826ee2bf857fd6351e9c4",
        );

        let encrypted = rsa_public_encrypt_nopad_1024(b"fixture", SPKI_PEM).unwrap();
        assert_eq!(hex_lower(&encrypted), EXPECTED);

        // Extract inner PKCS#1 DER from SPKI BIT STRING and re-wrap as PKCS#1 PEM.
        let spki_der =
            decode_pem_block(std::str::from_utf8(SPKI_PEM).unwrap(), "PUBLIC KEY").unwrap();
        let (_, body, _) = read_der_tlv(&spki_der).unwrap();
        let (_, _, rest) = read_der_tlv(body).unwrap();
        let (_, bit_val, _) = read_der_tlv(rest).unwrap();
        let pkcs1_pem = format!(
            "-----BEGIN RSA PUBLIC KEY-----\n{}\n-----END RSA PUBLIC KEY-----",
            BASE64.encode(&bit_val[1..])
        );
        let pkcs1_encrypted =
            rsa_public_encrypt_nopad_1024(b"fixture", pkcs1_pem.as_bytes()).unwrap();
        assert_eq!(hex_lower(&pkcs1_encrypted), EXPECTED);

        assert!(rsa_public_encrypt_nopad_1024(b"fixture", b"invalid").is_err());
        assert!(rsa_public_encrypt_nopad_1024(&[0u8; 129], SPKI_PEM).is_err());
        assert!(rsa_public_encrypt_nopad_1024(&[0xffu8; 128], SPKI_PEM).is_err());
    }

    #[test]
    fn random_bytes_returns_requested_length() {
        assert!(random_bytes(0).unwrap().is_empty());
        let bytes = random_bytes(32).unwrap();
        assert_eq!(bytes.len(), 32);
    }
}
