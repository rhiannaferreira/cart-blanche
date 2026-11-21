//! IDs as u64 keys. A string of 1..=10 bytes from [0-9a-z] packs exactly as
//! 6-bit codes (0 = padding, '0'..'9' = 1..10, 'a'..'z' = 11..36), first byte
//! most significant, so numeric order is byte-wise order. Anything else gets a
//! hashed key with the top bit set; its bytes go to a side table used for
//! comparisons and output.
//!
//! Callers must guarantee that 16 bytes from the start of `b` are readable
//! (the CSV buffers carry slack), because the fast path loads whole words.
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub type Key = u64;

static LONG: Mutex<Option<HashMap<Key, Vec<u8>>>> = Mutex::new(None);

pub const TAG: u64 = 1 << 63;

const L01: u64 = 0x0101_0101_0101_0101;
const L80: u64 = 0x8080_8080_8080_8080;

/// 6-bit codes of up to 8 bytes in w (byte i = char i), lanes beyond n zero.
/// Returns None if any of the first n bytes is outside [0-9a-z].
#[inline(always)]
fn codes(w: u64, n: usize) -> Option<u64> {
    let lanes: u64 = if n >= 8 { u64::MAX } else { (1u64 << (8 * n)) - 1 };
    let x = w & lanes;
    if x & L80 != 0 {
        return None;
    }
    let ge30 = x.wrapping_add(0x50 * L01) & L80;
    let gt39 = x.wrapping_add(0x46 * L01) & L80;
    let ge61 = x.wrapping_add(0x1F * L01) & L80;
    let gt7a = x.wrapping_add(0x05 * L01) & L80;
    let digit = ge30 & !gt39;
    let lower = ge61 & !gt7a;
    if (digit | lower) != (L80 & lanes) {
        return None;
    }
    let lo = lower >> 7;
    Some(x - (0x2F * L01 & lanes) - 0x27 * lo)
}

#[inline(always)]
fn pack8(c: u64) -> u64 {
    // c holds one 6-bit code per byte, char 0 in byte 0; returns 48 bits, char 0 on top
    let s = c.swap_bytes();
    #[cfg(target_feature = "bmi2")]
    unsafe {
        std::arch::x86_64::_pext_u64(s, 0x3F3F_3F3F_3F3F_3F3F)
    }
    #[cfg(not(target_feature = "bmi2"))]
    {
        let mut r = 0u64;
        for i in (0..8).rev() {
            r = (r << 6) | ((s >> (8 * i)) & 0x3F);
        }
        r
    }
}

#[inline(always)]
pub fn key(b: &[u8]) -> Key {
    let n = b.len();
    if n >= 1 && n <= 10 {
        let w0 = unsafe { std::ptr::read_unaligned(b.as_ptr() as *const u64) };
        if let Some(c0) = codes(w0, n.min(8)) {
            let hi = pack8(c0) << 12;
            if n <= 8 {
                return hi;
            }
            let w1 = unsafe { std::ptr::read_unaligned(b.as_ptr().add(8) as *const u16) } as u64;
            if let Some(c1) = codes(w1, n - 8) {
                return hi | ((c1 & 0x3F) << 6) | ((c1 >> 8) & 0x3F);
            }
        }
    }
    slow_key(b)
}

/// Safe variant for slices without readable slack (small files, tests).
pub fn key_owned(b: &[u8]) -> Key {
    let mut buf = [0u8; 32];
    if b.len() <= 16 {
        buf[..b.len()].copy_from_slice(b);
        key(&buf[..b.len()])
    } else {
        slow_key(b)
    }
}

#[cold]
#[inline(never)]
fn slow_key(b: &[u8]) -> Key {
    let mut h: u64 = 0xcbf29ce484222325;
    for &c in b {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b.len() as u64;
    h = h.wrapping_mul(0x9E3779B97F4A7C15);
    h ^= h >> 31;
    // top bit set; never u64::MAX (the hash table's empty marker)
    let mut k = TAG | (h >> 2);
    if k == u64::MAX {
        k -= 1;
    }
    let mut g = LONG.lock().unwrap();
    g.get_or_insert_with(HashMap::new).entry(k).or_insert_with(|| b.to_vec());
    k
}

const ALPH: &[u8; 37] = b"\x000123456789abcdefghijklmnopqrstuvwxyz";

#[inline]
pub fn unpack_into(k: Key, out: &mut Vec<u8>) {
    if k & TAG != 0 {
        let g = LONG.lock().unwrap();
        if let Some(v) = g.as_ref().and_then(|m| m.get(&k)) {
            out.extend_from_slice(v);
        }
        return;
    }
    for i in (0..10).rev() {
        let c = ((k >> (6 * i)) & 0x3F) as usize;
        if c == 0 {
            break;
        }
        out.push(ALPH[c]);
    }
}

pub fn unpack(k: Key) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    unpack_into(k, &mut v);
    v
}

#[inline(always)]
pub fn cmp_keys(a: Key, b: Key) -> Ordering {
    if (a | b) & TAG == 0 {
        a.cmp(&b)
    } else if a == b {
        Ordering::Equal
    } else {
        unpack(a).cmp(&unpack(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_order() {
        let ids: Vec<&[u8]> = vec![b"u1", b"u10", b"u2", b"a", b"zzzzzzzzzz", b"0", b"u43a7vvaq", b"e42huhoug1", b"U1", b" u1", b"u1 ", b"abcdefghijk"];
        for a in &ids {
            let ka = key_owned(a);
            assert_eq!(&unpack(ka)[..], *a);
            for b in &ids {
                let kb = key_owned(b);
                assert_eq!(cmp_keys(ka, kb), a.cmp(b), "{:?} {:?}", a, b);
                assert_eq!(ka == kb, a == b);
            }
        }
    }
}
