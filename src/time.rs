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
    if b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
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
