//! Timestamp parsing per rules.md: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 with `+HH:MM`
//! offset, F3 epoch milliseconds (ASCII digits only).
pub const DAY_MS: i64 = 86_400_000;

#[inline(always)]
fn d2(b: &[u8], i: usize) -> Option<u32> {
    let a = b[i].wrapping_sub(b'0');
    let c = b[i + 1].wrapping_sub(b'0');
    if a > 9 || c > 9 {
        None
    } else {
        Some(a as u32 * 10 + c as u32)
    }
}

/// Eight ASCII digits, first in the lowest byte, as a number.
#[inline(always)]
fn digits8(w: u64) -> Option<u64> {
    let x = w.wrapping_sub(0x3030_3030_3030_3030);
    if (x | x.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let x = (x.wrapping_mul(10) + (x >> 8)) & 0x00FF_00FF_00FF_00FF;
    let x = (x.wrapping_mul(100) + (x >> 16)) & 0x0000_FFFF_0000_FFFF;
    Some((x.wrapping_mul(10000) + (x >> 32)) & 0xFFFF_FFFF)
}

#[inline(always)]
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Direct-mapped cache of validated dates: the 10 raw bytes "YYYY-MM-DD" to
/// the day's first millisecond. Every dataset spans a few weeks, so it hits.
pub struct DateCache {
    k0: [u64; 64],
    k1: [u16; 64],
    v: [i64; 64],
}

impl DateCache {
    pub fn new() -> Self {
        DateCache { k0: [0; 64], k1: [0; 64], v: [i64::MIN; 64] }
    }
}

#[cold]
fn parse_date_slow(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = d2(b, 0)? as i64 * 100 + d2(b, 2)? as i64;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    if !(1..=12).contains(&mo) || d < 1 {
        return None;
    }
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    };
    if d > dim {
        return None;
    }
    Some(days_from_civil(y, mo, d) * DAY_MS)
}

#[inline(always)]
fn parse_dt(b: &[u8], c: &mut DateCache) -> Option<i64> {
    let w0 = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let w1 = u16::from_le_bytes(b[8..10].try_into().unwrap());
    let i = ((w0.wrapping_mul(0x9E3779B97F4A7C15) ^ (w1 as u64).wrapping_mul(0xC2B2AE3D27D4EB4F)) >> 58) as usize;
    let day = if c.k0[i] == w0 && c.k1[i] == w1 && c.v[i] != i64::MIN {
        c.v[i]
    } else {
        let d = parse_date_slow(b)?;
        c.k0[i] = w0;
        c.k1[i] = w1;
        c.v[i] = d;
        d
    };
    if b[10] != b'T' {
        return None;
    }
    // "HH:MM:SS" in one word: ':' at lanes 2 and 5, digits elsewhere
    let w = u64::from_le_bytes(b[11..19].try_into().unwrap());
    const COLONS: u64 = 0x0000_3A00_003A_0000;
    const CMASK: u64 = 0x0000_FF00_00FF_0000;
    if w & CMASK != COLONS {
        return None;
    }
    let x = (w & !CMASK).wrapping_sub(0x3030_0030_3000_3030 & !CMASK);
    let x = x & !CMASK;
    // every digit lane must now be 0..=9 with no borrow
    if (x | x.wrapping_add(0x7676_0076_7600_7676 & !CMASK)) & (0x8080_0080_8000_8080) != 0 {
        return None;
    }
    let lane = |i: u32| ((x >> (8 * i)) & 0xFF) as u32;
    let h = lane(0) * 10 + lane(1);
    let mi = lane(3) * 10 + lane(4);
    let s = lane(6) * 10 + lane(7);
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(day + (h as i64 * 3600 + mi as i64 * 60 + s as i64) * 1000)
}

#[inline(always)]
pub fn parse_ts(b: &[u8], c: &mut DateCache) -> Option<i64> {
    let n = b.len();
    if n == 20 {
        if b[19] != b'Z' {
            return None;
        }
        return parse_dt(b, c);
    }
    if n == 25 && (b[19] == b'+' || b[19] == b'-') && b[22] == b':' {
        let base = parse_dt(b, c)?;
        let oh = d2(b, 20)?;
        let om = d2(b, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let off = (oh as i64 * 60 + om as i64) * 60_000;
        return Some(if b[19] == b'+' { base - off } else { base + off });
    }
    if n == 0 {
        return None;
    }
    if (9..=16).contains(&n) {
        let k = n - 8;
        let w0 = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let w0 = if k == 8 { w0 } else { ((w0 & ((1u64 << (8 * k)) - 1)) << (8 * (8 - k))) | (0x3030_3030_3030_3030u64 >> (8 * k)) };
        let w1 = u64::from_le_bytes(b[n - 8..n].try_into().unwrap());
        let hi = digits8(w0)?;
        let lo = digits8(w1)?;
        return Some((hi * 100_000_000 + lo) as i64);
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as u64)?;
    }
    if v > i64::MAX as u64 {
        return None;
    }
    Some(v as i64)
}
