// Shopping Sessions pipeline. Implements rules.md exactly; no dependencies.
//
// Stages:
//   1. lookups: manifest, products, campaigns
//   2. links: parse, union-find, name each person by the byte-wise smallest ID
//   3. events: parse in parallel into partitions by event_id hash
//   4. dedup each event_id partition, map user -> person, repartition by person
//   5. clicks: parse in parallel into person partitions
//   6. per person partition: sort, sessionize, credit, assist, aggregate
//   7. write Parquet (own minimal writer)
use std::cmp::Ordering;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AO};

mod csv;
mod keys;
mod parquet;
mod table;
mod time;

use keys::{cmp_keys, key, Key};
use table::{Bins, LinkMap, Map64};
use time::{parse_ts, DateCache, DAY_MS};

// ---------------------------------------------------------------- threads
fn nthreads() -> usize {
    if let Ok(v) = std::env::var("NCPU") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
}

/// Runs f(state, item) for items 0..n over nt threads, returning each thread's state.
pub fn par<S: Send, F>(nt: usize, n: usize, init: impl Fn() -> S + Sync, f: F) -> Vec<S>
where
    F: Fn(&mut S, usize) + Sync,
{
    let next = AtomicUsize::new(0);
    let stat = std::env::var_os("PARSTAT").is_some();
    let t0 = std::time::Instant::now();
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|_| {
                sc.spawn(|| {
                    let mut s = init();
                    let mut worst = (0f64, 0usize);
                    loop {
                        let i = next.fetch_add(1, AO::Relaxed);
                        if i >= n {
                            break;
                        }
                        let t = std::time::Instant::now();
                        f(&mut s, i);
                        if stat {
                            let d = t.elapsed().as_secs_f64();
                            if d > worst.0 {
                                worst = (d, i);
                            }
                        }
                    }
                    (s, t0.elapsed().as_secs_f64(), worst)
                })
            })
            .collect();
        let r: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        if stat {
            let ends: Vec<String> = r.iter().map(|x| format!("{:.3}", x.1)).collect();
            let w = r.iter().map(|x| x.2).fold((0f64, 0), |a, b| if b.0 > a.0 { b } else { a });
            eprintln!("par n={n}: thread ends {} | worst task {} {:.3}s", ends.join(" "), w.1, w.0);
        }
        r.into_iter().map(|x| x.0).collect()
    })
}

// ---------------------------------------------------------------- csv
/// Splits one line into exactly N fields per the rules' dialect; false if malformed.
#[inline(always)]
pub(crate) fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    if line.is_empty() {
        return false;
    }
    let len = line.len();
    let mut n = 0;
    let mut start = 0;
    loop {
        let mut end = start;
        let mut quotes = 0u32;
        while end < len {
            let c = line[end];
            if c == b',' {
                break;
            }
            quotes += (c == b'"') as u32;
            end += 1;
        }
        if n >= N {
            return false;
        }
        let f = &line[start..end];
        if quotes == 0 {
            out[n] = f;
        } else if quotes == 2 && f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' {
            out[n] = &f[1..f.len() - 1];
        } else {
            return false;
        }
        n += 1;
        if end >= len {
            break;
        }
        start = end + 1;
    }
    n == N
}

#[inline(always)]
fn memchr_nl(d: &[u8], from: usize) -> usize {
    match d[from..].iter().position(|&x| x == b'\n') {
        Some(p) => p + from,
        None => d.len(),
    }
}

// ---------------------------------------------------------------- files
struct Src {
    // (Send/Sync: the mapping is read-only and lives until exit)
    f: File,
    map: *const u8, // whole-file read-only mapping, or null
    len: usize,
    data_start: usize, // first byte after the header line
}

unsafe impl Sync for Src {}
unsafe impl Send for Src {}

struct Range {
    src: usize,
    a: usize,
    b: usize,
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "csv").unwrap_or(false) && p.is_file() {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn open_sources(files: &[PathBuf]) -> Vec<Src> {
    let mut out = vec![];
    for p in files {
        let Ok(f) = File::open(p) else { continue };
        let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u8; 4096];
        let mut off = 0;
        let mut ds = None;
        while off < len {
            let n = f.read_at(&mut buf, off as u64).unwrap_or(0);
            if n == 0 {
                break;
            }
            if let Some(p) = buf[..n].iter().position(|&c| c == b'\n') {
                ds = Some(off + p + 1);
                break;
            }
            off += n;
        }
        let Some(ds) = ds else { continue };
        if ds >= len {
            continue;
        }
        let map = if std::env::var_os("NOMMAP").is_some() { std::ptr::null() } else { table::map_file(&f, len) };
        out.push(Src { f, map, len, data_start: ds });
    }
    out
}

fn ranges(srcs: &[Src], target: usize) -> Vec<Range> {
    let mut out = vec![];
    for (i, s) in srcs.iter().enumerate() {
        let mut a = s.data_start;
        while a < s.len {
            let b = (a + target).min(s.len);
            out.push(Range { src: i, a, b });
            a = b;
        }
    }
    out
}

/// Calls f with the fields of every well-formed row whose line starts in [r.a, r.b).
fn with_rows<const N: usize>(
    srcs: &[Src],
    r: &Range,
    buf: &mut Vec<u8>,
    pos: &mut Vec<u32>,
    f: impl FnMut(&[&[u8]; N]),
) {
    let s = &srcs[r.src];
    let from = if r.a > s.data_start { r.a - 1 } else { r.a };
    if !s.map.is_null() && r.b + (1 << 16) < s.len {
        // well inside the file: the mapping has all lines plus slack
        let d = unsafe { std::slice::from_raw_parts(s.map.add(from), r.b + (1 << 16) - 128 - from) };
        let end_rel = r.b - from;
        if d[end_rel - 1..].contains(&b'\n') {
            let p = if r.a > s.data_start { memchr_nl(d, 0) + 1 } else { 0 };
            let _ = pos;
            csv::rows_fast::<N>(d, p, end_rel, f);
            return;
        }
    }
    let mut want = (r.b + 4096).min(s.len) - from;
    loop {
        if buf.len() < want + 192 {
            buf.resize(want + 192, 0); // slack: SIMD scans and key() load past the end
        }
        let mut got = 0;
        while got < want {
            let n = s.f.read_at(&mut buf[got..want], (from + got) as u64).unwrap_or(0);
            if n == 0 {
                break;
            }
            got += n;
        }
        let d = &buf[..got];
        let end_rel = r.b - from; // lines starting before this offset are ours
        let complete = from + got >= s.len || d[end_rel - 1..].contains(&b'\n');
        if !complete {
            want = (want * 2).min(s.len - from);
            continue;
        }
        let p = if r.a > s.data_start { memchr_nl(d, 0) + 1 } else { 0 };
        let _ = pos;
        csv::rows_fast::<N>(d, p, end_rel.min(d.len()), f);
        return;
    }
}

fn read_small(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn small_rows<const N: usize>(path: &Path, mut f: impl FnMut(&[&[u8]; N])) {
    let d = read_small(path);
    let Some(nl) = d.iter().position(|&c| c == b'\n') else { return };
    let d = &d[nl + 1..];
    let mut p = 0;
    while p < d.len() {
        let e = memchr_nl(d, p);
        let mut fs: [&[u8]; N] = [&[]; N];
        if split(&d[p..e], &mut fs) {
            f(&fs);
        }
        p = e + 1;
    }
}

// ---------------------------------------------------------------- records
#[derive(Clone, Copy)]
struct RawEv {
    eid: Key,
    person: Key,
    ts: u32,     // ms since window_start
    ingest: u32, // s since window_start - 30 days, saturating
    price: u32,
    cat: u16,
    kind: u8, // 0 view 1 cart 2 purchase 255 invalid
}

/// A surviving event: pc = price << 8 | category (price 0 unless a purchase).
#[derive(Clone, Copy)]
struct Ev {
    person: Key,
    ts: u32,
    pc: u32,
}

#[derive(Clone, Copy)]
struct Click {
    person: Key,
    cid: Key,
    ts: u32, // ms since window_start - 7 days
    camp: u16,
}

const P1: usize = 4096;
const P2: usize = 2048;

#[inline(always)]
fn mix(x: u64) -> u64 {
    let mut x = x ^ (x >> 31);
    x = x.wrapping_mul(0x9E3779B97F4A7C15);
    x ^ (x >> 29)
}
#[inline(always)]
fn part1(k: Key) -> usize {
    (mix(k) >> 40) as usize % P1
}
#[inline(always)]
fn part2(k: Key) -> usize {
    (mix(k ^ 0x5555_5555_5555_5555) >> 40) as usize % P2
}

pub struct UserRow {
    pub person: Key,
    pub day: i32, // index from day0
    pub sessions: u32,
    pub events: u32,
    pub revenue: i64,
    pub top: i32, // -1 null
}

/// Products: "p<canonical decimal>" in a dense array, anything else in a map.
struct Products {
    dense: Vec<u16>, // u16::MAX = absent
    other: std::collections::HashMap<Vec<u8>, u16>,
    unknown: u16,
}
impl Products {
    fn canon(b: &[u8]) -> Option<usize> {
        if b.len() < 2 || b.len() > 8 || b[0] != b'p' || b[1] == b'0' {
            return None;
        }
        let mut v = 0usize;
        for &c in &b[1..] {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            v = v * 10 + d as usize;
        }
        Some(v)
    }
    #[inline(always)]
    fn get(&self, b: &[u8]) -> u16 {
        if let Some(v) = Self::canon(b) {
            if v < self.dense.len() && self.dense[v] != u16::MAX {
                return self.dense[v];
            }
            return self.unknown;
        }
        if self.other.is_empty() {
            return self.unknown;
        }
        *self.other.get(b).unwrap_or(&self.unknown)
    }
}

/// Campaigns: "c_<canonical decimal>" in a dense array, anything else in a map.
struct Campaigns {
    dense: Vec<u16>,
    other: std::collections::HashMap<Vec<u8>, u16>,
}
impl Campaigns {
    fn canon(b: &[u8]) -> Option<usize> {
        if b.len() < 3 || b.len() > 9 || b[0] != b'c' || b[1] != b'_' || b[2] == b'0' {
            return None;
        }
        let mut v = 0usize;
        for &c in &b[2..] {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            v = v * 10 + d as usize;
        }
        Some(v)
    }
    #[inline(always)]
    fn get(&self, b: &[u8]) -> Option<u16> {
        if let Some(v) = Self::canon(b) {
            if v < self.dense.len() && self.dense[v] != u16::MAX {
                return Some(self.dense[v]);
            }
            return None;
        }
        if self.other.is_empty() {
            return None;
        }
        self.other.get(b).copied()
    }
}

#[inline(always)]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > 10_000_000 {
            return None;
        }
    }
    if v >= 1 {
        Some(v as u32)
    } else {
        None
    }
}

fn main() {
    let t0 = std::time::Instant::now();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or("/data".into()));
    let outdir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or("/output".into()));
    let timing = std::env::var("TIMING").is_ok();
    let lap = |s: &str| {
        if timing {
            eprintln!("{:>8.3}s {}", t0.elapsed().as_secs_f64(), s);
        }
    };
    let nt = nthreads();

    // ---- manifest
    let man = String::from_utf8_lossy(&read_small(&data.join("manifest.json"))).to_string();
    let get = |name: &str| -> i64 {
        let i = man.find(name).expect("manifest field");
        let rest = &man[i + name.len()..];
        let q1 = rest.find('"').unwrap();
        let rest = &rest[q1 + 1..];
        let q2 = rest.find('"').unwrap();
        parse_ts(rest[..q2].as_bytes(), &mut DateCache::new()).expect("manifest ts")
    };
    let ws = get("\"window_start\"");
    let we = get("\"window_end\"");
    let cs = ws - 7 * DAY_MS;
    assert!(we - cs < u32::MAX as i64, "window too long");
    let ibase = ws - 15 * DAY_MS; // ingest ms base; rows that matter are ingested within [ws, we + 5 days]

    // ---- products -> categories, ids in byte-wise order of name ("unknown" competes)
    let prod_file = read_small(&data.join("products.csv"));
    let mut prod_raw: Vec<(&[u8], &[u8])> = Vec::with_capacity(prod_file.len() / 16);
    if let Some(nl) = prod_file.iter().position(|&c| c == b'\n') {
        let d = &prod_file[nl + 1..];
        let mut p = 0;
        while p < d.len() {
            let e = memchr_nl(d, p);
            let mut fs: [&[u8]; 2] = [&[]; 2];
            if split(&d[p..e], &mut fs) {
                prod_raw.push((fs[0], fs[1]));
            }
            p = e + 1;
        }
    }
    let mut cats: Vec<Vec<u8>> = vec![b"unknown".to_vec()];
    for (_, c) in &prod_raw {
        if !cats.iter().any(|x| x.as_slice() == *c) {
            cats.push(c.to_vec());
        }
    }
    cats.sort();
    let cat_id = |c: &[u8]| cats.binary_search_by(|x| x.as_slice().cmp(c)).unwrap() as u16;
    let mut products = Products { dense: vec![], other: Default::default(), unknown: cat_id(b"unknown") };
    for (p, c) in &prod_raw {
        let id = cat_id(c);
        match Products::canon(p) {
            Some(v) if v < 1 << 22 => {
                if products.dense.len() <= v {
                    products.dense.resize(v + 1, u16::MAX);
                }
                if products.dense[v] == u16::MAX {
                    products.dense[v] = id;
                }
            }
            _ => {
                products.other.entry(p.to_vec()).or_insert(id);
            }
        }
    }

    // ---- campaigns
    let mut camps: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    let mut campaigns = Campaigns { dense: vec![], other: Default::default() };
    small_rows::<2>(&data.join("campaigns.csv"), |f| {
        let id = camps.len() as u16;
        let fresh = match Campaigns::canon(f[0]) {
            Some(v) if v < 1 << 22 => {
                if campaigns.dense.len() <= v {
                    campaigns.dense.resize(v + 1, u16::MAX);
                }
                if campaigns.dense[v] == u16::MAX {
                    campaigns.dense[v] = id;
                    true
                } else {
                    false
                }
            }
            _ => {
                if campaigns.other.contains_key(f[0]) {
                    false
                } else {
                    campaigns.other.insert(f[0].to_vec(), id);
                    true
                }
            }
        };
        if fresh {
            camps.push((f[0].to_vec(), f[1].to_vec()));
        }
    });
    let organic = camps.len() as u16;
    assert!(cats.len() <= 256, "too many categories");
    lap("lookups");

    // links (union-find) run on their own thread while events are parsed
    let links_job = || {
    // ---- links
    let lsrc = open_sources(&list_csv(&data.join("links")));
    let lr = ranges(&lsrc, 1 << 20);
    let pairs: Vec<(Vec<u8>, Vec<u32>, Vec<(Key, Key)>)> = par(
        nt,
        lr.len(),
        || (Vec::new(), Vec::new(), Vec::new()),
        |(buf, pos, v), i| {
            with_rows::<2>(&lsrc, &lr[i], buf, pos, |f| {
                if !f[0].is_empty() && !f[1].is_empty() {
                    v.push((key(f[0]), key(f[1])));
                }
            });
        },
    );
    let total_pairs: usize = pairs.iter().map(|p| p.2.len()).sum();
    let mut idx = Map64::new(total_pairs + 16);
    let mut ids: Vec<Key> = Vec::with_capacity(total_pairs * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(total_pairs * 2);
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    let intern = |idx: &mut Map64, k: Key, ids: &mut Vec<Key>, parent: &mut Vec<u32>| -> u32 {
        let n = ids.len() as u64;
        let (v, new) = idx.get_or_insert(k, n);
        if new {
            ids.push(k);
            parent.push(n as u32);
        }
        v as u32
    };
    for (_, _, v) in &pairs {
        for (j, &(a, b)) in v.iter().enumerate() {
            if let Some(&(pa, pb)) = v.get(j + 16) {
                idx.prefetch(pa);
                idx.prefetch(pb);
            }
            let ia = intern(&mut idx, a, &mut ids, &mut parent);
            let ib = intern(&mut idx, b, &mut ids, &mut parent);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
        }
    }
    drop(pairs);
    let n = ids.len();
    let mut name: Vec<Key> = vec![0; n];
    let mut has: Vec<bool> = vec![false; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        parent[i] = r as u32;
        if !has[r] || cmp_keys(ids[i], name[r]) == Ordering::Less {
            name[r] = ids[i];
            has[r] = true;
        }
    }
    let nmap = (0..n).filter(|&i| name[parent[i] as usize] != ids[i]).count();
    if std::env::var_os("TIMING").is_some() { eprintln!("ids {n} nmap {nmap}"); }
    let mut linkmap = LinkMap::new(nmap + 16);
    for i in 0..n {
        if i + 16 < n {
            linkmap.prefetch_slot(ids[i + 16]);
        }
        let r = parent[i] as usize;
        if name[r] != ids[i] {
            linkmap.insert(ids[i], name[r]);
        }
    }
    drop(idx);
        linkmap
    };
    let (parsed, linkmap, ev_est) = std::thread::scope(|sc| {
        let h = sc.spawn(links_job);
    // ---- events: parse into event_id partitions
    let esrc = open_sources(&list_csv(&data.join("events")));
    let er = ranges(&esrc, 2 << 20);
    struct EvState {
        buf: Vec<u8>,
        pos: Vec<u32>,
        dc: DateCache,
        parts: Bins<RawEv>,
    }
    if let Ok(mode) = std::env::var("BENCH") {
        let t = std::time::Instant::now();
        if mode == "sum" {
            let r: Vec<u64> = par(nt, er.len(), || 0u64, |acc, i| {
                let rg = &er[i];
                let s = &esrc[rg.src];
                let d = unsafe { std::slice::from_raw_parts(s.map.add(rg.a), rg.b - rg.a) };
                let mut x = 0u64;
                for c in d.chunks_exact(64) {
                    x = x.wrapping_add(u64::from_le_bytes(c[0..8].try_into().unwrap()) ^ u64::from_le_bytes(c[32..40].try_into().unwrap()));
                }
                *acc = acc.wrapping_add(x);
            });
            eprintln!("bench sum {:.3}s {}", t.elapsed().as_secs_f64(), r.iter().fold(0u64, |a, b| a.wrapping_add(*b)));
            std::process::exit(0);
        }
        let r: Vec<(Vec<u8>, Vec<u32>, usize, DateCache, u64)> = par(nt, er.len(), || (Vec::new(), Vec::new(), 0usize, DateCache::new(), 0u64), |(buf, pos, c, dc, acc), i| {
            with_rows::<7>(&esrc, &er[i], buf, pos, |f| {
                *c += 1;
                if mode == "ts" { *acc = (*acc as u64).wrapping_add(parse_ts(f[2], dc).unwrap_or(0) as u64 ^ parse_ts(f[3], dc).unwrap_or(0) as u64); }
                if mode == "key" { *acc = (*acc as u64).wrapping_add(key(f[0]) ^ key(f[1])); }
            });
        });
        eprintln!("bench {mode}: {} rows {:.3}s {}", r.iter().map(|x| x.2).sum::<usize>(), t.elapsed().as_secs_f64(), r.iter().map(|x| x.4).fold(0, u64::wrapping_add));
        std::process::exit(0);
    }
    let wsz = (we - ws) as u64;
    let ev_bytes: usize = esrc.iter().map(|s| s.len - s.data_start).sum();
    let ev_est = ev_bytes / 70 + 1024; // rows, about 75 bytes each
    let ev_cap = ev_est * 11 / 10 / (nt * P1) + 64;
    let parsed: Vec<EvState> = par(
        nt,
        er.len(),
        || EvState { buf: Vec::new(), pos: Vec::new(), dc: DateCache::new(), parts: Bins::new(P1, ev_cap) },
        |st, i| {
            let parts = &mut st.parts;
            let dc = &mut st.dc;
            let emit = |o: RawEv, parts: &mut Bins<RawEv>| {
                parts.push(part1(o.eid), o);
            };
            with_rows::<7>(&esrc, &er[i], &mut st.buf, &mut st.pos, |f| {
                if f[0].is_empty() {
                    return;
                }
                let eid = key(f[0]);
                let ing_ms = time::parse_ingest(f[3], dc);
                let ingest = match ing_ms {
                    Some(m) => (m.wrapping_sub(ibase) as u64).min(u32::MAX as u64 - 1) as u32,
                    None => u32::MAX,
                };
                let mut r = RawEv { eid, person: 0, ts: 0, ingest, price: 0, cat: 0, kind: 255 };
                'v: {
                    if f[1].is_empty() || f[5].is_empty() {
                        break 'v;
                    }
                    let uk = key(f[1]);
                    let Some(ts) = parse_ts(f[2], dc) else { break 'v };
                    let rel = ts.wrapping_sub(ws) as u64;
                    if rel >= wsz {
                        break 'v;
                    }
                    let kind = match f[4] {
                        b"view" => 0,
                        b"cart" => 1,
                        b"purchase" => 2,
                        _ => break 'v,
                    };
                    if kind == 2 {
                        match parse_price(f[6]) {
                            Some(p) => r.price = p,
                            None => break 'v,
                        }
                    }
                    if let Some(m) = ing_ms {
                        if m - ts > DAY_MS {
                            break 'v;
                        }
                    }
                    r.person = uk; // resolved when it leaves the ring
                    r.ts = rel as u32;
                    if kind == 2 {
                        r.cat = products.get(f[5]);
                    }
                    r.kind = kind;
                }
                emit(r, parts);
            });
            table::fence();
        },
    );
        let mut parsed = parsed;
        for st in parsed.iter_mut() {
            st.parts.finish();
        }
        (parsed, h.join().unwrap(), ev_est)
    });
    let person_of = |u: Key| -> Key { linkmap.map(u) };
    lap("links + events parsed");

    // ---- clicks, and dedup per event_id partition with repartition by person,
    // as one pool of tasks: click ranges first, then the many small partitions
    let csrc = open_sources(&list_csv(&data.join("clicks")));
    let cr = ranges(&csrc, 2 << 20);
    let csz = (we - cs) as u64;
    let ck_bytes: usize = csrc.iter().map(|s| s.len - s.data_start).sum();
    let ck_cap = (ck_bytes / 45 + 1024) * 11 / 10 / (nt * P2) + 64;
    struct DState {
        out: Bins<Ev>,
        surv: Vec<Ev>,
        tbl: Vec<RawEv>,
        buf: Vec<u8>,
        pos: Vec<u32>,
        dc: DateCache,
        parts: Bins<Click>,
    }
    let ncr = cr.len();
    let dstates: Vec<DState> = par(
        nt,
        ncr + P1,
        || DState {
            out: Bins::<Ev>::new(P2, ev_est / (nt * P2) + 64),
            surv: Vec::new(),
            tbl: Vec::new(),
            buf: Vec::new(),
            pos: Vec::new(),
            dc: DateCache::new(),
            parts: Bins::<Click>::new(P2, ck_cap),
        },
        |st, task| {
            if task < ncr {
                let i = task;
                let (buf, pos, dc, parts) = (&mut st.buf, &mut st.pos, &mut st.dc, &mut st.parts);
                // clicks wait in a small ring so the link-table prefetch can land
                const RING: usize = 16;
                let mut ring = [Click { person: 0, cid: 0, ts: 0, camp: 0 }; RING];
                let mut rn = 0usize;
                let out = |c: Click, parts: &mut Bins<Click>| {
                    let person = person_of(c.person);
                    parts.push(part2(person), Click { person, ..c });
                };
                with_rows::<4>(&csrc, &cr[i], buf, pos, |f| {
                    if f[0].is_empty() || f[1].is_empty() {
                        return;
                    }
                    let Some(ts) = parse_ts(f[3], dc) else { return };
                    let rel = ts.wrapping_sub(cs) as u64;
                    if rel >= csz {
                        return;
                    }
                    let Some(camp) = campaigns.get(f[2]) else { return };
                    let uk = key(f[1]);
                    linkmap.prefetch(uk);
                    let slot = rn % RING;
                    if rn >= RING {
                        out(ring[slot], parts);
                    }
                    ring[slot] = Click { person: uk, cid: key(f[0]), ts: rel as u32, camp };
                    rn += 1;
                });
                for j in rn.saturating_sub(RING)..rn {
                    out(ring[j % RING], parts);
                }
                return;
            }
            let p = task - ncr;
            let (out, surv, tbl) = (&mut st.out, &mut st.surv, &mut st.tbl);
            let total: usize = parsed.iter().map(|s| s.parts.len(p)).sum();
            surv.clear();
            surv.reserve(total);
            // open addressing on event_id, each slot holding the copy with the
            // smallest ingest so far (the first one read on a tie)
            let cap = (total + total / 2 + 16).next_power_of_two();
            let mask = cap - 1;
            const NONE: RawEv = RawEv { eid: u64::MAX, person: 0, ts: 0, ingest: 0, price: 0, cat: 0, kind: 255 };
            tbl.clear();
            tbl.resize(cap, NONE);
            for s in parsed.iter() {
                let (x, y) = s.parts.bin(p);
                for r in x.iter().chain(y.iter()) {
                    let mut i = (mix(r.eid) as usize) & mask;
                    loop {
                        let t = unsafe { tbl.get_unchecked_mut(i) };
                        if t.eid == u64::MAX {
                            *t = *r;
                            break;
                        }
                        if t.eid == r.eid {
                            if r.ingest < t.ingest {
                                *t = *r;
                            }
                            break;
                        }
                        i = (i + 1) & mask;
                    }
                }
            }
            // winners -> person partitions, waiting in a ring so the link-table
            // prefetch can land
            const RING: usize = 32;
            let mut ring = [Ev { person: 0, ts: 0, pc: 0 }; RING];
            let mut rn = 0usize;
            for r in tbl.iter() {
                if r.kind != 255 && r.eid != u64::MAX {
                    let pc = if r.kind == 2 { (r.price << 8) | r.cat as u32 } else { 0 };
                    linkmap.prefetch(r.person);
                    let slot = rn % RING;
                    if rn >= RING {
                        let mut e = ring[slot];
                        e.person = person_of(e.person);
                        out.push(part2(e.person), e);
                    }
                    ring[slot] = Ev { person: r.person, ts: r.ts, pc };
                    rn += 1;
                }
            }
            for j in rn.saturating_sub(RING)..rn {
                let mut e = ring[j % RING];
                e.person = person_of(e.person);
                out.push(part2(e.person), e);
            }
            let _ = &surv;
            table::fence();
        },
    );
    let mut deduped: Vec<Bins<Ev>> = Vec::with_capacity(nt);
    let mut clicks: Vec<Bins<Click>> = Vec::with_capacity(nt);
    for mut st in dstates {
        st.out.finish();
        deduped.push(st.out);
        clicks.push(st.parts);
    }
    std::mem::forget(parsed);
    lap("clicks parsed, events deduped");

    // ---- sessionize + attribute per person partition
    let day0 = ws.div_euclid(DAY_MS);
    let ndays = ((we - 1).div_euclid(DAY_MS) - day0 + 1) as usize;
    let ncamp = camps.len() + 1;
    let ncat = cats.len();
    const WEEK: i64 = 7 * DAY_MS;
    const GAP: u32 = 1_800_000;
    let off = ws - cs; // event ts + off = click-relative ts
    let wsday = ws - day0 * DAY_MS; // ms from day0 midnight to window_start

    struct SState {
        camp: Vec<[i64; 2]>, // sessions | purchasing << 32, revenue
        asst: Vec<u32>,
        users: Vec<Vec<UserRow>>,
        evs: Vec<Ev>,
        cks: Vec<Click>,
        ev2: Vec<Ev>,
        ck2: Vec<Click>,
        elid: Vec<u32>,
        clid: Vec<u32>,
        eoff: Vec<u32>,
        coff: Vec<u32>,
        lm: Map64,
    }
    let order: Vec<usize> = {
        let mut o: Vec<(usize, usize)> =
            (0..P2).map(|p| (deduped.iter().map(|d| d.len(p)).sum::<usize>(), p)).collect();
        o.sort_unstable_by(|a, b| b.cmp(a));
        o.into_iter().map(|x| x.1).collect()
    };
    let noassist = std::env::var_os("NOASSIST").is_some();
    let sres: Vec<SState> = par(
        nt,
        P2,
        || SState { camp: vec![[0i64; 2]; ncamp * ndays], asst: vec![0u32; ncamp * ndays], users: vec![], evs: Vec::new(), cks: Vec::new(), ev2: Vec::new(), ck2: Vec::new(), elid: Vec::new(), clid: Vec::new(), eoff: Vec::new(), coff: Vec::new(), lm: Map64::new(16) },
        |st, oi| {
            let b = order[oi];
            let mut parts: Vec<&[Ev]> = Vec::with_capacity(2 * deduped.len());
            let mut nev = 0;
            for d in &deduped {
                let (x, y) = d.bin(b);
                for z in [x, y] {
                    if !z.is_empty() {
                        parts.push(z);
                        nev += z.len();
                    }
                }
            }
            let cks = &mut st.cks;
            cks.clear();
            for d in &clicks {
                let (x, y) = d.bin(b);
                cks.extend_from_slice(x);
                cks.extend_from_slice(y);
            }
            // group by person with a counting sort, then order each person's rows by time
            let lm = &mut st.lm;
            lm.reset(nev / 8 + 64);
            let elid = &mut st.elid;
            elid.clear();
            let mut nl = 0u64;
            for e in parts.iter().flat_map(|z| z.iter()) {
                let (l, _) = lm.get_or_insert(e.person, nl);
                nl += (l == nl) as u64;
                elid.push(l as u32);
            }
            let nl = nl as usize;
            let eoff = &mut st.eoff;
            eoff.clear();
            eoff.resize(nl + 1, 0);
            for &l in elid.iter() {
                eoff[l as usize + 1] += 1;
            }
            for l in 0..nl {
                eoff[l + 1] += eoff[l];
            }
            let ev2 = &mut st.ev2;
            ev2.clear();
            ev2.reserve(nev);
            unsafe { ev2.set_len(nev) };
            {
                let mut cur: Vec<u32> = eoff[..nl].to_vec();
                for (e, &l) in parts.iter().flat_map(|z| z.iter()).zip(elid.iter()) {
                    let c = &mut cur[l as usize];
                    ev2[*c as usize] = *e;
                    *c += 1;
                }
            }
            for l in 0..nl {
                let g = &mut ev2[eoff[l] as usize..eoff[l + 1] as usize];
                if g.len() <= 24 {
                    for i in 1..g.len() {
                        let x = g[i];
                        let mut j = i;
                        while j > 0 && g[j - 1].ts > x.ts {
                            g[j] = g[j - 1];
                            j -= 1;
                        }
                        g[j] = x;
                    }
                } else {
                    g.sort_unstable_by_key(|e| e.ts);
                }
            }
            // clicks of these persons, grouped the same way; others can credit nothing
            let coff = &mut st.coff;
            coff.clear();
            coff.resize(nl + 1, 0);
            let clid = &mut st.clid;
            clid.clear();
            for c in cks.iter() {
                let l = lm.get(c.person).map(|v| v as u32).unwrap_or(u32::MAX);
                clid.push(l);
                if l != u32::MAX {
                    coff[l as usize + 1] += 1;
                }
            }
            for l in 0..nl {
                coff[l + 1] += coff[l];
            }
            let ck2 = &mut st.ck2;
            ck2.clear();
            ck2.reserve(coff[nl] as usize);
            unsafe { ck2.set_len(coff[nl] as usize) };
            {
                let mut cur: Vec<u32> = coff[..nl].to_vec();
                for (c, &l) in cks.iter().zip(clid.iter()) {
                    if l != u32::MAX {
                        let x = &mut cur[l as usize];
                        ck2[*x as usize] = *c;
                        *x += 1;
                    }
                }
            }
            for l in 0..nl {
                let g = &mut ck2[coff[l] as usize..coff[l + 1] as usize];
                if g.len() > 1 {
                    g.sort_unstable_by(|x, y| x.ts.cmp(&y.ts).then_with(|| cmp_keys(y.cid, x.cid)));
                }
            }
            let evs = &*ev2;
            let cks = &*ck2;
            let camp_acc = &mut st.camp;
            let asst = &mut st.asst;
            let mut users: Vec<UserRow> = vec![];
            let mut catrev = vec![0i64; ncat];
            let mut touched: Vec<u16> = vec![];
            let mut s_start: Vec<i64> = vec![]; // click-relative
            let mut s_day: Vec<u32> = vec![];
            let mut pcl: Vec<(u16, u32)> = vec![];
            let mut day_first: Vec<u32> = vec![];
            let mut mark: Vec<u32> = vec![0; ncamp];
            let mut sid = 0u32;

            let flush = |row: &UserRow, catrev: &mut Vec<i64>, touched: &mut Vec<u16>, users: &mut Vec<UserRow>| {
                let mut top = -1i32;
                let mut bestv = 0i64;
                for &c in touched.iter() {
                    let v = catrev[c as usize];
                    if v > bestv || (v == bestv && (c as i32) < top) {
                        bestv = v;
                        top = c as i32;
                    }
                    catrev[c as usize] = 0;
                }
                touched.clear();
                users.push(UserRow { top, ..*row });
            };

            for l in 0..nl {
                let (ei, ej) = (eoff[l] as usize, eoff[l + 1] as usize);
                let person = evs[ei].person;
                let pc = &cks[coff[l] as usize..coff[l + 1] as usize];
                s_start.clear();
                s_day.clear();
                let mut row = UserRow { person, day: -1, sessions: 0, events: 0, revenue: 0, top: -1 };
                let mut k = 0usize;
                let mut kl = 0usize;
                let small_pc = pc.len() <= 48;
                let mut i = ei;
                while i < ej {
                    let start = evs[i].ts;
                    let mut last = start;
                    let mut j = i;
                    let mut rev = 0i64;
                    while j < ej && evs[j].ts - last <= GAP {
                        let e = &evs[j];
                        last = e.ts;
                        rev += (e.pc >> 8) as i64;
                        j += 1;
                    }
                    let dayi = ((start as i64 + wsday) / DAY_MS) as u32;
                    if row.sessions > 0 && row.day != dayi as i32 {
                        flush(&row, &mut catrev, &mut touched, &mut users);
                        row.sessions = 0;
                        row.events = 0;
                        row.revenue = 0;
                    }
                    let purch = rev > 0;
                    if purch {
                        for e in &evs[i..j] {
                            if e.pc != 0 {
                                let c = (e.pc & 0xFF) as usize;
                                if catrev[c] == 0 {
                                    touched.push(c as u16);
                                }
                                catrev[c] += (e.pc >> 8) as i64;
                            }
                        }
                    }
                    row.day = dayi as i32;
                    row.sessions += 1;
                    row.events += (j - i) as u32;
                    row.revenue += rev;

                    let sc = start as i64 + off;
                    while k < pc.len() && (pc[k].ts as i64) <= sc {
                        k += 1;
                    }
                    let camp = if k > 0 && sc - pc[k - 1].ts as i64 <= WEEK { pc[k - 1].camp } else { organic };
                    if small_pc {
                        // assists: campaigns with a click in [start - 7 days, start], each once
                        while kl < k && (pc[kl].ts as i64) < sc - WEEK {
                            kl += 1;
                        }
                        sid = sid.wrapping_add(1);
                        for c in &pc[kl..k] {
                            let m = unsafe { mark.get_unchecked_mut(c.camp as usize) };
                            if *m != sid {
                                *m = sid;
                                asst[c.camp as usize * ndays + dayi as usize] += 1;
                            }
                        }
                    }
                    let a = &mut camp_acc[camp as usize * ndays + dayi as usize];
                    a[0] += 1 | ((purch as i64) << 32);
                    a[1] += rev;
                    s_start.push(sc);
                    s_day.push(dayi);
                    i = j;
                }
                flush(&row, &mut catrev, &mut touched, &mut users);

                if !pc.is_empty() && !small_pc && !noassist {
                    // day_first[d] = index of the first session on day d or later
                    day_first.clear();
                    let mut si = 0usize;
                    for d in 0..=ndays {
                        while si < s_day.len() && (s_day[si] as usize) < d {
                            si += 1;
                        }
                        day_first.push(si as u32);
                    }
                    pcl.clear();
                    pcl.extend(pc.iter().map(|c| (c.camp, c.ts)));
                    pcl.sort_unstable();
                    let mut q = 0;
                    while q < pcl.len() {
                        let camp = pcl[q].0;
                        let a = pcl[q].1 as i64;
                        let mut e = a + WEEK;
                        q += 1;
                        while q < pcl.len() && pcl[q].0 == camp && (pcl[q].1 as i64) <= e {
                            e = e.max(pcl[q].1 as i64 + WEEK);
                            q += 1;
                        }
                        let lo = s_start.partition_point(|&s| s < a);
                        let hi = s_start.partition_point(|&s| s <= e);
                        if lo < hi {
                            let (d0, d1) = (s_day[lo] as usize, s_day[hi - 1] as usize);
                            let row = &mut asst[camp as usize * ndays..camp as usize * ndays + ndays];
                            for d in d0..=d1 {
                                let n = (day_first[d + 1] as usize).min(hi) - (day_first[d] as usize).max(lo);
                                row[d] += n as u32;
                            }
                        }
                    }
                }
            }
            st.users.push(users);
        },
    );
    std::mem::forget(deduped);
    std::mem::forget(clicks);
    lap("sessionized");

    let mut camp_acc = vec![[0i64; 4]; ncamp * ndays];
    let mut users: Vec<Vec<UserRow>> = Vec::with_capacity(P2);
    for s in sres {
        for (x, y) in camp_acc.iter_mut().zip(s.asst.iter()) {
            x[3] += *y as i64;
        }
        for (x, y) in camp_acc.iter_mut().zip(s.camp.iter()) {
            x[0] += y[0] & 0xFFFF_FFFF;
            x[1] += y[0] >> 32;
            x[2] += y[1];
        }
        users.extend(s.users);
    }
    parquet::write_all(&outdir, nt, &users, &camp_acc, &camps, ndays, day0, &cats);
    lap("written");
    if timing {
        if let Ok(t) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            for l in t.lines().filter(|l| l.starts_with("AnonHuge") || l.starts_with("Rss")) {
                eprintln!("{l}");
            }
        }
    }
    if std::env::var_os("NORELEASE").is_none() {
        table::release_all(nt);
    }
    std::process::exit(0); // skip freeing the rest
}
