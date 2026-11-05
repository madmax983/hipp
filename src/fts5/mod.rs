//! FTS5 full-text search (v0.16, leaf `fts5`): the pin's `fts5`
//! module, byte-compatible with its shadow tables.
//!
//! This module is the storage core, a pure function of the rows it is
//! given (the session layer, `session::fts5`, reads and writes them
//! through the pager):
//!
//! - **`%_data`** — the structure record (rowid 10: cookie, levels,
//!   segments, write counter; the V2 form with origins for
//!   `contentless_delete=1`), the averages record (rowid 1), and the
//!   segment b-trees: leaf pages of `pgsz` bytes (4-byte header, terms
//!   prefix-compressed, doclists of rowid deltas and position lists,
//!   the page-index footer), doclist-index pages, keyed as the pin's
//!   `FTS5_SEGMENT_ROWID` / `FTS5_DLIDX_ROWID`.
//! - **The pending terms** ([`Hash`], `fts5_hash.c`): what a
//!   transaction wrote, in the pin's in-memory doclist format, flushed
//!   into a new level-0 segment exactly where the pin flushes
//!   ([`flush`], `fts5FlushOneHash`).
//! - **Segment writing** ([`Writer`], `Fts5SegWriter`): both of the
//!   pin's page-filling rules (the flush path splits a position list
//!   at the last varint that fits, the merge path at the first that
//!   overflows), the `%_idx` separator terms and doclist indexes.
//! - **Merging** (`fts5IndexMergeLevel`, automerge, crisismerge,
//!   usermerge, optimize, promotion, incremental merges that trim
//!   their inputs): the structure after a write sequence is the pin's.
//! - **Reading** ([`decode_seg`], [`Reader`]): segments decoded to
//!   terms and doclists; a term's doclist merged over every segment
//!   and the pending terms, newest first, deletes applied.
//!
//! Configuration is `config`, tokenizers `tok`, the query language
//! `expr`, the auxiliary functions `aux`.

pub mod aux;
pub mod config;
pub mod expr;
pub mod tok;
mod unicode;

use crate::arena::AVec;

/// The averages record's rowid (`FTS5_AVERAGES_ROWID`).
pub const AVERAGES_ROWID: i64 = 1;
/// The structure record's rowid (`FTS5_STRUCTURE_ROWID`).
pub const STRUCTURE_ROWID: i64 = 10;
/// `FTS5_MAX_SEGMENT`.
const MAX_SEGMENT: u32 = 2000;
/// `FTS5_MIN_DLIDX_SIZE`.
const MIN_DLIDX_SIZE: u32 = 4;
/// `FTS5_WORK_UNIT`.
const WORK_UNIT: u64 = 64;
/// `FTS5_OPT_WORK_UNIT`.
const OPT_WORK_UNIT: i64 = 1000;
/// `FTS5_MAX_LEVEL`.
const MAX_LEVEL: usize = 64;
/// `sizeof(Fts5HashEntry)` on the pin's 64-bit build: what each
/// pending term costs before its key and doclist.
const HASH_ENTRY: i64 = 48;

/// The index's level of detail (`detail=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Full,
    Column,
    None,
}

/// Corrupt index data (the pin's `FTS5_CORRUPT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corrupt;

/// `FTS5_SEGMENT_ROWID(segid, pgno)`.
pub fn seg_rowid(segid: u32, pgno: u32) -> i64 {
    (i64::from(segid) << 37).wrapping_add(i64::from(pgno))
}

/// `FTS5_DLIDX_ROWID(segid, height, pgno)`.
fn dlidx_rowid(segid: u32, height: usize, pgno: u32) -> i64 {
    (i64::from(segid) << 37)
        .wrapping_add(1 << 36)
        .wrapping_add((height as i64) << 31)
        .wrapping_add(i64::from(pgno))
}

/// Append SQLite's varint of `v`.
pub fn pv(out: &mut AVec<u8>, v: u64) {
    crate::record::encode_varint(v, out);
}

/// Read a varint at `*i` (advancing it); `None` past the end.
pub fn gv(a: &[u8], i: &mut usize) -> Option<u64> {
    let (v, n) = crate::record::decode_varint::<()>(a.get(*i..)?).ok()?;
    *i += n;
    Some(v)
}

/// The length of the varint at `a[i..]` (1 past the end).
fn vlen(a: &[u8], i: usize) -> usize {
    let mut j = i;
    match gv(a, &mut j) {
        Some(_) => j - i,
        None => 1,
    }
}

fn get_u16(a: &[u8], i: usize) -> usize {
    match (a.get(i), a.get(i + 1)) {
        (Some(x), Some(y)) => (usize::from(*x) << 8) | usize::from(*y),
        _ => 0,
    }
}

fn put_u16(a: &mut AVec<u8>, i: usize, v: usize) {
    if let Some(s) = a.as_mut_slice().get_mut(i..i + 2) {
        s[0] = (v >> 8) as u8;
        s[1] = v as u8;
    }
}

fn copy(b: &[u8]) -> AVec<u8> {
    let mut v = AVec::new();
    v.extend_from_slice(b);
    v
}

// ---------------------------------------------------------------
// The structure record
// ---------------------------------------------------------------

/// One segment (`Fts5StructureSegment`).
#[derive(Debug, Clone, Default)]
pub struct Seg {
    pub id: u32,
    pub first: u32,
    pub last: u32,
    pub o1: u64,
    pub o2: u64,
    pub npgt: u32,
    pub nentt: u64,
    pub nent: u64,
}

impl Seg {
    fn size(&self) -> i64 {
        1 + i64::from(self.last) - i64::from(self.first)
    }
}

/// One level (`Fts5StructureLevel`).
#[derive(Debug, Clone, Default)]
pub struct Level {
    pub merge: usize,
    pub segs: AVec<Seg>,
}

/// The structure record (`Fts5Structure`).
#[derive(Debug, Clone, Default)]
pub struct Structure {
    pub cookie: u32,
    pub levels: AVec<Level>,
    pub nseg: usize,
    pub wc: u64,
    /// `nOriginCntr` (0: a V1 record).
    pub origin: u64,
}

const V2: [u8; 4] = [0xFF, 0x00, 0x00, 0x01];

impl Structure {
    /// The empty structure `sqlite3Fts5IndexReinit` writes.
    pub fn empty(contentless_delete: bool) -> Structure {
        Structure {
            origin: u64::from(contentless_delete),
            ..Structure::default()
        }
    }

    /// `fts5StructureDecode`.
    pub fn decode(a: &[u8]) -> Result<Structure, Corrupt> {
        let c = a.get(..4).ok_or(Corrupt)?;
        let mut s = Structure {
            cookie: u32::from_be_bytes([c[0], c[1], c[2], c[3]]),
            ..Structure::default()
        };
        let mut i = 4;
        let v2 = a.get(4..8) == Some(&V2[..]);
        if v2 {
            i = 8;
        }
        let nlevel = gv(a, &mut i).ok_or(Corrupt)?;
        let mut nseg = gv(a, &mut i).ok_or(Corrupt)?;
        if nlevel > u64::from(MAX_SEGMENT) || nseg > u64::from(MAX_SEGMENT) {
            return Err(Corrupt);
        }
        s.nseg = nseg as usize;
        s.wc = gv(a, &mut i).ok_or(Corrupt)?;
        let mut maxo = 0u64;
        for il in 0..nlevel {
            let merge = gv(a, &mut i).ok_or(Corrupt)? as usize;
            let total = gv(a, &mut i).ok_or(Corrupt)?;
            if total < merge as u64 || total > nseg {
                return Err(Corrupt);
            }
            nseg -= total;
            let mut lvl = Level {
                merge,
                segs: AVec::new(),
            };
            for _ in 0..total {
                let mut g = || gv(a, &mut i).ok_or(Corrupt);
                let mut sg = Seg {
                    id: g()? as u32,
                    first: g()? as u32,
                    last: g()? as u32,
                    ..Seg::default()
                };
                if v2 {
                    sg.o1 = g()?;
                    sg.o2 = g()?;
                    sg.npgt = g()? as u32;
                    sg.nentt = g()?;
                    sg.nent = g()?;
                    maxo = maxo.max(sg.o2);
                }
                if sg.last < sg.first {
                    return Err(Corrupt);
                }
                lvl.segs.push(sg);
            }
            if il > 0 && s.levels.as_slice().last().is_some_and(|p| p.merge > 0) && total == 0 {
                return Err(Corrupt);
            }
            if il + 1 == nlevel && merge > 0 {
                return Err(Corrupt);
            }
            s.levels.push(lvl);
        }
        if nseg != 0 {
            return Err(Corrupt);
        }
        if v2 {
            s.origin = maxo + 1;
        }
        Ok(s)
    }

    /// `fts5StructureWrite`'s record.
    pub fn encode(&self) -> AVec<u8> {
        let mut b = copy(&self.cookie.to_be_bytes());
        if self.origin > 0 {
            b.extend_from_slice(&V2);
        }
        pv(&mut b, self.levels.len() as u64);
        pv(&mut b, self.nseg as u64);
        pv(&mut b, self.wc);
        for l in self.levels.iter() {
            pv(&mut b, l.merge as u64);
            pv(&mut b, l.segs.len() as u64);
            for s in l.segs.iter() {
                pv(&mut b, u64::from(s.id));
                pv(&mut b, u64::from(s.first));
                pv(&mut b, u64::from(s.last));
                if self.origin > 0 {
                    pv(&mut b, s.o1);
                    pv(&mut b, s.o2);
                    pv(&mut b, u64::from(s.npgt));
                    pv(&mut b, s.nentt);
                    pv(&mut b, s.nent);
                }
            }
        }
        b
    }

    fn level(&mut self, i: usize) -> &mut Level {
        while self.levels.len() <= i {
            self.levels.push(Level::default());
        }
        &mut self.levels[i]
    }

    /// `fts5AllocateSegid`: the smallest unused segment id.
    fn alloc_segid(&self) -> Option<u32> {
        if self.nseg >= MAX_SEGMENT as usize {
            return None;
        }
        let mut id = 1u32;
        loop {
            if !self
                .levels
                .iter()
                .any(|l| l.segs.iter().any(|s| s.id == id))
            {
                return Some(id);
            }
            id += 1;
        }
    }

    /// Every segment, newest first (level 0's last first).
    pub fn newest_first(&self) -> AVec<Seg> {
        let mut out = AVec::new();
        for l in self.levels.iter() {
            for s in l.segs.iter().rev() {
                out.push(s.clone());
            }
        }
        out
    }

    /// `fts5StructurePromote`.
    fn promote(&mut self, il: usize) {
        let Some(lvl) = self.levels.as_slice().get(il) else {
            return;
        };
        let Some(seg) = lvl.segs.as_slice().last() else {
            return;
        };
        let szseg = seg.size();
        let mut promote = None;
        let mut szpromote = 0;
        let mut it = il;
        while it > 0 {
            it -= 1;
            let t = &self.levels[it];
            if !t.segs.is_empty() {
                let szmax = t.segs.iter().map(|s| s.size()).max().unwrap_or(0);
                if szmax >= szseg {
                    promote = Some(it);
                    szpromote = szmax;
                }
                break;
            }
        }
        let (ip, sz) = match promote {
            Some(p) => (p, szpromote),
            None => (il, szseg),
        };
        self.promote_to(ip, sz);
    }

    /// `fts5StructurePromoteTo`.
    fn promote_to(&mut self, ip: usize, szpromote: i64) {
        if self.levels[ip].merge != 0 {
            return;
        }
        for il in ip + 1..self.levels.len() {
            if self.levels[il].merge != 0 {
                return;
            }
            while let Some(s) = self.levels[il].segs.as_slice().last().cloned() {
                if s.size() > szpromote {
                    return;
                }
                self.levels[il].segs.pop();
                self.levels[ip].segs.insert_at(0, s);
            }
        }
    }
}

// ---------------------------------------------------------------
// The block store and the write log
// ---------------------------------------------------------------

/// A write to the shadow tables, in the order the pin makes it.
#[derive(Debug, Clone)]
pub enum Op {
    /// `REPLACE INTO %_data VALUES(rowid, block)`.
    Put(i64, AVec<u8>),
    /// `DELETE FROM %_data WHERE id>=lo AND id<=hi`.
    Del(i64, i64),
    /// `INSERT INTO %_idx VALUES(segid, term, pgno)`.
    IdxPut(u32, AVec<u8>, i64),
    /// `DELETE FROM %_idx WHERE segid=?`.
    IdxDel(u32),
}

/// The `%_data` rows an operation has read (the whole table: the
/// session loads it) and the writes it made, applied in order.
#[derive(Debug, Default)]
pub struct Store {
    blocks: AVec<(i64, AVec<u8>)>,
    pub log: AVec<Op>,
}

impl Store {
    /// Add a row read from `%_data` (rows arrive in rowid order).
    pub fn load(&mut self, id: i64, b: AVec<u8>) {
        match self.find(id) {
            Ok(i) => self.blocks[i].1 = b,
            Err(i) => self.blocks.insert_at(i, (id, b)),
        }
    }

    fn find(&self, id: i64) -> Result<usize, usize> {
        self.blocks.as_slice().binary_search_by_key(&id, |b| b.0)
    }

    /// A store holding only an empty structure record (to read a
    /// [`Hash`] alone).
    pub fn with_empty() -> Store {
        let mut s = Store::default();
        s.load(STRUCTURE_ROWID, Structure::empty(false).encode());
        s
    }

    /// Row `id`'s block.
    pub fn get(&self, id: i64) -> Option<&[u8]> {
        self.find(id).ok().map(|i| self.blocks[i].1.as_slice())
    }

    fn put(&mut self, id: i64, b: AVec<u8>) {
        self.log.push(Op::Put(id, b.clone()));
        self.load(id, b);
    }

    fn del(&mut self, lo: i64, hi: i64) {
        self.log.push(Op::Del(lo, hi));
        let a = self.find(lo).unwrap_or_else(|i| i);
        let mut b = a;
        while b < self.blocks.len() && self.blocks[b].0 <= hi {
            b += 1;
        }
        if b > a {
            let mut rest = AVec::new();
            for (k, x) in self.blocks.iter().enumerate() {
                if k < a || k >= b {
                    rest.push(x.clone());
                }
            }
            self.blocks = rest;
        }
    }

    /// The structure record (corrupt when missing or malformed).
    pub fn structure(&self) -> Result<Structure, Corrupt> {
        Structure::decode(self.get(STRUCTURE_ROWID).ok_or(Corrupt)?)
    }

    /// `fts5StructureWrite`.
    pub fn write_structure(&mut self, s: &Structure) {
        self.put(STRUCTURE_ROWID, s.encode());
    }

    /// `sqlite3Fts5IndexSetAverages`.
    pub fn write_averages(&mut self, b: AVec<u8>) {
        self.put(AVERAGES_ROWID, b);
    }

    /// `sqlite3Fts5IndexReinit` after `DELETE FROM %_data`: the empty
    /// averages and structure records.
    pub fn reinit(&mut self, cookie: u32, contentless_delete: bool) {
        self.blocks.clear();
        self.put(AVERAGES_ROWID, AVec::new());
        let mut s = Structure::empty(contentless_delete);
        s.cookie = cookie;
        self.write_structure(&s);
    }

    /// `fts5DataRemoveSegment`.
    fn remove_segment(&mut self, s: &Seg) {
        self.del(seg_rowid(s.id, 0), seg_rowid(s.id + 1, 0) - 1);
        if s.npgt > 0 {
            self.del(
                seg_rowid(s.id + (1 << 16), 0),
                seg_rowid(s.id + (1 << 16), s.npgt - 1),
            );
        }
        self.log.push(Op::IdxDel(s.id));
    }
}

// ---------------------------------------------------------------
// The pending terms (fts5_hash.c)
// ---------------------------------------------------------------

#[derive(Debug, Clone)]
struct HEntry {
    /// Index byte (`'0'` main, `'1'...` prefix indexes) and token.
    key: AVec<u8>,
    data: AVec<u8>,
    rowid: i64,
    /// Offset of the reserved poslist-size byte (0: none open).
    isz: usize,
    del: bool,
    content: bool,
    col: i32,
    pos: i32,
}

/// The pending terms of a transaction (`Fts5Hash`), sorted by key.
#[derive(Debug, Clone)]
pub struct Hash {
    e: AVec<HEntry>,
    detail: Detail,
    /// `nPendingData`.
    pub bytes: i64,
}

impl Hash {
    pub fn new(detail: Detail) -> Hash {
        Hash {
            e: AVec::new(),
            detail,
            bytes: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.e.is_empty()
    }

    pub fn detail(&self) -> Detail {
        self.detail
    }

    fn cost(e: &HEntry) -> i64 {
        HASH_ENTRY + e.key.len() as i64 + e.data.len() as i64
    }

    /// `fts5HashAddPoslistSize` into `out` (the entry's own data, or a
    /// copy): returns the finished data.
    fn finish(detail: Detail, e: &HEntry, data: &mut AVec<u8>) {
        if e.isz == 0 {
            return;
        }
        if detail == Detail::None {
            if e.del {
                data.push(0);
                if e.content {
                    data.push(0);
                }
            }
        } else {
            let nsz = data.len().saturating_sub(e.isz + 1);
            let npos = (nsz as u64) * 2 + u64::from(e.del);
            let mut v = AVec::new();
            pv(&mut v, npos);
            data.replace_range(e.isz, 1, v.as_slice());
        }
    }

    fn close(detail: Detail, e: &mut HEntry) {
        let mut d = core::mem::take(&mut e.data);
        Self::finish(detail, e, &mut d);
        e.data = d;
        e.isz = 0;
        e.del = false;
        e.content = false;
    }

    /// `sqlite3Fts5HashWrite`: token `tok` of index `byte` at column
    /// `col` (negative: a delete), position `pos`, of row `rowid`.
    pub fn write(&mut self, rowid: i64, col: i32, pos: i32, byte: u8, tok: &[u8]) {
        let detail = self.detail;
        let mut key = AVec::new();
        key.push(byte);
        key.extend_from_slice(tok);
        let at = self
            .e
            .as_slice()
            .binary_search_by(|x| x.key.as_slice().cmp(key.as_slice()));
        let mut bnew = detail == Detail::Full;
        let i = match at {
            Ok(i) => {
                self.bytes -= Self::cost(&self.e[i]);
                i
            }
            Err(i) => {
                let mut data = AVec::new();
                pv(&mut data, rowid as u64);
                let isz = data.len();
                let mut e = HEntry {
                    key,
                    data,
                    rowid,
                    isz,
                    del: false,
                    content: false,
                    col: 0,
                    pos: 0,
                };
                if detail != Detail::None {
                    e.data.push(0);
                    e.col = if detail == Detail::Full { 0 } else { -1 };
                }
                // The size field's offset counts from the entry's
                // start on the pin; only its position in `data`
                // matters here (never 0: a rowid precedes it).
                self.e.insert_at(i, e);
                i
            }
        };
        let e = &mut self.e[i];
        if rowid != e.rowid {
            Self::close(detail, e);
            pv(&mut e.data, (rowid as u64).wrapping_sub(e.rowid as u64));
            e.rowid = rowid;
            bnew = true;
            e.isz = e.data.len();
            if detail != Detail::None {
                e.data.push(0);
                e.col = if detail == Detail::Full { 0 } else { -1 };
                e.pos = 0;
            }
        }
        if col >= 0 {
            if detail == Detail::None {
                e.content = true;
            } else {
                let mut p = pos;
                if col != e.col {
                    if detail == Detail::Full {
                        e.data.push(1);
                        pv(&mut e.data, col as u64);
                        e.col = col;
                        e.pos = 0;
                    } else {
                        bnew = true;
                        e.col = col;
                        p = col;
                    }
                }
                if bnew {
                    pv(
                        &mut e.data,
                        (p.wrapping_sub(e.pos).wrapping_add(2)) as u32 as u64,
                    );
                    e.pos = p;
                }
            }
        } else {
            e.del = true;
        }
        self.bytes += Self::cost(&self.e[i]);
    }

    /// The finished doclist of entry `i`.
    fn doclist(&self, i: usize) -> AVec<u8> {
        let e = &self.e[i];
        let mut d = e.data.clone();
        Self::finish(self.detail, e, &mut d);
        d
    }

    /// Every (key, finished doclist), in key order.
    pub fn entries(&self) -> AVec<(AVec<u8>, AVec<u8>)> {
        let mut out = AVec::new();
        for i in 0..self.e.len() {
            out.push((self.e[i].key.clone(), self.doclist(i)));
        }
        out
    }
}

// ---------------------------------------------------------------
// Segment writing (Fts5SegWriter)
// ---------------------------------------------------------------

#[derive(Debug, Default)]
struct DlWriter {
    pgno: u32,
    prev_valid: bool,
    prev: i64,
    buf: AVec<u8>,
}

/// A segment being written.
struct Writer {
    segid: u32,
    pgsz: usize,
    buf: AVec<u8>,
    pgidx: AVec<u8>,
    prev_pgidx: usize,
    pgno: u32,
    term: AVec<u8>,
    first_term: bool,
    first_rowid_page: bool,
    first_rowid_doclist: bool,
    prev_rowid: i64,
    n_empty: u32,
    n_leaf: i64,
    bt_page: u32,
    btterm: AVec<u8>,
    dl: AVec<DlWriter>,
}

fn prefix_compress(old: &[u8], new: &[u8]) -> usize {
    old.iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

impl Writer {
    /// `fts5WriteInit`.
    fn new(segid: u32, pgsz: usize) -> Writer {
        let mut dl = AVec::new();
        dl.push(DlWriter::default());
        Writer {
            segid,
            pgsz,
            buf: copy(&[0, 0, 0, 0]),
            pgidx: AVec::new(),
            prev_pgidx: 0,
            pgno: 1,
            term: AVec::new(),
            first_term: true,
            first_rowid_page: false,
            first_rowid_doclist: false,
            prev_rowid: 0,
            n_empty: 0,
            n_leaf: 0,
            bt_page: 1,
            btterm: AVec::new(),
            dl,
        }
    }

    fn room(&self) -> usize {
        self.buf.len() + self.pgidx.len()
    }

    /// `fts5WriteFlushLeaf`.
    fn flush_leaf(&mut self, st: &mut Store) {
        let n = self.buf.len();
        put_u16(&mut self.buf, 2, n);
        if self.first_term {
            // fts5WriteBtreeNoTerm
            if self.first_rowid_page && !self.dl[0].buf.is_empty() {
                pv(&mut self.dl[0].buf, 0);
            }
            self.n_empty += 1;
        } else {
            let p = core::mem::take(&mut self.pgidx);
            self.buf.extend_from_slice(p.as_slice());
        }
        let page = core::mem::replace(&mut self.buf, copy(&[0, 0, 0, 0]));
        st.put(seg_rowid(self.segid, self.pgno), page);
        self.pgidx.clear();
        self.prev_pgidx = 0;
        self.pgno += 1;
        self.n_leaf += 1;
        self.first_term = true;
        self.first_rowid_page = true;
    }

    /// `fts5WriteFlushDlidx` / `fts5WriteDlidxClear`.
    fn flush_dlidx(&mut self, st: &mut Store) -> bool {
        let flag = !self.dl[0].buf.is_empty() && self.n_empty >= MIN_DLIDX_SIZE;
        for i in 0..self.dl.len() {
            if self.dl[i].buf.is_empty() {
                break;
            }
            if flag {
                let b = self.dl[i].buf.clone();
                st.put(dlidx_rowid(self.segid, i, self.dl[i].pgno), b);
            }
            self.dl[i].buf.clear();
            self.dl[i].prev_valid = false;
        }
        self.n_empty = 0;
        flag
    }

    /// `fts5WriteFlushBtree`.
    fn flush_btree(&mut self, st: &mut Store) {
        if self.bt_page == 0 {
            return;
        }
        let flag = self.flush_dlidx(st);
        st.log.push(Op::IdxPut(
            self.segid,
            self.btterm.clone(),
            i64::from(flag) + (i64::from(self.bt_page) << 1),
        ));
        self.bt_page = 0;
    }

    /// `fts5WriteAppendTerm`.
    fn append_term(&mut self, st: &mut Store, term: &[u8]) {
        let nterm = term.len();
        if self.room() + nterm + 2 >= self.pgsz && self.buf.len() > 4 {
            self.flush_leaf(st);
        }
        pv(&mut self.pgidx, (self.buf.len() - self.prev_pgidx) as u64);
        self.prev_pgidx = self.buf.len();
        let nprefix;
        if self.first_term {
            nprefix = 0;
            if self.pgno != 1 {
                let n = if self.term.is_empty() {
                    nterm
                } else {
                    (1 + prefix_compress(self.term.as_slice(), term)).min(nterm)
                };
                self.flush_btree(st);
                self.btterm = copy(term.get(..n).unwrap_or(term));
                self.bt_page = self.pgno;
            }
        } else {
            nprefix = prefix_compress(self.term.as_slice(), term);
            pv(&mut self.buf, nprefix as u64);
        }
        pv(&mut self.buf, (nterm - nprefix) as u64);
        self.buf
            .extend_from_slice(term.get(nprefix..).unwrap_or(&[]));
        self.term = copy(term);
        self.first_term = false;
        self.first_rowid_page = false;
        self.first_rowid_doclist = true;
        self.dl[0].pgno = self.pgno;
    }

    /// `fts5WriteDlidxAppend`.
    fn dlidx_append(&mut self, st: &mut Store, rowid: i64) {
        let mut i = 0;
        let mut done = false;
        while !done {
            while self.dl.len() <= i {
                self.dl.push(DlWriter::default());
            }
            if self.dl[i].buf.len() >= self.pgsz {
                self.dl[i].buf[0] = 0x01;
                let b = self.dl[i].buf.clone();
                st.put(dlidx_rowid(self.segid, i, self.dl[i].pgno), b);
                while self.dl.len() < i + 2 {
                    self.dl.push(DlWriter::default());
                }
                if self.dl[i + 1].buf.is_empty() {
                    let first = {
                        let b = self.dl[i].buf.as_slice();
                        let mut k = 1;
                        let _ = gv(b, &mut k);
                        gv(b, &mut k).unwrap_or(0) as i64
                    };
                    let pg = self.dl[i].pgno;
                    let up = &mut self.dl[i + 1];
                    up.pgno = pg;
                    pv(&mut up.buf, 0);
                    pv(&mut up.buf, u64::from(pg));
                    pv(&mut up.buf, first as u64);
                    up.prev_valid = true;
                    up.prev = first;
                }
                self.dl[i].buf.clear();
                self.dl[i].prev_valid = false;
                self.dl[i].pgno += 1;
            } else {
                done = true;
            }
            let val = if self.dl[i].prev_valid {
                (rowid as u64).wrapping_sub(self.dl[i].prev as u64)
            } else {
                let pg = if i == 0 {
                    self.pgno
                } else {
                    self.dl[i - 1].pgno
                };
                pv(&mut self.dl[i].buf, u64::from(!done));
                pv(&mut self.dl[i].buf, u64::from(pg));
                rowid as u64
            };
            pv(&mut self.dl[i].buf, val);
            self.dl[i].prev_valid = true;
            self.dl[i].prev = rowid;
            i += 1;
        }
    }

    /// `fts5WriteAppendRowid`.
    fn append_rowid(&mut self, st: &mut Store, rowid: i64) {
        if self.room() >= self.pgsz {
            self.flush_leaf(st);
        }
        if self.first_rowid_page {
            let n = self.buf.len();
            put_u16(&mut self.buf, 0, n);
            self.dlidx_append(st, rowid);
        }
        if self.first_rowid_doclist || self.first_rowid_page {
            pv(&mut self.buf, rowid as u64);
        } else {
            pv(
                &mut self.buf,
                (rowid as u64).wrapping_sub(self.prev_rowid as u64),
            );
        }
        self.prev_rowid = rowid;
        self.first_rowid_doclist = false;
        self.first_rowid_page = false;
    }

    /// `fts5WriteAppendPoslistData` (the merge path's split: at the
    /// first varint boundary at or past the page's end).
    fn append_poslist(&mut self, st: &mut Store, data: &[u8]) {
        let mut a = data;
        while self.room() + a.len() >= self.pgsz {
            let nreq = self.pgsz as isize - self.room() as isize;
            let mut ncopy = 0usize;
            while (ncopy as isize) < nreq && ncopy < a.len() {
                ncopy += vlen(a, ncopy);
            }
            let ncopy = ncopy.min(a.len());
            self.buf.extend_from_slice(&a[..ncopy]);
            a = &a[ncopy..];
            self.flush_leaf(st);
        }
        if !a.is_empty() {
            self.buf.extend_from_slice(a);
        }
    }

    /// `fts5WriteFinish`: the last leaf page number (0: none).
    fn finish(&mut self, st: &mut Store) -> u32 {
        if self.buf.len() > 4 {
            self.flush_leaf(st);
        }
        let last = self.pgno - 1;
        if self.pgno > 1 {
            self.flush_btree(st);
        }
        last
    }
}

/// `fts5PoslistPrefix`: the bytes of whole varints of `a` that fit
/// in `nmax` (at least one varint).
fn poslist_prefix(a: &[u8], nmax: usize) -> usize {
    let mut ret = vlen(a, 0);
    if ret < nmax {
        loop {
            if ret >= a.len() {
                break;
            }
            let i = vlen(a, ret);
            if ret + i > nmax {
                break;
            }
            ret += i;
        }
    }
    ret.min(a.len())
}

/// The runtime merge settings (`%_config`).
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub pgsz: usize,
    pub automerge: i64,
    pub crisismerge: i64,
    pub usermerge: i64,
    pub hashsize: i64,
    /// `deletemerge`, and whether the table is `contentless_delete`
    /// (`fts5IndexFindDeleteMerge` only runs then).
    pub deletemerge: i64,
    pub cdel: bool,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            pgsz: 4050,
            automerge: 4,
            crisismerge: 16,
            usermerge: 4,
            hashsize: 1024 * 1024,
            deletemerge: 10,
            cdel: false,
        }
    }
}

/// `fts5FlushOneHash`, then automerge and crisismerge: the pending
/// terms become a level-0 segment; `nrows` is `nPendingRow`.
pub fn flush(
    st: &mut Store,
    h: &Hash,
    t: &Tuning,
    detail: Detail,
    nrows: u64,
) -> Result<(), Corrupt> {
    let mut s = st.structure()?;
    let mut last = 0u32;
    if !h.is_empty() {
        let segid = s.alloc_segid().ok_or(Corrupt)?;
        let pgsz = t.pgsz;
        let mut w = Writer::new(segid, pgsz);
        for (key, dl) in h.entries().iter() {
            let dl = dl.as_slice();
            w.append_term(st, key.as_slice());
            if pgsz > w.room() + dl.len() {
                w.buf.extend_from_slice(dl);
            } else {
                let mut rowid = 0i64;
                let mut prev = 0i64;
                let mut off = 0usize;
                while off < dl.len() {
                    let d = gv(dl, &mut off).ok_or(Corrupt)?;
                    rowid = rowid.wrapping_add(d as i64);
                    if w.first_rowid_page {
                        let n = w.buf.len();
                        put_u16(&mut w.buf, 0, n);
                        pv(&mut w.buf, rowid as u64);
                        w.first_rowid_page = false;
                        w.dlidx_append(st, rowid);
                    } else {
                        pv(&mut w.buf, (rowid as u64).wrapping_sub(prev as u64));
                    }
                    prev = rowid;
                    if detail == Detail::None {
                        if dl.get(off) == Some(&0) {
                            w.buf.push(0);
                            off += 1;
                            if dl.get(off) == Some(&0) {
                                w.buf.push(0);
                                off += 1;
                            }
                        }
                        if w.room() >= pgsz {
                            w.flush_leaf(st);
                        }
                    } else {
                        let mut k = off;
                        let sz = gv(dl, &mut k).ok_or(Corrupt)?;
                        let ncopy = (k - off) + (sz >> 1) as usize;
                        let pl = dl.get(off..off + ncopy).ok_or(Corrupt)?;
                        if w.room() + ncopy <= pgsz {
                            w.buf.extend_from_slice(pl);
                        } else {
                            let mut ip = 0;
                            loop {
                                let space = pgsz.saturating_sub(w.room());
                                let n = if ncopy - ip <= space {
                                    ncopy - ip
                                } else {
                                    poslist_prefix(&pl[ip..], space)
                                };
                                w.buf.extend_from_slice(&pl[ip..ip + n]);
                                ip += n;
                                if w.room() >= pgsz {
                                    w.flush_leaf(st);
                                }
                                if ip >= ncopy {
                                    break;
                                }
                            }
                        }
                        off += ncopy;
                    }
                }
            }
        }
        last = w.finish(st);
        if last > 0 {
            let origin = s.origin;
            let lvl = s.level(0);
            let mut sg = Seg {
                id: segid,
                first: 1,
                last,
                ..Seg::default()
            };
            if origin > 0 {
                sg.o1 = origin;
                sg.o2 = origin;
                sg.nent = nrows;
            }
            lvl.segs.push(sg);
            if origin > 0 {
                s.origin += 1;
            }
            s.nseg += 1;
            s.promote(0);
        }
    }
    automerge(st, &mut s, t, detail, i64::from(last))?;
    crisismerge(st, &mut s, t, detail)?;
    st.write_structure(&s);
    Ok(())
}

/// `fts5IndexAutomerge`.
fn automerge(
    st: &mut Store,
    s: &mut Structure,
    t: &Tuning,
    detail: Detail,
    nleaf: i64,
) -> Result<(), Corrupt> {
    if t.automerge <= 0 || s.levels.is_empty() {
        return Ok(());
    }
    let w = s.wc;
    let nleaf = nleaf.max(0) as u64;
    let nwork = (w.wrapping_add(nleaf) / WORK_UNIT).wrapping_sub(w / WORK_UNIT);
    s.wc = w.wrapping_add(nleaf);
    let nrem = (WORK_UNIT * nwork * s.levels.len() as u64) as i64;
    merge(st, s, nrem, t.automerge as usize, t, detail)?;
    Ok(())
}

/// `fts5IndexCrisismerge`.
fn crisismerge(
    st: &mut Store,
    s: &mut Structure,
    t: &Tuning,
    detail: Detail,
) -> Result<(), Corrupt> {
    let ncrisis = t.crisismerge.max(0) as usize;
    if s.levels.is_empty() {
        return Ok(());
    }
    let mut il = 0;
    while il < s.levels.len() && s.levels[il].segs.len() >= ncrisis {
        merge_level(st, s, il, None, t, detail)?;
        s.promote(il + 1);
        il += 1;
    }
    Ok(())
}

/// `fts5IndexFindDeleteMerge`: on a contentless_delete table, the
/// level whose tombstoned share of entries is largest and at least
/// `deletemerge` percent.
fn delete_merge_level(s: &Structure, t: &Tuning) -> Option<usize> {
    if !t.cdel || t.deletemerge <= 0 {
        return None;
    }
    let (mut ret, mut nbest) = (None, 0i64);
    for (il, l) in s.levels.iter().enumerate() {
        let (mut ne, mut nt) = (0i64, 0i64);
        for sg in l.segs.iter() {
            ne = ne.wrapping_add(sg.nent as i64);
            nt = nt.wrapping_add(sg.nentt as i64);
        }
        if ne > 0 {
            let pct = i64::from((nt.wrapping_mul(100) / ne) as i32);
            if pct >= t.deletemerge && pct > nbest {
                ret = Some(il);
                nbest = pct;
            }
        }
        if l.merge > 0 {
            break;
        }
    }
    ret
}

/// `fts5IndexMerge`: returns whether any merge was made.
fn merge(
    st: &mut Store,
    s: &mut Structure,
    npg: i64,
    nmin: usize,
    t: &Tuning,
    detail: Detail,
) -> Result<bool, Corrupt> {
    let mut nrem = npg;
    let mut nmin = nmin;
    let mut ret = false;
    while nrem > 0 {
        let mut best = 0usize;
        let mut nbest = 0usize;
        for (il, l) in s.levels.iter().enumerate() {
            if l.merge > 0 {
                if l.merge > nbest {
                    best = il;
                    nbest = nmin;
                }
                break;
            }
            if l.segs.len() > nbest {
                nbest = l.segs.len();
                best = il;
            }
        }
        if nbest < nmin {
            match delete_merge_level(s, t) {
                Some(l) => best = l,
                None => break,
            }
        }
        merge_level(st, s, best, Some(&mut nrem), t, detail)?;
        ret = true;
        if s.levels[best].merge == 0 {
            s.promote(best + 1);
        }
        if nmin == 1 {
            nmin = 2;
        }
    }
    Ok(ret)
}

/// Segment `sg`'s tombstone hash pages (`FTS5_TOMBSTONE_ROWID`;
/// none unless the pin deleted rows of a `contentless_delete` table).
fn tomb_pages(st: &Store, sg: &Seg) -> Result<AVec<AVec<u8>>, Corrupt> {
    let mut out = AVec::new();
    for pg in 0..sg.npgt {
        out.push(copy(
            st.get(seg_rowid(sg.id + (1 << 16), pg)).ok_or(Corrupt)?,
        ));
    }
    Ok(out)
}

/// `fts5MultiIterIsDeleted` / `fts5IndexTombstoneQuery`: whether a
/// segment's tombstone hash (`pages`) holds `rowid` (v0.16 review fix:
/// a pin-deleted row of a contentless_delete table was returned by
/// queries and kept by merges).
fn tomb_hit(pages: &[AVec<u8>], rowid: i64) -> bool {
    let r = rowid as u64;
    let n = pages.len() as u64;
    let Some(p) = r.checked_rem(n).and_then(|i| pages.get(i as usize)) else {
        return false;
    };
    let p = p.as_slice();
    if r == 0 {
        return p.get(1).is_some_and(|b| *b != 0);
    }
    let sz = if p.first() == Some(&4) { 4 } else { 8 };
    let nslot = if p.len() > 16 { (p.len() - 8) / sz } else { 1 };
    let mut i = ((r / n) % nslot as u64) as usize;
    let mut ncollide = nslot;
    loop {
        let Some(k) = p.get(8 + i * sz..8 + i * sz + sz) else {
            return false;
        };
        let v = k.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b));
        if v == 0 {
            return false;
        }
        if v == r {
            return true;
        }
        if ncollide == 0 {
            return false;
        }
        ncollide -= 1;
        i = (i + 1) % nslot;
    }
}

/// One doclist entry of a segment.
#[derive(Debug, Clone)]
pub struct Entry {
    pub rowid: i64,
    pub del: bool,
    /// Position-list bytes (detail=none: 0 or 1, "has content").
    pub npos: usize,
    pub data: AVec<u8>,
}

/// One term of a segment, where the pin's segment iterator finds it.
#[derive(Debug, Clone)]
pub struct TermRec {
    pub term: AVec<u8>,
    /// Page holding the term, the offset just past its bytes there,
    /// the next term's offset on that page (or `nn + 1`) and the
    /// page-index offset after that term's entry.
    pg: u32,
    off: usize,
    eod: usize,
    pgidx_off: usize,
    /// The page of the doclist's first rowid.
    first_pg: u32,
    pub entries: AVec<Entry>,
}

/// Decode the leaf pages of segment `sg` into its terms and
/// doclists, in order.
pub fn decode_seg(st: &Store, sg: &Seg, detail: Detail) -> Result<AVec<TermRec>, Corrupt> {
    let mut out: AVec<TermRec> = AVec::new();
    if sg.first == 0 {
        return Ok(out);
    }
    // Bytes of the current poslist still to read (on later pages).
    let mut pending = 0usize;
    let mut in_doclist = false;
    let mut prev_rowid = 0i64;
    for pg in sg.first..=sg.last {
        let a = st.get(seg_rowid(sg.id, pg)).ok_or(Corrupt)?;
        if a.len() < 4 {
            return Err(Corrupt);
        }
        let szleaf = get_u16(a, 2);
        let rowid_ptr = get_u16(a, 0);
        if szleaf > a.len() || szleaf < 4 {
            return Err(Corrupt);
        }
        let nn = a.len();
        // Term offsets on this page, and the page-index offset after
        // each entry.
        let mut terms: AVec<(usize, usize)> = AVec::new();
        {
            let mut i = szleaf;
            let mut at = 0usize;
            while i < nn {
                let d = gv(a, &mut i).ok_or(Corrupt)? as usize;
                at += d;
                terms.push((at, i));
            }
        }
        let mut off = 4usize;
        if pending > 0 {
            let n = pending.min(szleaf - off);
            let e = out
                .as_mut_slice()
                .last_mut()
                .and_then(|t| t.entries.as_mut_slice().last_mut())
                .ok_or(Corrupt)?;
            e.data.extend_from_slice(&a[off..off + n]);
            pending -= n;
            off += n;
            if pending > 0 {
                continue;
            }
        }
        let mut ti = 0usize;
        // A doclist continuing from the previous page.
        let next_term = |ti: usize| terms.as_slice().get(ti).map_or(szleaf, |t| t.0);
        while off < szleaf {
            if off == next_term(ti) {
                // A term.
                let t = out.as_slice().last().map(|t| t.term.clone());
                let mut term = AVec::new();
                if ti == 0 {
                    let n = gv(a, &mut off).ok_or(Corrupt)? as usize;
                    term.extend_from_slice(a.get(off..off + n).ok_or(Corrupt)?);
                    off += n;
                } else {
                    let keep = gv(a, &mut off).ok_or(Corrupt)? as usize;
                    let n = gv(a, &mut off).ok_or(Corrupt)? as usize;
                    let prev = t.ok_or(Corrupt)?;
                    term.extend_from_slice(prev.as_slice().get(..keep).ok_or(Corrupt)?);
                    term.extend_from_slice(a.get(off..off + n).ok_or(Corrupt)?);
                    off += n;
                }
                let (eod, pgidx_off) = match terms.as_slice().get(ti + 1) {
                    Some(t) => (t.0, t.1),
                    None => (nn + 1, nn),
                };
                out.push(TermRec {
                    term,
                    pg,
                    off,
                    eod,
                    pgidx_off,
                    first_pg: pg,
                    entries: AVec::new(),
                });
                ti += 1;
                in_doclist = true;
                if off >= szleaf {
                    break;
                }
                // The first rowid of a doclist is absolute.
                let r = gv(a, &mut off).ok_or(Corrupt)? as i64;
                prev_rowid = r;
                pending = read_entry(a, &mut off, szleaf, next_term(ti), r, detail, &mut out)?;
                if pending > 0 {
                    break;
                }
                continue;
            }
            if !in_doclist {
                return Err(Corrupt);
            }
            // A rowid: absolute when it is the page's first.
            let start = off;
            let v = gv(a, &mut off).ok_or(Corrupt)?;
            let r = if start == rowid_ptr {
                v as i64
            } else {
                prev_rowid.wrapping_add(v as i64)
            };
            prev_rowid = r;
            let tr = out.as_mut_slice().last_mut().ok_or(Corrupt)?;
            if tr.entries.is_empty() {
                tr.first_pg = pg;
            }
            pending = read_entry(a, &mut off, szleaf, next_term(ti), r, detail, &mut out)?;
            if pending > 0 {
                break;
            }
        }
    }
    if pending > 0 {
        return Err(Corrupt);
    }
    Ok(out)
}

/// Read one entry's size field and position list after its rowid
/// `r` (at `*off`), appending it to the last term: returns the
/// position-list bytes still to read on later pages.
fn read_entry(
    a: &[u8],
    off: &mut usize,
    szleaf: usize,
    eod: usize,
    r: i64,
    detail: Detail,
    out: &mut AVec<TermRec>,
) -> Result<usize, Corrupt> {
    let tr = out.as_mut_slice().last_mut().ok_or(Corrupt)?;
    if detail == Detail::None {
        let lim = eod.min(szleaf);
        let mut e = Entry {
            rowid: r,
            del: false,
            npos: 1,
            data: AVec::new(),
        };
        if *off < lim && a.get(*off) == Some(&0) {
            e.del = true;
            *off += 1;
            if *off < lim && a.get(*off) == Some(&0) {
                *off += 1;
            } else {
                e.npos = 0;
            }
        }
        tr.entries.push(e);
        return Ok(0);
    }
    let sz = gv(a, off).ok_or(Corrupt)?;
    let npos = (sz >> 1) as usize;
    let n = npos.min(szleaf.saturating_sub(*off));
    let mut e = Entry {
        rowid: r,
        del: sz & 1 == 1,
        npos,
        data: AVec::new(),
    };
    e.data
        .extend_from_slice(a.get(*off..*off + n).ok_or(Corrupt)?);
    *off += n;
    tr.entries.push(e);
    Ok(npos - n)
}

/// The pin's merge iterator (`Fts5Iter` over a level's segments):
/// a tournament tree whose comparisons step an older segment past an
/// entry a newer one holds too (`fts5MultiIterDoCompare` /
/// `fts5MultiIterAdvanced` / `fts5MultiIterAdvanceRowid`). Where its
/// segments stand when a merge stops decides how they are trimmed.
struct Multi<'a> {
    segs: &'a [(Seg, AVec<TermRec>)],
    /// Per segment: (term, entry) indexes.
    pos: AVec<(usize, usize)>,
    eof: AVec<bool>,
    /// `aFirst[]`: winner and `bTermEq` per internal node.
    first: AVec<(usize, bool)>,
    nslot: usize,
    switch: i64,
}

impl<'a> Multi<'a> {
    fn new(segs: &'a [(Seg, AVec<TermRec>)]) -> Multi<'a> {
        let mut nslot = 2;
        while nslot < segs.len() {
            nslot *= 2;
        }
        let mut m = Multi {
            segs,
            pos: (0..nslot).map(|_| (0, 0)).collect(),
            eof: (0..nslot)
                .map(|k| segs.get(k).is_none_or(|s| s.1.is_empty()))
                .collect(),
            first: (0..nslot).map(|_| (0, false)).collect(),
            nslot,
            switch: 0,
        };
        let mut i = nslot - 1;
        while i > 0 {
            if let Some(eq) = m.compare(i) {
                m.seg_next(eq);
                m.advanced(eq, i);
            }
            i -= 1;
        }
        m.set_eof();
        m
    }

    fn term(&self, k: usize) -> &[u8] {
        self.segs[k].1[self.pos[k].0].term.as_slice()
    }

    fn rowid(&self, k: usize) -> i64 {
        let (t, e) = self.pos[k];
        self.segs[k].1[t].entries[e].rowid
    }

    /// The segment iterator's xNext: whether it moved to a new term.
    fn seg_next(&mut self, k: usize) -> bool {
        if self.eof[k] {
            return false;
        }
        let d = &self.segs[k].1;
        let (mut t, mut e) = self.pos[k];
        e += 1;
        let mut newterm = false;
        while d.as_slice().get(t).is_some_and(|x| e >= x.entries.len()) {
            t += 1;
            e = 0;
            newterm = true;
        }
        if t >= d.len() {
            self.eof[k] = true;
        }
        self.pos[k] = (t, e);
        newterm
    }

    /// `fts5MultiIterDoCompare`: the segment to step when the node's
    /// two sides stand on the same entry.
    fn compare(&mut self, iout: usize) -> Option<usize> {
        let (i1, i2) = if iout >= self.nslot / 2 {
            let a = (iout - self.nslot / 2) * 2;
            (a, a + 1)
        } else {
            (self.first[iout * 2].0, self.first[iout * 2 + 1].0)
        };
        self.first[iout].1 = false;
        let res = if self.eof[i1] {
            i2
        } else if self.eof[i2] {
            i1
        } else {
            let c = self.term(i1).cmp(self.term(i2));
            let c = if c.is_eq() {
                self.first[iout].1 = true;
                if self.rowid(i1) == self.rowid(i2) {
                    return Some(i2);
                }
                self.rowid(i1).cmp(&self.rowid(i2))
            } else {
                c
            };
            if c.is_lt() { i1 } else { i2 }
        };
        self.first[iout].0 = res;
        None
    }

    /// `fts5MultiIterAdvanced`.
    fn advanced(&mut self, changed: usize, minset: usize) {
        let mut i = (self.nslot + changed) / 2;
        while i >= minset && i > 0 {
            if let Some(eq) = self.compare(i) {
                self.seg_next(eq);
                i = self.nslot + eq;
            }
            i /= 2;
        }
    }

    /// `fts5MultiIterAdvanceRowid`: whether the tree needs a full
    /// update.
    fn advance_rowid(&mut self, changed: usize) -> bool {
        let mut new = changed;
        if self.rowid(new) >= self.switch {
            let mut other = changed ^ 1;
            self.switch = i64::MAX;
            let mut i = (self.nslot + changed) / 2;
            loop {
                if self.first[i].1 {
                    let (rn, ro) = (self.rowid(new), self.rowid(other));
                    if rn == ro {
                        return true;
                    } else if ro < rn {
                        self.switch = ro;
                        new = other;
                    } else if ro < self.switch {
                        self.switch = ro;
                    }
                }
                self.first[i].0 = new;
                if i == 1 {
                    break;
                }
                other = self.first[i ^ 1].0;
                i /= 2;
            }
        }
        false
    }

    fn set_eof(&mut self) {
        let w = self.first[1].0;
        if !self.eof[w] {
            self.switch = self.rowid(w);
        }
    }

    /// The winning segment, unless every one is at its end.
    fn current(&self) -> Option<usize> {
        let w = self.first[1].0;
        (!self.eof[w]).then_some(w)
    }

    /// `fts5MultiIterNext`.
    fn next(&mut self) {
        let w = self.first[1].0;
        let newterm = self.seg_next(w);
        if self.eof[w] || newterm || self.advance_rowid(w) {
            self.advanced(w, 1);
            self.set_eof();
        }
    }
}

/// `fts5IndexMergeLevel`: merge level `il`'s segments (or continue
/// its incremental merge) into level `il + 1`, writing at most about
/// `*nrem` leaves when given.
fn merge_level(
    st: &mut Store,
    s: &mut Structure,
    il: usize,
    nrem: Option<&mut i64>,
    t: &Tuning,
    detail: Detail,
) -> Result<(), Corrupt> {
    let budget = nrem.as_ref().map(|r| **r);
    let continuing = s.levels[il].merge > 0;
    let (ninput, mut w, out_idx) = if continuing {
        let ninput = s.levels[il].merge;
        let out = s.levels.as_slice().get(il + 1).ok_or(Corrupt)?;
        let last = out.segs.as_slice().last().ok_or(Corrupt)?;
        let mut w = Writer::new(last.id, t.pgsz);
        w.pgno = last.last + 1;
        w.bt_page = 0;
        (ninput, w, out.segs.len() - 1)
    } else {
        let segid = s.alloc_segid().ok_or(Corrupt)?;
        if il + 1 == s.levels.len() {
            if s.levels.len() >= MAX_LEVEL {
                return Err(Corrupt);
            }
            s.levels.push(Level::default());
        }
        let ninput = s.levels[il].segs.len();
        let mut sg = Seg {
            id: segid,
            first: 1,
            ..Seg::default()
        };
        if s.origin > 0
            && let (Some(a), Some(b)) = (
                s.levels[il].segs.as_slice().first(),
                s.levels[il].segs.as_slice().last(),
            )
        {
            sg.o1 = a.o1;
            sg.o2 = b.o2;
        }
        s.levels[il + 1].segs.push(sg);
        s.nseg += 1;
        let out_idx = s.levels[il + 1].segs.len() - 1;
        (ninput, Writer::new(segid, t.pgsz), out_idx)
    };
    let boldest = s.levels[il + 1].segs.len() == 1 && s.levels.len() == il + 2;
    // The inputs, newest first (the pin's iterator order).
    let mut inputs: AVec<(Seg, AVec<TermRec>)> = AVec::new();
    let mut tombs: AVec<AVec<AVec<u8>>> = AVec::new();
    for k in (0..ninput).rev() {
        let sg = s.levels[il].segs.as_slice().get(k).ok_or(Corrupt)?.clone();
        let d = decode_seg(st, &sg, detail)?;
        tombs.push(tomb_pages(st, &sg)?);
        inputs.push((sg, d));
    }
    let mut m = Multi::new(inputs.as_slice());
    let mut term: Option<AVec<u8>> = None;
    let mut written = false;
    let mut broke = false;
    while let Some(k) = m.current() {
        let tr = &inputs[k].1[m.pos[k].0];
        // The pin's iterator steps past a tombstoned entry unseen.
        if tomb_hit(tombs[k].as_slice(), tr.entries[m.pos[k].1].rowid) {
            m.next();
            continue;
        }
        if term.as_ref().is_none_or(|t| *t != tr.term) {
            if let Some(b) = budget
                && w.n_leaf > b
            {
                broke = true;
                break;
            }
            term = Some(tr.term.clone());
            written = false;
        }
        let e = &tr.entries[m.pos[k].1];
        if !(e.npos == 0 && (boldest || !e.del)) {
            if !written {
                w.append_term(st, tr.term.as_slice());
                written = true;
            }
            w.append_rowid(st, e.rowid);
            if detail == Detail::None {
                if e.del {
                    w.buf.push(0);
                    if e.npos > 0 {
                        w.buf.push(0);
                    }
                }
            } else {
                pv(&mut w.buf, (e.npos as u64) * 2 + u64::from(e.del));
                w.append_poslist(st, e.data.as_slice());
            }
        }
        m.next();
    }
    let pos: AVec<usize> = (0..inputs.len())
        .map(|k| if m.eof[k] { usize::MAX } else { m.pos[k].0 })
        .collect();
    let last = w.finish(st);
    if let Some(sg) = s.levels[il + 1].segs.as_mut_slice().get_mut(out_idx) {
        sg.last = last;
    }
    if !broke {
        let mut nent = 0u64;
        for k in 0..ninput {
            let old = s.levels[il].segs[k].clone();
            nent = nent.wrapping_add(old.nent.wrapping_sub(old.nentt));
            st.remove_segment(&old);
        }
        if let Some(sg) = s.levels[il + 1].segs.as_mut_slice().get_mut(out_idx) {
            sg.nent = sg.nent.wrapping_add(nent);
        }
        s.levels[il].segs.remove_front(ninput);
        s.nseg -= ninput;
        s.levels[il].merge = 0;
        if last == 0 {
            s.levels[il + 1].segs.remove_at(out_idx);
            s.nseg -= 1;
        }
    } else {
        // fts5TrimSegments
        for (k, (sg, d)) in inputs.iter().enumerate() {
            let slot = ninput - 1 - k;
            if sg.first == 0 {
                continue;
            }
            let target = &mut s.levels[il].segs[slot];
            match d.as_slice().get(pos[k]).filter(|_| pos[k] != usize::MAX) {
                None => {
                    target.first = 0;
                    target.last = 0;
                }
                Some(tr) => {
                    let id = sg.id;
                    let page = st.get(seg_rowid(id, tr.pg)).ok_or(Corrupt)?;
                    let szleaf = get_u16(page, 2);
                    if tr.off > szleaf {
                        return Err(Corrupt);
                    }
                    let mut b = copy(&[0, 0, 0, 0]);
                    pv(&mut b, tr.term.len() as u64);
                    b.extend_from_slice(tr.term.as_slice());
                    b.extend_from_slice(page.get(tr.off..szleaf).ok_or(Corrupt)?);
                    let n = b.len();
                    put_u16(&mut b, 2, n);
                    pv(&mut b, 4);
                    if tr.first_pg == tr.pg && tr.eod < szleaf && tr.pgidx_off <= page.len() {
                        let ndiff = szleaf - tr.eod;
                        let v = b.len() - 1 - ndiff - 4;
                        pv(&mut b, v as u64);
                        b.extend_from_slice(page.get(tr.pgidx_off..).unwrap_or(&[]));
                    }
                    target.first = tr.pg;
                    st.del(seg_rowid(id, 1), seg_rowid(id, tr.pg));
                    st.put(seg_rowid(id, tr.pg), b);
                }
            }
        }
        s.levels[il].merge = ninput;
    }
    if let Some(r) = nrem {
        *r -= w.n_leaf;
    }
    Ok(())
}

/// `sqlite3Fts5IndexOptimize` (after the pending terms are flushed):
/// every segment merged into one.
pub fn optimize(st: &mut Store, t: &Tuning, detail: Detail) -> Result<(), Corrupt> {
    let s = st.structure()?;
    let Some(mut n) = optimize_struct(&s) else {
        return Ok(());
    };
    let Some(il) = n.levels.iter().position(|l| !l.segs.is_empty()) else {
        return Ok(());
    };
    while n
        .levels
        .as_slice()
        .get(il)
        .is_some_and(|l| !l.segs.is_empty())
    {
        let mut nrem = OPT_WORK_UNIT;
        merge_level(st, &mut n, il, Some(&mut nrem), t, detail)?;
    }
    st.write_structure(&n);
    Ok(())
}

/// `fts5IndexOptimizeStruct`.
fn optimize_struct(s: &Structure) -> Option<Structure> {
    let nseg = s.nseg;
    if nseg == 0 {
        return None;
    }
    for l in s.levels.iter() {
        let nthis = l.segs.len();
        if nthis > 0 && (nthis == nseg || (nthis + 1 == nseg && l.merge == nthis)) {
            if nseg == 1 && nthis == 1 && l.segs[0].npgt == 0 {
                return None;
            }
            return Some(s.clone());
        }
    }
    let nlevel = (s.levels.len() + 1).min(MAX_LEVEL);
    let mut n = Structure {
        cookie: s.cookie,
        levels: AVec::new(),
        nseg,
        wc: s.wc,
        origin: s.origin,
    };
    for _ in 0..nlevel {
        n.levels.push(Level::default());
    }
    let mut segs = AVec::new();
    for l in s.levels.iter().rev() {
        for sg in l.segs.iter() {
            segs.push(sg.clone());
        }
    }
    n.levels[nlevel - 1].segs = segs;
    Some(n)
}

/// `sqlite3Fts5IndexMerge` (after the pending terms are flushed):
/// the `merge` command.
pub fn user_merge(st: &mut Store, t: &Tuning, detail: Detail, nmerge: i64) -> Result<(), Corrupt> {
    let s = st.structure()?;
    let mut nmin = t.usermerge.max(0) as usize;
    let mut nmerge = nmerge;
    let mut s = if nmerge < 0 {
        nmin = 1;
        nmerge = nmerge.wrapping_neg();
        match optimize_struct(&s) {
            Some(n) => n,
            None => return Ok(()),
        }
    } else {
        s
    };
    if !s.levels.is_empty() && merge(st, &mut s, nmerge, nmin, t, detail)? {
        st.write_structure(&s);
    }
    Ok(())
}

// ---------------------------------------------------------------
// Reading
// ---------------------------------------------------------------

/// One row of a term's merged doclist.
#[derive(Debug, Clone)]
pub struct Hit {
    pub rowid: i64,
    /// Position list (detail=full: columns and offsets; column:
    /// column numbers; none: empty).
    pub pl: AVec<u8>,
}

/// The index as a statement reads it: every segment decoded, newest
/// first, the pending terms as the newest of all.
#[derive(Debug, Default)]
pub struct Reader {
    segs: AVec<AVec<TermRec>>,
    /// Each segment's tombstone hash pages (none for the pending
    /// terms).
    tombs: AVec<AVec<AVec<u8>>>,
}

impl Reader {
    /// Decode the segments `st`'s structure lists and `h`'s pending
    /// terms.
    pub fn new(st: &Store, h: Option<&Hash>, detail: Detail) -> Result<Reader, Corrupt> {
        let s = st.structure()?;
        let mut r = Reader::default();
        if let Some(h) = h
            && !h.is_empty()
        {
            r.segs.push(hash_terms(h, detail)?);
            r.tombs.push(AVec::new());
        }
        for sg in s.newest_first().iter() {
            r.segs.push(decode_seg(st, sg, detail)?);
            r.tombs.push(tomb_pages(st, sg)?);
        }
        Ok(r)
    }

    /// The merged doclist of every key `key` (exactly, or as a
    /// prefix when `prefix`), rows ascending; per row the union of
    /// the matching keys' position lists.
    pub fn lookup(&self, key: &[u8], prefix: bool, detail: Detail) -> AVec<Hit> {
        // Every matching key, each merged on its own.
        let mut keys: AVec<AVec<u8>> = AVec::new();
        for seg in self.segs.iter() {
            let start = seg.as_slice().partition_point(|t| t.term.as_slice() < key);
            for t in seg.as_slice().get(start..).unwrap_or(&[]) {
                let hit = if prefix {
                    t.term.as_slice().starts_with(key)
                } else {
                    t.term.as_slice() == key
                };
                if !hit {
                    break;
                }
                if let Err(i) = keys
                    .as_slice()
                    .binary_search_by(|k| k.as_slice().cmp(t.term.as_slice()))
                {
                    keys.insert_at(i, t.term.clone());
                }
            }
        }
        let mut out: AVec<Hit> = AVec::new();
        for k in keys.iter() {
            let one = self.merged(k.as_slice());
            out = if out.is_empty() {
                one
            } else {
                union_hits(&out, &one, detail)
            };
        }
        out
    }

    /// One key's doclist, newest segment winning each row, deleted
    /// rows (no positions) dropped.
    fn merged(&self, key: &[u8]) -> AVec<Hit> {
        let mut rows: AVec<(i64, usize, usize, usize)> = AVec::new();
        for (si, seg) in self.segs.iter().enumerate() {
            if let Ok(ti) = seg
                .as_slice()
                .binary_search_by(|t| t.term.as_slice().cmp(key))
            {
                for (ei, e) in seg[ti].entries.iter().enumerate() {
                    rows.push((e.rowid, si, ti, ei));
                }
            }
        }
        rows.as_mut_slice().sort_by_key(|r| (r.0, r.1));
        let mut out = AVec::new();
        let mut last: Option<i64> = None;
        for (rowid, si, ti, ei) in rows.iter() {
            if last == Some(*rowid) {
                continue;
            }
            last = Some(*rowid);
            let e = &self.segs[*si][*ti].entries[*ei];
            if e.npos == 0 || tomb_hit(self.tombs[*si].as_slice(), *rowid) {
                continue;
            }
            out.push(Hit {
                rowid: *rowid,
                pl: e.data.clone(),
            });
        }
        out
    }

    /// Every key of the main index (`'0'`-prefixed), with its merged
    /// doclist, in key order (fts5vocab, integrity-check).
    pub fn all_terms(&self) -> AVec<(AVec<u8>, AVec<Hit>)> {
        let mut keys: AVec<AVec<u8>> = AVec::new();
        for seg in self.segs.iter() {
            for t in seg.iter() {
                if let Err(i) = keys
                    .as_slice()
                    .binary_search_by(|k| k.as_slice().cmp(t.term.as_slice()))
                {
                    keys.insert_at(i, t.term.clone());
                }
            }
        }
        let mut out = AVec::new();
        for k in keys.iter() {
            let m = self.merged(k.as_slice());
            if !m.is_empty() {
                out.push((k.clone(), m));
            }
        }
        out
    }
}

/// The pending terms as a decoded segment.
fn hash_terms(h: &Hash, detail: Detail) -> Result<AVec<TermRec>, Corrupt> {
    let mut out = AVec::new();
    for (key, dl) in h.entries().iter() {
        let mut tr = TermRec {
            term: key.clone(),
            pg: 0,
            off: 0,
            eod: 0,
            pgidx_off: 0,
            first_pg: 0,
            entries: AVec::new(),
        };
        let a = dl.as_slice();
        let mut off = 0;
        let mut rowid = 0i64;
        let mut one: AVec<TermRec> = AVec::new();
        one.push(tr.clone());
        while off < a.len() {
            rowid = rowid.wrapping_add(gv(a, &mut off).ok_or(Corrupt)? as i64);
            let left = read_entry(a, &mut off, a.len(), a.len(), rowid, detail, &mut one)?;
            if left > 0 {
                return Err(Corrupt);
            }
        }
        if let Some(t) = one.pop() {
            tr.entries = t.entries;
        }
        out.push(tr);
    }
    Ok(out)
}

/// A position list's positions (`(column << 32) | offset`; detail=
/// column: the column numbers).
pub fn positions(pl: &[u8]) -> AVec<i64> {
    let mut out = AVec::new();
    let mut i = 0;
    let mut col = 0i64;
    let mut off = 0i64;
    while i < pl.len() {
        let Some(v) = gv(pl, &mut i) else {
            break;
        };
        if v == 1 {
            col = gv(pl, &mut i).unwrap_or(0) as i64;
            off = 0;
            continue;
        }
        off = off.wrapping_add(v as i64 - 2);
        out.push((col << 32) | (off & 0xFFFF_FFFF));
    }
    out
}

/// The position list of `pos` (sorted, distinct).
pub fn poslist_of(pos: &[i64]) -> AVec<u8> {
    let mut out = AVec::new();
    let mut col = 0i64;
    let mut prev = 0i64;
    for p in pos.iter() {
        let c = p >> 32;
        let o = p & 0xFFFF_FFFF;
        if c != col {
            out.push(1);
            pv(&mut out, c as u64);
            col = c;
            prev = 0;
        }
        pv(&mut out, (o - prev + 2) as u64);
        prev = o;
    }
    out
}

/// Two doclists' union (rows ascending, positions merged).
pub(crate) fn union_hits(a: &AVec<Hit>, b: &AVec<Hit>, detail: Detail) -> AVec<Hit> {
    let mut out = AVec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let x = a.as_slice().get(i);
        let y = b.as_slice().get(j);
        match (x, y) {
            (Some(x), Some(y)) if x.rowid == y.rowid => {
                let pl = if detail == Detail::None {
                    AVec::new()
                } else {
                    let mut p = positions(x.pl.as_slice());
                    for q in positions(y.pl.as_slice()).iter() {
                        if let Err(k) = p.as_slice().binary_search(q) {
                            p.insert_at(k, *q);
                        }
                    }
                    poslist_of(p.as_slice())
                };
                out.push(Hit { rowid: x.rowid, pl });
                i += 1;
                j += 1;
            }
            (Some(x), Some(y)) if x.rowid < y.rowid => {
                out.push(x.clone());
                i += 1;
            }
            (Some(x), None) => {
                out.push(x.clone());
                i += 1;
            }
            (_, Some(y)) => {
                out.push(y.clone());
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}
