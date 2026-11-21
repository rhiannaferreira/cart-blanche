//! Open-addressing hash map from u64 keys (never u64::MAX) to u64 values.
pub struct Map64 {
    slots: Vec<[u64; 2]>,
    mask: usize,
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
        Map64 { slots: vec![[EMPTY, 0]; cap], mask: cap - 1 }
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

/// A zeroed, never-freed allocation backed by transparent huge pages when the
/// kernel allows it. Fewer TLB misses for big random-access tables.
pub fn alloc_huge<T: Copy>(n: usize) -> &'static mut [T] {
    let bytes = (n * std::mem::size_of::<T>()).max(1);
    let len = (bytes + (2 << 20) - 1) & !((2 << 20) - 1);
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + (2 << 20), PROT_RW, MAP_PRIVATE_ANON, -1, 0);
        assert!(!p.is_null() && p as isize != -1, "mmap failed");
        // align to 2 MiB so the kernel can use huge pages
        let a = ((p as usize + (2 << 20) - 1) & !((2 << 20) - 1)) as *mut u8;
        madvise(a, len, MADV_HUGEPAGE);
        std::slice::from_raw_parts_mut(a as *mut T, n)
    }
}

/// Read-mostly map u64 -> u64 for the link table: interleaved key/value slots
/// on huge pages, with a prefetch hook so lookups can overlap other work.
pub struct LinkMap {
    slots: &'static mut [[u64; 2]],
    mask: usize,
}

impl LinkMap {
    pub fn new(n: usize) -> Self {
        let cap = ((n * 3 / 2).max(16)).next_power_of_two();
        let slots = alloc_huge::<[u64; 2]>(cap);
        for s in slots.iter_mut() {
            s[0] = EMPTY;
        }
        LinkMap { slots, mask: cap - 1 }
    }
    #[inline(always)]
    fn home(&self, k: u64) -> usize {
        h(k) as usize & self.mask
    }
    pub fn insert(&mut self, k: u64, v: u64) {
        let mut i = self.home(k);
        loop {
            if self.slots[i][0] == EMPTY || self.slots[i][0] == k {
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
            let p = self.slots.as_ptr().add(self.home(k)) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }
    /// The value for k, or k itself when absent.
    #[inline(always)]
    pub fn map(&self, k: u64) -> u64 {
        let mut i = self.home(k);
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s[0] == k {
                return s[1];
            }
            if s[0] == EMPTY {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
}
