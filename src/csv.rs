//! Fast CSV row scanning per the rules' dialect.
//!
//! A SIMD pass records the offsets of every ',', '"' and '\n' in a buffer; rows
//! are then cut from that list. A field may be wrapped in quotes as a whole
//! (stripped); any other quote, a wrong field count, or an empty line makes the
//! line malformed, and it is skipped.

/// Appends to `out` the offsets (relative to d) of ',', '"', '\n' in d[from..to].
#[inline(never)]
pub fn index(d: &[u8], from: usize, to: usize, out: &mut Vec<u32>) {
    out.clear();
    out.reserve((to - from) / 6 + 64);
    let mut i = from;
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
    unsafe {
        use std::arch::x86_64::*;
        let comma = _mm512_set1_epi8(b',' as i8);
        let quote = _mm512_set1_epi8(b'"' as i8);
        let nl = _mm512_set1_epi8(b'\n' as i8);
        while i + 64 <= to {
            let v = _mm512_loadu_si512(d.as_ptr().add(i) as *const _);
            let mut m = _mm512_cmpeq_epi8_mask(v, comma)
                | _mm512_cmpeq_epi8_mask(v, quote)
                | _mm512_cmpeq_epi8_mask(v, nl);
            let len = out.len();
            let cnt = m.count_ones() as usize;
            if out.capacity() < len + 64 {
                out.reserve(4096);
            }
            let p = out.as_mut_ptr().add(len);
            let mut k = 0;
            while m != 0 {
                *p.add(k) = (i as u32) + m.trailing_zeros();
                m &= m - 1;
                k += 1;
            }
            out.set_len(len + cnt);
            i += 64;
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
    {
        while i + 8 <= to {
            let w = u64::from_le_bytes(d[i..i + 8].try_into().unwrap());
            let mut m = has(w, b',') | has(w, b'"') | has(w, b'\n');
            while m != 0 {
                out.push(i as u32 + (m.trailing_zeros() >> 3));
                m &= m - 1;
            }
            i += 8;
        }
    }
    while i < to {
        let c = d[i];
        if c == b',' || c == b'"' || c == b'\n' {
            out.push(i as u32);
        }
        i += 1;
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
#[inline(always)]
fn has(w: u64, c: u8) -> u64 {
    // exact zero-byte detection: high bit set in each byte equal to c
    let x = w ^ (0x0101010101010101u64 * c as u64);
    let t = (x & 0x7F7F7F7F7F7F7F7F).wrapping_add(0x7F7F7F7F7F7F7F7F);
    !(t | x | 0x7F7F7F7F7F7F7F7F)
}

/// Yields the offsets of ',', '"' and '\n' in d[start..end] in order, from
/// 64-byte block masks. Needs 64 readable bytes past `end`.
struct Seps {
    p: *const u8,
    base: usize,
    m: u64,
    end: usize,
}

impl Seps {
    #[inline(always)]
    fn block(&self, base: usize) -> u64 {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
        unsafe {
            use std::arch::x86_64::*;
            let v = _mm512_loadu_si512(self.p.add(base) as *const _);
            _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8))
                | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8))
                | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8))
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
        {
            let mut m = 0u64;
            for j in 0..8 {
                let w = unsafe { std::ptr::read_unaligned(self.p.add(base + 8 * j) as *const u64) };
                let x = has(w, b',') | has(w, b'"') | has(w, b'\n');
                for b in 0..8 {
                    m |= ((x >> (8 * b + 7)) & 1) << (8 * j + b);
                }
            }
            m
        }
    }
    #[inline(always)]
    fn new(d: &[u8], start: usize, end: usize) -> Self {
        let base = start & !63;
        let mut s = Seps { p: d.as_ptr(), base, m: 0, end };
        s.m = s.block(base) & (u64::MAX << (start - base));
        s
    }
    /// Next separator offset, or `end` when the data is exhausted.
    #[inline(always)]
    fn next(&mut self) -> usize {
        while self.m == 0 {
            self.base += 64;
            if self.base >= self.end {
                return self.end;
            }
            self.m = self.block(self.base);
        }
        let p = self.base + self.m.trailing_zeros() as usize;
        self.m &= self.m - 1;
        if p >= self.end {
            self.m = 0;
            self.base = usize::MAX - 64;
            return self.end;
        }
        p
    }
}

/// Calls f with the fields of every well-formed row whose line starts in
/// [start, stop). d must hold every such line in full (a line may end at
/// d.len()), plus 64 readable bytes of slack past d.len().
#[inline(always)]
pub fn rows<const N: usize>(d: &[u8], start: usize, stop: usize, _pos: &mut Vec<u32>, mut f: impl FnMut(&[&[u8]; N])) {
    if start >= stop {
        return;
    }
    let end = match d[stop - 1..].iter().position(|&c| c == b'\n') {
        Some(p) => stop - 1 + p + 1,
        None => d.len(),
    };
    let at = |p: usize| -> u8 { if p < end { unsafe { *d.get_unchecked(p) } } else { b'\n' } };
    let mut it = Seps::new(d, start, end);
    let mut fields: [&[u8]; N] = [&[]; N];
    let mut ls = start;
    'line: while ls < end {
        let mut fs = ls;
        let mut i = 0;
        let last;
        loop {
            let mut p = it.next();
            let mut c = at(p);
            let mut a = fs;
            let mut b = p;
            if c == b'"' {
                // a quote must open the field and close it right before a separator
                if p != fs {
                    last = p;
                    break;
                }
                let q = it.next();
                if at(q) != b'"' {
                    last = q;
                    break;
                }
                p = it.next();
                c = at(p);
                if c == b'"' || p != q + 1 {
                    last = p;
                    break;
                }
                a = fs + 1;
                b = q;
            }
            // field [a, b), terminated by c at p
            if i < N {
                fields[i] = unsafe { d.get_unchecked(a..b) };
            }
            i += 1;
            if c == b'\n' {
                if i == N && p > ls {
                    f(&fields);
                }
                ls = p + 1;
                continue 'line;
            }
            fs = p + 1;
        }
        // malformed: skip to the end of this line
        if at(last) == b'\n' {
            ls = last + 1;
            continue 'line;
        }
        loop {
            let p = it.next();
            if at(p) == b'\n' {
                ls = p + 1;
                continue 'line;
            }
        }
    }
}

#[inline(always)]
fn masks128(p: *const u8) -> (u128, u128, u128) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
    unsafe {
        use std::arch::x86_64::*;
        let v0 = _mm512_loadu_si512(p as *const _);
        let v1 = _mm512_loadu_si512(p.add(64) as *const _);
        let nl = _mm512_set1_epi8(b'\n' as i8);
        let cm = _mm512_set1_epi8(b',' as i8);
        let qt = _mm512_set1_epi8(b'"' as i8);
        let n = (_mm512_cmpeq_epi8_mask(v0, nl) as u128) | ((_mm512_cmpeq_epi8_mask(v1, nl) as u128) << 64);
        let c = (_mm512_cmpeq_epi8_mask(v0, cm) as u128) | ((_mm512_cmpeq_epi8_mask(v1, cm) as u128) << 64);
        let q = (_mm512_cmpeq_epi8_mask(v0, qt) as u128) | ((_mm512_cmpeq_epi8_mask(v1, qt) as u128) << 64);
        (n, c, q)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
    {
        let (mut n, mut c, mut q) = (0u128, 0u128, 0u128);
        for j in 0..128 {
            let b = unsafe { *p.add(j) };
            n |= ((b == b'\n') as u128) << j;
            c |= ((b == b',') as u128) << j;
            q |= ((b == b'"') as u128) << j;
        }
        (n, c, q)
    }
}

/// Inclusive prefix XOR of the bits of x: bit i of the result is the XOR of
/// bits 0..=i.
#[inline(always)]
fn prefix_xor(x: u128) -> u128 {
    #[inline(always)]
    fn p64(x: u64) -> u64 {
        #[cfg(all(target_arch = "x86_64", target_feature = "pclmulqdq"))]
        unsafe {
            use std::arch::x86_64::*;
            _mm_cvtsi128_si64(_mm_clmulepi64_si128(_mm_set_epi64x(0, x as i64), _mm_set1_epi8(-1), 0)) as u64
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "pclmulqdq")))]
        {
            let mut x = x;
            for s in [1, 2, 4, 8, 16, 32] {
                x ^= x << s;
            }
            x
        }
    }
    let lo = p64(x as u64);
    let hi = p64((x >> 64) as u64) ^ (0u64.wrapping_sub(lo >> 63));
    (lo as u128) | ((hi as u128) << 64)
}

/// Like `rows`, but cuts each line with two 64-byte compares and no
/// data-dependent branches. Lines over 127 bytes, and a last line without a
/// newline, go through the scalar splitter. Needs 128 readable bytes past d.len().
const PF: usize = 4096;

#[inline(always)]
pub fn rows_fast<const N: usize>(d: &[u8], start: usize, stop: usize, mut f: impl FnMut(&[&[u8]; N])) {
    let base = d.as_ptr();
    let mut fields: [&[u8]; N] = [&[]; N];
    let mut ls = start;
    while ls < stop {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_prefetch(base.add(ls + PF) as *const i8, std::arch::x86_64::_MM_HINT_T0);
        }
        let (n, c, q) = masks128(unsafe { base.add(ls) });
        if n == 0 || ls + (n.trailing_zeros() as usize) >= d.len() {
            // long line, or the data ends without a newline
            let e = match d[ls..].iter().position(|&x| x == b'\n') {
                Some(p) => ls + p,
                None => d.len(),
            };
            if crate::split(&d[ls..e], &mut fields) {
                f(&fields);
            }
            ls = e + 1;
            continue;
        }
        let l = n.trailing_zeros() as usize;
        let lm = (1u128 << l) - 1;
        let c = c & lm;
        let q = q & lm;
        let next = ls + l + 1;
        if c.count_ones() as usize != N - 1 {
            ls = next;
            continue;
        }
        // a quote may only open a field (after a separator) or close one (before
        // a separator), and every field opened must be closed, and vice versa
        let seps = c | (1u128 << l);
        let starts = (c << 1) | 1;
        let ends1 = seps >> 1;
        let qs = q & starts;
        let qe = q & ends1;
        let z = prefix_xor(qs | (qe << 1));
        let ok = (q & !(starts | ends1)) | (qs & qe) | (z & seps) == 0;
        let mut a_m = (starts & !qs) | (qs << 1);
        let mut b_m = (seps & !(qe << 1)) | qe;
        for i in 0..N {
            let a = a_m.trailing_zeros() as usize;
            let b = b_m.trailing_zeros() as usize;
            a_m &= a_m.wrapping_sub(1);
            b_m &= b_m.wrapping_sub(1);
            fields[i] = unsafe { std::slice::from_raw_parts(base.add(ls + a), b.max(a) - a) };
        }
        let _ = c;
        if ok {
            f(&fields);
        }
        ls = next;
    }
}
