//! Open-addressing hash map from u64 keys (never u64::MAX) to u64 values.
pub struct Map64 {
    slots: Vec<[u64; 2]>,
    mask: usize,
    len: usize,
}

const EMPTY: u64 = u64::MAX;

#[inline(always)]
fn h(k: u64) -> u64 {
    let x = k.wrapping_mul(0x9E3779B97F4A7C15);
    x ^ (x >> 32)
}

impl Map64 {
    pub fn new(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        Map64 { slots: vec![[EMPTY, 0]; cap], mask: cap - 1, len: 0 }
    }
    /// Empties the map and sizes it for n keys at a load of at most ~0.65,
    /// reusing the allocation.
    pub fn reset(&mut self, n: usize) {
        let cap = (n + n / 2 + 16).next_power_of_two();
        if self.slots.len() < cap {
            self.slots = vec![[EMPTY, 0]; cap];
        } else {
            self.slots[..cap].fill([EMPTY, 0]);
        }
        self.mask = cap - 1;
        self.len = 0;
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let live = self.mask + 1;
        let old = std::mem::take(&mut self.slots);
        let cap = live * 2;
        self.slots = vec![[EMPTY, 0]; cap];
        self.mask = cap - 1;
        for s in &old[..live] {
            if s[0] != EMPTY {
                let i = self.slot(s[0]);
                self.slots[i] = *s;
            }
        }
    }
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let p = self.slots.as_ptr().add(h(k) as usize & self.mask) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }
    /// The slot holding k, or the empty slot where k would go.
    #[inline(always)]
    pub fn slot(&self, k: u64) -> usize {
        let mut i = h(k) as usize & self.mask;
        loop {
            let x = unsafe { self.slots.get_unchecked(i)[0] };
            if x == k || x == EMPTY {
                return i;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    pub fn val_at(&self, i: usize) -> Option<u64> {
        let s = unsafe { self.slots.get_unchecked(i) };
        if s[0] == EMPTY {
            None
        } else {
            Some(s[1])
        }
    }
    #[inline(always)]
    pub fn set_at(&mut self, i: usize, k: u64, v: u64) {
        unsafe { *self.slots.get_unchecked_mut(i) = [k, v] };
    }
    #[inline(always)]
    pub fn get(&self, k: u64) -> Option<u64> {
        self.val_at(self.slot(k))
    }
    #[inline(always)]
    pub fn get_or_insert(&mut self, k: u64, v: u64) -> (u64, bool) {
        let i = self.slot(k);
        match self.val_at(i) {
            None => {
                self.set_at(i, k, v);
                self.len += 1;
                if self.len * 10 > (self.mask + 1) * 7 {
                    self.grow();
                }
                (v, true)
            }
            Some(old) => (old, false),
        }
    }
}

// ---------------------------------------------------------------- huge pages
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}
const PROT_RW: i32 = 0x1 | 0x2;
const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;
const MADV_HUGEPAGE: i32 = 14;

static REGIONS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// Drops the pages of every big mapping, from nt threads at once, so that
/// process exit has little left to tear down. Nothing may be read afterwards.
pub fn release_all(nt: usize) {
    const MADV_DONTNEED: i32 = 4;
    let mut parts = vec![];
    for &(a, len) in REGIONS.lock().unwrap().iter() {
        let mut o = 0;
        while o < len {
            let l = (len - o).min(64 << 20);
            parts.push((a + o, l));
            o += l;
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..nt {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= parts.len() {
                    break;
                }
                unsafe { madvise(parts[i].0 as *mut u8, parts[i].1, MADV_DONTNEED) };
            });
        }
    });
}

/// A zeroed, never-freed allocation backed by transparent huge pages when the
/// kernel allows it. Fewer TLB misses for big random-access tables.
pub fn alloc_huge<T: Copy + 'static>(n: usize) -> &'static mut [T] {
    let bytes = (n * std::mem::size_of::<T>()).max(1);
    let len = (bytes + (2 << 20) - 1) & !((2 << 20) - 1);
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + (2 << 20), PROT_RW, MAP_PRIVATE_ANON, -1, 0);
        assert!(!p.is_null() && p as isize != -1, "mmap failed");
        // align to 2 MiB so the kernel can use huge pages
        let a = ((p as usize + (2 << 20) - 1) & !((2 << 20) - 1)) as *mut u8;
        madvise(a, len, MADV_HUGEPAGE);
        REGIONS.lock().unwrap().push((a as usize, len));
        std::slice::from_raw_parts_mut(a as *mut T, n)
    }
}

/// Read-mostly map u64 -> u64 for the link table: interleaved key/value slots
/// on huge pages, with a prefetch hook so lookups can overlap other work.
pub struct LinkMap {
    slots: &'static mut [[u64; 2]],
    mask: usize,
    filter: Vec<u64>, // one bit per hash bucket: "may be in the map"
    fmask: usize,
}

impl LinkMap {
    pub fn new(n: usize) -> Self {
        let cap = ((n + n / 4).max(16)).next_power_of_two();
        let slots = alloc_huge::<[u64; 2]>(cap); // zeroed: key 0 never occurs, so 0 marks empty
        let fbits = (n * 16).next_power_of_two().clamp(1 << 16, 1 << 26);
        LinkMap { slots, mask: cap - 1, filter: vec![0; fbits / 64], fmask: fbits - 1 }
    }
    #[inline(always)]
    fn home(&self, k: u64) -> usize {
        h(k) as usize & self.mask
    }
    #[inline(always)]
    fn fbit(&self, k: u64) -> usize {
        (k.wrapping_mul(0xC2B2AE3D27D4EB4F) >> 20) as usize & self.fmask
    }
    #[inline(always)]
    pub fn prefetch_slot(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let p = self.slots.as_ptr().add(self.home(k)) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }
    pub fn insert(&mut self, k: u64, v: u64) {
        let b = self.fbit(k);
        self.filter[b >> 6] |= 1 << (b & 63);
        let mut i = self.home(k);
        loop {
            if self.slots[i][0] == 0 || self.slots[i][0] == k {
                self.slots[i] = [k, v];
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let b = self.fbit(k);
            let f = self.filter.as_ptr().add(b >> 6) as *const i8;
            std::arch::x86_64::_mm_prefetch(f, std::arch::x86_64::_MM_HINT_T0);
            let p = self.slots.as_ptr().add(self.home(k)) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }
    /// The value for k, or k itself when absent.
    #[inline(always)]
    pub fn map(&self, k: u64) -> u64 {
        let b = self.fbit(k);
        if unsafe { *self.filter.get_unchecked(b >> 6) } & (1 << (b & 63)) == 0 {
            return k;
        }
        let mut i = self.home(k);
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s[0] == k {
                return s[1];
            }
            if s[0] == 0 {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u8; 64]);

/// Fixed-capacity bins carved from one huge-page region, for scattering
/// records into partitions without reallocation. A bin that fills up spills
/// into an ordinary Vec. Records whose size divides 64 are gathered into a
/// cache line per bin and written out whole with streaming stores; call
/// `finish` before reading.
pub struct Bins<T: Copy + 'static> {
    base: *mut T,
    cap: usize,
    lens: Vec<usize>,
    spill: Vec<Vec<T>>,
    wc: Vec<Line>,
}

unsafe impl<T: Copy + Send + 'static> Send for Bins<T> {}
unsafe impl<T: Copy + Sync + 'static> Sync for Bins<T> {}

impl<T: Copy + 'static> Bins<T> {
    const PER: usize = if 64 % std::mem::size_of::<T>() == 0 { 64 / std::mem::size_of::<T>() } else { 0 };
    pub fn new(nbins: usize, cap: usize) -> Self {
        let mut cap = cap.max(4);
        if Self::PER > 0 {
            cap = cap.next_multiple_of(Self::PER);
        }
        let mem = alloc_huge::<T>(nbins * cap);
        let wc = if Self::PER > 0 { vec![Line([0; 64]); nbins] } else { Vec::new() };
        Bins { base: mem.as_mut_ptr(), cap, lens: vec![0; nbins], spill: (0..nbins).map(|_| Vec::new()).collect(), wc }
    }
    #[inline(always)]
    pub fn push(&mut self, b: usize, x: T) {
        let l = unsafe { *self.lens.get_unchecked(b) };
        if l < self.cap {
            unsafe {
                *self.lens.get_unchecked_mut(b) = l + 1;
                if Self::PER == 0 {
                    self.base.add(b * self.cap + l).write(x);
                    return;
                }
                let k = l % Self::PER;
                let line = self.wc.get_unchecked_mut(b) as *mut Line as *mut T;
                line.add(k).write(x);
                if k == Self::PER - 1 {
                    let dst = self.base.add(b * self.cap + l + 1 - Self::PER);
                    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
                    {
                        use std::arch::x86_64::*;
                        let v = _mm512_load_si512(line as *const _);
                        _mm512_stream_si512(dst as *mut _, v);
                    }
                    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
                    std::ptr::copy_nonoverlapping(line, dst, Self::PER);
                }
            }
        } else {
            self.spill[b].push(x);
        }
    }
    /// Writes out partly filled lines. Call on the reading side, after the
    /// writing thread has finished (and fenced).
    pub fn finish(&mut self) {
        if Self::PER == 0 {
            return;
        }
        for b in 0..self.lens.len() {
            let l = self.lens[b].min(self.cap);
            let k = l % Self::PER;
            if k > 0 {
                unsafe {
                    let line = &self.wc[b] as *const Line as *const T;
                    std::ptr::copy_nonoverlapping(line, self.base.add(b * self.cap + l - k), k);
                }
            }
        }
    }
    #[inline(always)]
    pub fn bin(&self, b: usize) -> (&[T], &[T]) {
        let s = unsafe { std::slice::from_raw_parts(self.base.add(b * self.cap), self.lens[b].min(self.cap)) };
        (s, &self.spill[b])
    }
    pub fn len(&self, b: usize) -> usize {
        self.lens[b].min(self.cap) + self.spill[b].len()
    }
}

/// Orders this thread's streaming stores before what follows.
#[inline(always)]
pub fn fence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_sfence()
    }
}

/// Maps a whole file read-only (never unmapped). Null on failure.
pub fn map_file(f: &std::fs::File, len: usize) -> *const u8 {
    use std::os::unix::io::AsRawFd;
    const PROT_READ: i32 = 1;
    const MAP_PRIVATE: i32 = 2;
    const MAP_POPULATE: i32 = 0x8000;
    unsafe {
        let flags = if std::env::var_os("POPULATE").is_some() { MAP_PRIVATE | MAP_POPULATE } else { MAP_PRIVATE };
        let p = mmap(std::ptr::null_mut(), len, PROT_READ, flags, f.as_raw_fd(), 0);
        if p.is_null() || p as isize == -1 {
            return std::ptr::null();
        }
        madvise(p, len, 2); // MADV_SEQUENTIAL
        REGIONS.lock().unwrap().push((p as usize, len));
        p
    }
}

/// Partitions shared by all threads: each thread claims blocks of slots with
/// one atomic add per block, so memory stays close to the real row count.
/// A thread's unfinished block is padded with a sentinel the reader skips.
pub struct Shared<T: Copy + 'static> {
    base: *mut T,
    cap: usize,
    cursors: Vec<std::sync::atomic::AtomicUsize>,
}
unsafe impl<T: Copy + Send + 'static> Send for Shared<T> {}
unsafe impl<T: Copy + Sync + 'static> Sync for Shared<T> {}

pub const BLOCK: usize = 64;

pub struct Writer<T: Copy + 'static> {
    pos: Vec<(u32, u32)>, // (next, end) in the current block of each bin
    pub spill: Vec<Vec<T>>,
}

impl<T: Copy + 'static> Shared<T> {
    pub fn new(nbins: usize, cap: usize) -> Self {
        let cap = (cap + BLOCK).next_multiple_of(BLOCK);
        let mem = alloc_huge::<T>(nbins * cap);
        Shared {
            base: mem.as_mut_ptr(),
            cap,
            cursors: (0..nbins).map(|_| std::sync::atomic::AtomicUsize::new(0)).collect(),
        }
    }
    pub fn writer(&self) -> Writer<T> {
        let n = self.cursors.len();
        Writer { pos: vec![(0, 0); n], spill: (0..n).map(|_| Vec::new()).collect() }
    }
    #[inline(always)]
    pub fn push(&self, w: &mut Writer<T>, b: usize, x: T) {
        let p = unsafe { w.pos.get_unchecked_mut(b) };
        if p.0 < p.1 {
            unsafe { self.base.add(b * self.cap + p.0 as usize).write(x) };
            p.0 += 1;
        } else {
            self.claim(w, b, x);
        }
    }
    #[cold]
    #[inline(never)]
    fn claim(&self, w: &mut Writer<T>, b: usize, x: T) {
        let s = self.cursors[b].fetch_add(BLOCK, std::sync::atomic::Ordering::Relaxed);
        if s < self.cap {
            unsafe { self.base.add(b * self.cap + s).write(x) };
            w.pos[b] = (s as u32 + 1, (s + BLOCK).min(self.cap) as u32);
        } else {
            w.pos[b] = (0, 0);
            w.spill[b].push(x);
        }
    }
    /// Pads the writer's open blocks with the sentinel.
    pub fn finish(&self, w: &mut Writer<T>, sentinel: T) {
        for (b, p) in w.pos.iter_mut().enumerate() {
            for i in p.0..p.1 {
                unsafe { self.base.add(b * self.cap + i as usize).write(sentinel) };
            }
            *p = (0, 0);
        }
    }
    /// The bin's claimed slots (may contain sentinels).
    pub fn bin(&self, b: usize) -> &[T] {
        let n = self.cursors[b].load(std::sync::atomic::Ordering::Relaxed).min(self.cap);
        unsafe { std::slice::from_raw_parts(self.base.add(b * self.cap), n) }
    }
}
