//! A minimal Parquet writer: one row group, one PLAIN uncompressed data page
//! (v1) per column, Thrift compact metadata. Enough for the two output tables.
use crate::keys::unpack;
use crate::UserRow;
use std::io::Write;
use std::path::Path;

// ---------------------------------------------------------------- thrift compact
struct T {
    b: Vec<u8>,
    last: Vec<i16>,
}

const I32: u8 = 5;
const I64: u8 = 6;
const BIN: u8 = 8;
const LIST: u8 = 9;
const STRUCT: u8 = 12;

impl T {
    fn new() -> Self {
        T { b: vec![], last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn fh(&mut self, id: i16, ty: u8) {
        let last = *self.last.last().unwrap();
        let d = id - last;
        if d > 0 && d <= 15 {
            self.b.push(((d as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.fh(id, I32);
        self.zz(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.fh(id, I64);
        self.zz(v);
    }
    fn strf(&mut self, id: i16, s: &[u8]) {
        self.fh(id, BIN);
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
    fn begin(&mut self, id: i16) {
        self.fh(id, STRUCT);
        self.last.push(0);
    }
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.b.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, ty: u8, n: usize) {
        self.fh(id, LIST);
        if n < 15 {
            self.b.push(((n as u8) << 4) | ty);
        } else {
            self.b.push(0xF0 | ty);
            self.varint(n as u64);
        }
    }
}

// ---------------------------------------------------------------- columns
#[derive(Clone, Copy)]
enum Kind {
    Str,     // BYTE_ARRAY, UTF8
    OptStr,  // optional BYTE_ARRAY, UTF8
    Date,    // INT32, DATE
    Int64,   // INT64
}

struct Col {
    name: &'static str,
    kind: Kind,
    data: Vec<u8>,     // PLAIN values
    defs: Vec<bool>,   // for OptStr: true = present
}

impl Col {
    fn new(name: &'static str, kind: Kind, cap: usize) -> Self {
        Col { name, kind, data: Vec::with_capacity(cap), defs: vec![] }
    }
    #[inline(always)]
    fn s(&mut self, v: &[u8]) {
        self.data.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.data.extend_from_slice(v);
        if let Kind::OptStr = self.kind {
            self.defs.push(true);
        }
    }
    #[inline(always)]
    fn null(&mut self) {
        self.defs.push(false);
    }
    #[inline(always)]
    fn i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    fn i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
}

/// RLE runs of a 1-bit definition level, prefixed by the 4-byte length.
fn def_levels(defs: &[bool]) -> Vec<u8> {
    let mut body = T::new();
    let mut i = 0;
    while i < defs.len() {
        let v = defs[i];
        let mut j = i;
        while j < defs.len() && defs[j] == v {
            j += 1;
        }
        body.varint(((j - i) as u64) << 1);
        body.b.push(v as u8);
        i = j;
    }
    let mut out = (body.b.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body.b);
    out
}

fn write_file(path: &Path, cols: &[Col], nrows: usize) {
    let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).expect("create"));
    let mut pos: u64 = 0;
    let put = |f: &mut std::io::BufWriter<std::fs::File>, b: &[u8], pos: &mut u64| {
        f.write_all(b).expect("write");
        *pos += b.len() as u64;
    };
    put(&mut f, b"PAR1", &mut pos);
    let mut metas = vec![]; // (offset, total size)
    for c in cols {
        let levels = match c.kind {
            Kind::OptStr => def_levels(&c.defs),
            _ => vec![],
        };
        let size = (levels.len() + c.data.len()) as i32;
        let mut h = T::new();
        h.i32f(1, 0); // DATA_PAGE
        h.i32f(2, size);
        h.i32f(3, size);
        h.begin(5);
        h.i32f(1, nrows as i32);
        h.i32f(2, 0); // PLAIN
        h.i32f(3, 3); // RLE
        h.i32f(4, 3); // RLE
        h.end();
        h.b.push(0);
        let off = pos;
        put(&mut f, &h.b, &mut pos);
        put(&mut f, &levels, &mut pos);
        put(&mut f, &c.data, &mut pos);
        metas.push((off, pos - off));
    }
    // FileMetaData
    let mut m = T::new();
    m.i32f(1, 1);
    m.list(2, STRUCT, cols.len() + 1);
    m.begin_elem();
    m.strf(4, b"schema");
    m.i32f(5, cols.len() as i32);
    m.end();
    for c in cols {
        m.begin_elem();
        let (ty, rep) = match c.kind {
            Kind::Str => (6, 0),
            Kind::OptStr => (6, 1),
            Kind::Date => (1, 0),
            Kind::Int64 => (2, 0),
        };
        m.i32f(1, ty);
        m.i32f(3, rep);
        m.strf(4, c.name.as_bytes());
        match c.kind {
            Kind::Str | Kind::OptStr => {
                m.i32f(6, 0); // UTF8
                m.begin(10);
                m.begin(1); // STRING
                m.end();
                m.end();
            }
            Kind::Date => {
                m.i32f(6, 6); // DATE
                m.begin(10);
                m.begin(6); // DATE
                m.end();
                m.end();
            }
            Kind::Int64 => {}
        }
        m.end();
    }
    m.i64f(3, nrows as i64);
    m.list(4, STRUCT, 1);
    m.begin_elem();
    m.list(1, STRUCT, cols.len());
    let total: u64 = metas.iter().map(|x| x.1).sum();
    for (c, &(off, sz)) in cols.iter().zip(metas.iter()) {
        m.begin_elem();
        m.i64f(2, off as i64);
        m.begin(3);
        let ty = match c.kind {
            Kind::Str | Kind::OptStr => 6,
            Kind::Date => 1,
            Kind::Int64 => 2,
        };
        m.i32f(1, ty);
        m.list(2, I32, 2);
        m.zz(0); // PLAIN
        m.zz(3); // RLE
        m.list(3, BIN, 1);
        m.varint(c.name.len() as u64);
        m.b.extend_from_slice(c.name.as_bytes());
        m.i32f(4, 0); // UNCOMPRESSED
        m.i64f(5, nrows as i64);
        m.i64f(6, sz as i64);
        m.i64f(7, sz as i64);
        m.i64f(9, off as i64);
        m.end();
        m.end();
    }
    m.i64f(2, total as i64);
    m.i64f(3, nrows as i64);
    m.end();
    m.strf(6, b"cart-blanche");
    m.b.push(0);
    put(&mut f, &m.b, &mut pos);
    put(&mut f, &(m.b.len() as u32).to_le_bytes(), &mut pos);
    put(&mut f, b"PAR1", &mut pos);
    f.flush().expect("flush");
}

#[allow(clippy::too_many_arguments)]
pub fn write_all(
    outdir: &Path,
    nt: usize,
    users: &[Vec<UserRow>],
    camp_acc: &[[i64; 4]],
    camps: &[(Vec<u8>, Vec<u8>)],
    ndays: usize,
    day0: i64,
    cats: &[Vec<u8>],
) {
    let ud = outdir.join("user_daily");
    let cd = outdir.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir");
    std::fs::create_dir_all(&cd).expect("mkdir");

    let nfiles = (3 * nt).max(1).min(users.len().max(1));
    let per = users.len().div_ceil(nfiles).max(1);
    let groups: Vec<&[Vec<UserRow>]> = users.chunks(per).collect();
    let ng = groups.len();
    crate::par(nt, ng + 1, || (), |_, gi| {
        // campaign_daily first, then the user files
        let gi = if gi == 0 { ng } else { gi - 1 };
        if gi == ng {
            // campaign_daily
            let mut c = [
                Col::new("campaign_id", Kind::Str, 0),
                Col::new("channel", Kind::Str, 0),
                Col::new("day", Kind::Date, 0),
                Col::new("sessions", Kind::Int64, 0),
                Col::new("purchasing_sessions", Kind::Int64, 0),
                Col::new("revenue_cents", Kind::Int64, 0),
                Col::new("assisted_sessions", Kind::Int64, 0),
            ];
            let mut n = 0;
            for ci in 0..=camps.len() {
                for d in 0..ndays {
                    let x = camp_acc[ci * ndays + d];
                    if x[0] == 0 && x[3] == 0 {
                        continue;
                    }
                    if ci < camps.len() {
                        c[0].s(&camps[ci].0);
                        c[1].s(&camps[ci].1);
                    } else {
                        c[0].s(b"organic");
                        c[1].s(b"organic");
                    }
                    c[2].i32((day0 + d as i64) as i32);
                    c[3].i64(x[0]);
                    c[4].i64(x[1]);
                    c[5].i64(x[2]);
                    c[6].i64(x[3]);
                    n += 1;
                }
            }
            write_file(&cd.join("part-0.parquet"), &c, n);
            return;
        }
        let g = groups[gi];
        let n: usize = g.iter().map(|v| v.len()).sum();
        let mut c = [
            Col::new("user_id", Kind::Str, n * 14),
            Col::new("day", Kind::Date, n * 4),
            Col::new("sessions", Kind::Int64, n * 8),
            Col::new("events", Kind::Int64, n * 8),
            Col::new("revenue_cents", Kind::Int64, n * 8),
            Col::new("top_category", Kind::OptStr, n * 6),
        ];
        c[5].defs.reserve(n);
        let mut namebuf = vec![];
        for v in g {
            for r in v {
                namebuf.clear();
                namebuf.extend_from_slice(&unpack(r.person));
                c[0].s(&namebuf);
                c[1].i32((day0 + r.day as i64) as i32);
                c[2].i64(r.sessions as i64);
                c[3].i64(r.events as i64);
                c[4].i64(r.revenue);
                if r.top < 0 {
                    c[5].null();
                } else {
                    c[5].s(&cats[r.top as usize]);
                }
            }
        }
        write_file(&ud.join(format!("part-{gi}.parquet")), &c, n);
    });
}
