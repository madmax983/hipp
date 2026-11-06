//! FTS5 delete writes (v0.17, leaf `fts5_delete`): the two delete
//! modes of the pin's `fts5_index.c` that rewrite the index instead of
//! appending delete markers.
//!
//! - **Tombstones** (`contentless_delete=1`): a deleted row's rowid
//!   goes into the tombstone hash of every segment whose origin range
//!   holds the row's origin (`sqlite3Fts5IndexContentlessDelete`,
//!   `fts5IndexTombstoneAdd`), the hash pages rebuilt and grown as the
//!   pin grows them (`fts5IndexTombstoneRebuild` /
//!   `fts5IndexTombstoneRehash`: one 32-slot page, then a page of up
//!   to `pgsz` bytes, then `2n + 1` pages; 4-byte keys until a rowid
//!   needs 8). The structure's `nEntryTombstone` / `nPgTombstone`
//!   counters change in the cached structure, written at the next
//!   flush.
//! - **Secure delete** (`secure-delete=1`): flushing a pending delete
//!   removes the row's entry from the newest segment holding it, in
//!   place (`fts5FlushSecureDelete` / `fts5DoSecureDelete`): the
//!   entry's bytes, the next entry's rowid delta re-based, the term
//!   itself when its doclist empties (the next term on the page moved
//!   into its key, or the term at the end of an earlier page dropped),
//!   overflowing position lists cut from the pages that follow
//!   (`fts5SecureDeleteOverflow`), the first-rowid header and page
//!   index rewritten, and the `%_idx` row of a page left with no term
//!   deleted (`fts5SecureDeleteIdxEntry`). The doclist indexes are
//!   left as they are, as on the pin. The first such delete sets
//!   `version` to 5 and moves the cookie on.

use super::*;

/// `FTS5_TOMBSTONE_ROWID(segid, ipg)`.
fn tomb_rowid(segid: u32, ipg: u32) -> i64 {
    seg_rowid(segid + (1 << 16), ipg)
}

/// What one flush does in secure-delete mode, and what it learns.
#[derive(Debug, Clone, Copy, Default)]
pub struct SdFlush {
    /// `nContentlessDelete`: tombstones added since the last flush
    /// (counted as leaves written by automerge).
    pub ncdel: u64,
    /// `bSecureDelete`.
    pub secure: bool,
    /// `iVersion == FTS5_CURRENT_VERSION_SECUREDELETE` (5).
    pub v5: bool,
    /// Set when this flush moved `version` to 5 (the caller writes the
    /// `%_config` row; the structure's cookie moves on).
    pub bumped: bool,
}

// ---------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------

/// Zero-extend `a` to `n` bytes.
fn grow(a: &mut AVec<u8>, n: usize) {
    while a.len() < n {
        a.push(0);
    }
}

fn be32(a: &[u8], i: usize) -> u64 {
    a.get(i..i + 4).map_or(0, |s| {
        u64::from(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    })
}

fn key_size(p: &[u8]) -> usize {
    if p.first() == Some(&4) { 4 } else { 8 }
}

/// Slot `i`'s key of a tombstone page (0: empty).
fn slot(p: &[u8], sz: usize, i: usize) -> u64 {
    p.get(8 + i * sz..8 + i * sz + sz)
        .map_or(0, |k| k.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)))
}

fn set_slot(p: &mut [u8], sz: usize, i: usize, v: u64) {
    if let Some(k) = p.get_mut(8 + i * sz..8 + i * sz + sz) {
        for (j, b) in k.iter_mut().enumerate() {
            *b = (v >> (8 * (sz - 1 - j))) as u8;
        }
    }
}

/// `fts5IndexTombstoneAddToPage`: 0 when added, 1 when the page is
/// half full (and `force` is off), 2 when a 4-byte page cannot hold
/// `rowid`.
fn add_to_page(p: &mut [u8], force: bool, npg: u64, rowid: u64) -> u8 {
    let sz = key_size(p);
    let nslot = if p.len() > 16 { (p.len() - 8) / sz } else { 1 };
    let nelem = be32(p, 4);
    let mut i = match rowid.checked_div(npg) {
        Some(q) => (q % nslot as u64) as usize,
        None => 0,
    };
    let mut ncollide = nslot;
    if sz == 4 && rowid > 0xFFFF_FFFF {
        return 2;
    }
    if rowid == 0 {
        if let Some(b) = p.get_mut(1) {
            *b = 1;
        }
        return 0;
    }
    if !force && nelem >= (nslot / 2) as u64 {
        return 1;
    }
    let n = (nelem as u32).wrapping_add(1).to_be_bytes();
    if let Some(s) = p.get_mut(4..8) {
        s.copy_from_slice(&n);
    }
    while slot(p, sz, i) != 0 {
        i = (i + 1) % nslot;
        if ncollide == 0 {
            return 0;
        }
        ncollide -= 1;
    }
    set_slot(p, sz, i, rowid);
    0
}

/// `fts5IndexTombstoneRehash`: whether every key of the segment's
/// current hash fit the `out` pages.
fn rehash(
    st: &Store,
    sg: &Seg,
    p1: Option<&[u8]>,
    ipg1: u32,
    szkey: usize,
    out: &mut [AVec<u8>],
) -> Result<bool, Corrupt> {
    let nout = out.len() as u64;
    for o in out.iter_mut() {
        if let Some(b) = o.as_mut_slice().first_mut() {
            *b = szkey as u8;
        }
    }
    for ii in 0..sg.npgt {
        let data = match p1 {
            Some(p) if ii == ipg1 => p,
            _ => st.get(tomb_rowid(sg.id, ii)).ok_or(Corrupt)?,
        };
        let szin = key_size(data);
        let nslot = data.len().saturating_sub(8) / szin;
        let mut ok = true;
        for k in 0..nslot {
            let v = slot(data, szin, k);
            if v != 0 {
                let i = (v % nout) as usize;
                let pg = out.get_mut(i).ok_or(Corrupt)?;
                if add_to_page(pg.as_mut_slice(), false, nout, v) != 0 {
                    ok = false;
                    break;
                }
            }
        }
        if ii == 0
            && let (Some(o), Some(f)) = (out.first_mut(), data.get(1))
            && let Some(b) = o.as_mut_slice().get_mut(1)
        {
            *b = *f;
        }
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `fts5IndexTombstoneRebuild`: the segment's new hash pages.
fn rebuild(
    st: &Store,
    sg: &Seg,
    p1: Option<&[u8]>,
    ipg1: u32,
    szkey: usize,
    pgsz: usize,
) -> Result<AVec<AVec<u8>>, Corrupt> {
    const MINSLOT: usize = 32;
    let per_page = MINSLOT.max(pgsz.saturating_sub(8) / szkey);
    let (mut nout, mut nslot) = (0usize, 0usize);
    if sg.npgt == 0 {
        nout = 1;
        nslot = MINSLOT;
    } else if sg.npgt == 1 {
        let nelem = p1.map_or(0, |p| be32(p, 4)) as usize;
        nout = 1;
        nslot = nelem.saturating_mul(4).max(MINSLOT);
        if nslot > per_page {
            nout = 0;
        }
    }
    if nout == 0 {
        nout = sg.npgt as usize * 2 + 1;
        nslot = per_page;
    }
    // Each retry doubles the pages: a hash that never fits is corrupt.
    for _ in 0..16 {
        let mut out: AVec<AVec<u8>> = AVec::new();
        for _ in 0..nout {
            let mut pg = AVec::new();
            grow(&mut pg, 8 + nslot * szkey);
            out.push(pg);
        }
        if rehash(st, sg, p1, ipg1, szkey, out.as_mut_slice())? {
            return Ok(out);
        }
        nslot = per_page;
        nout = nout * 2 + 1;
    }
    Err(Corrupt)
}

/// `fts5IndexTombstoneAdd`: `rowid` into segment `sg`'s hash; whether
/// the hash was rebuilt (its page count stored in `sg`).
fn tomb_add(st: &mut Store, sg: &mut Seg, rowid: u64, pgsz: usize) -> Result<bool, Corrupt> {
    let mut old: Option<AVec<u8>> = None;
    let mut ipg = 0u32;
    if sg.npgt > 0 {
        ipg = (rowid % u64::from(sg.npgt)) as u32;
        let id = tomb_rowid(sg.id, ipg);
        // (A page the rowid does not fit is left unchanged.)
        let mut p = copy(st.get(id).ok_or(Corrupt)?);
        if add_to_page(p.as_mut_slice(), false, u64::from(sg.npgt), rowid) == 0 {
            st.put(id, p);
            return Ok(false);
        }
        old = Some(p);
    }
    let mut szkey = old.as_ref().map_or(4, |p| key_size(p.as_slice()));
    if rowid > 0xFFFF_FFFF {
        szkey = 8;
    }
    let mut pages = rebuild(st, sg, old.as_ref().map(|p| p.as_slice()), ipg, szkey, pgsz)?;
    let n = pages.len() as u64;
    let i = (rowid % n) as usize;
    let pg = pages.as_mut_slice().get_mut(i).ok_or(Corrupt)?;
    add_to_page(pg.as_mut_slice(), true, n, rowid);
    for (ii, p) in pages.into_iter().enumerate() {
        st.put(tomb_rowid(sg.id, ii as u32), p);
    }
    sg.npgt = n as u32;
    Ok(true)
}

/// The `%_data` rows [`contentless_delete`] reads for (`origin`,
/// `rowid`): the tombstone page each segment holding the origin hashes
/// the rowid to, or (`all`) every page of those hashes, which a rebuild
/// reads. (A delete then reads a page or so, not the index.)
pub fn tomb_ids(s: &Structure, origin: u64, rowid: i64, all: bool) -> AVec<i64> {
    let mut out = AVec::new();
    for sg in s.newest_first().iter() {
        if sg.o1 <= origin && sg.o2 >= origin && sg.npgt > 0 {
            let at = (rowid as u64 % u64::from(sg.npgt)) as u32;
            for pg in 0..sg.npgt {
                if all || pg == at {
                    out.push(tomb_rowid(sg.id, pg));
                }
            }
        }
    }
    out
}

/// `sqlite3Fts5IndexContentlessDelete`: rowid `rowid` of origin
/// `origin` is deleted. Updates `s` (the cached structure) and returns
/// the tombstones added (`nContentlessDelete`).
pub fn contentless_delete(
    st: &mut Store,
    s: &mut Structure,
    origin: u64,
    rowid: i64,
    pgsz: usize,
) -> Result<u64, Corrupt> {
    let mut found = false;
    let mut n = 0u64;
    for il in (0..s.levels.len()).rev() {
        for is in (0..s.levels[il].segs.len()).rev() {
            let sg = &mut s.levels[il].segs[is];
            if sg.o1 <= origin && sg.o2 >= origin {
                if !found {
                    sg.nentt = sg.nentt.wrapping_add(1);
                    found = true;
                }
                n += 1;
                // A rebuilt hash writes the cached structure at once.
                if tomb_add(st, sg, rowid as u64, pgsz)? {
                    st.write_structure(s);
                }
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------
// Secure delete
// ---------------------------------------------------------------

/// Where the pin's segment iterator stands on the entry to remove
/// (`Fts5SegIter`).
struct Loc {
    /// `iLeafPgno` / `iLeafOffset` (the position list's start, past
    /// the size field; detail=none: past the flags).
    leaf_pg: u32,
    leaf_off: usize,
    /// `nPos` (bytes; detail=none: 0 or 1) and `bDel`.
    npos: usize,
    bdel: bool,
    /// `iTermLeafPgno` / `iTermLeafOffset` (just past the term).
    term_pg: u32,
    term_off: usize,
    /// `iEndofDoclist` on the entry's page.
    eod: usize,
}

/// The entry for (`key`, `rowid`) in segment `sg`, if it has one.
fn locate(
    st: &Store,
    sg: &Seg,
    key: &[u8],
    rowid: i64,
    detail: Detail,
) -> Result<Option<Loc>, Corrupt> {
    if sg.first == 0 {
        return Ok(None);
    }
    let mut term: AVec<u8> = AVec::new();
    let mut have_term = false;
    let (mut term_pg, mut term_off) = (0u32, 0usize);
    let mut eod = 0usize;
    let mut pending = 0usize;
    let mut prev = 0i64;
    // The walk starts at the first term of the page the term's
    // doclist starts on, and inside the doclist goes on from the page
    // the rowid is on (review fix: every delete walked the segment
    // from its first page, quadratic in a flush's deletes).
    let first_pg = term_page(st, sg, key);
    let mut pg = first_pg;
    while pg <= sg.last {
        let mut from_rowid = false;
        if pg > first_pg
            && have_term
            && term.as_slice() == key
            && let Some(q) = rowid_page(st, sg, pg, rowid)?
        {
            pg = q;
            from_rowid = true;
        }
        let a = st.get(seg_rowid(sg.id, pg)).ok_or(Corrupt)?;
        let nn = a.len();
        if nn < 4 {
            return Err(Corrupt);
        }
        let szleaf = get_u16(a, 2);
        let rowid_ptr = get_u16(a, 0);
        if szleaf > nn || szleaf < 4 {
            return Err(Corrupt);
        }
        let mut offs: AVec<usize> = AVec::new();
        let mut i = szleaf;
        let mut at = 0usize;
        while i < nn {
            at = at.saturating_add(gv(a, &mut i).ok_or(Corrupt)? as usize);
            offs.push(at);
        }
        if let Some(f) = offs.as_slice().first()
            && rowid_ptr != 0
        {
            eod = *f;
        }
        let mut off = 4usize;
        if pg == first_pg {
            // (What comes before the first term is an earlier term's.)
            off = offs.as_slice().first().copied().unwrap_or(szleaf);
            pending = 0;
        } else if from_rowid {
            off = rowid_ptr;
            pending = 0;
        } else if pending > 0 {
            let n = pending.min(szleaf - off);
            pending -= n;
            off += n;
            if pending > 0 {
                pg = pg.checked_add(1).ok_or(Corrupt)?;
                continue;
            }
        }
        let mut ti = 0usize;
        while off < szleaf {
            let next = offs.as_slice().get(ti).copied().unwrap_or(szleaf);
            let start = off;
            let r;
            if off == next {
                let keep = if ti == 0 {
                    0
                } else {
                    gv(a, &mut off).ok_or(Corrupt)? as usize
                };
                let n = gv(a, &mut off).ok_or(Corrupt)? as usize;
                term.truncate(keep.min(term.len()));
                let end = off.checked_add(n).ok_or(Corrupt)?;
                term.extend_from_slice(a.get(off..end).ok_or(Corrupt)?);
                off = end;
                have_term = true;
                if term.as_slice() > key {
                    return Ok(None);
                }
                term_pg = pg;
                term_off = off;
                ti += 1;
                eod = offs.as_slice().get(ti).copied().unwrap_or(nn + 1);
                if off >= szleaf {
                    break;
                }
                r = gv(a, &mut off).ok_or(Corrupt)? as i64;
            } else {
                if !have_term {
                    return Err(Corrupt);
                }
                let v = gv(a, &mut off).ok_or(Corrupt)?;
                r = if start == rowid_ptr {
                    v as i64
                } else {
                    prev.wrapping_add(v as i64)
                };
            }
            prev = r;
            let lim = eod.min(szleaf);
            let (npos, bdel);
            if detail == Detail::None {
                let mut d = false;
                let mut np = 1;
                if off < lim && a.get(off) == Some(&0) {
                    d = true;
                    off += 1;
                    if off < lim && a.get(off) == Some(&0) {
                        off += 1;
                    } else {
                        np = 0;
                    }
                }
                npos = np;
                bdel = d;
            } else {
                let sz = gv(a, &mut off).ok_or(Corrupt)?;
                npos = (sz >> 1) as usize;
                bdel = sz & 1 == 1;
            }
            let here = term.as_slice() == key;
            if here && r == rowid {
                return Ok(Some(Loc {
                    leaf_pg: pg,
                    leaf_off: off,
                    npos,
                    bdel,
                    term_pg,
                    term_off,
                    eod,
                }));
            }
            if here && r > rowid {
                return Ok(None);
            }
            if detail != Detail::None {
                let n = npos.min(szleaf.saturating_sub(off));
                off += n;
                pending = npos - n;
                if pending > 0 {
                    break;
                }
            }
        }
        pg = match pg.checked_add(1) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(None)
}

/// The first term of leaf page `a` (stored whole); `None` when the
/// page holds no term.
fn first_term(a: &[u8]) -> Option<&[u8]> {
    let szleaf = get_u16(a, 2);
    if szleaf >= a.len() {
        return None;
    }
    let mut i = szleaf;
    let mut o = gv(a, &mut i)? as usize;
    let n = gv(a, &mut o)? as usize;
    a.get(o..o.checked_add(n)?)
}

/// The page of segment `sg` where `key`'s doclist starts, if the
/// segment has it: the last page whose first term is at most `key`
/// (terms ascend through the segment; pages holding no term are
/// passed over).
fn term_page(st: &Store, sg: &Seg, key: &[u8]) -> u32 {
    let (mut lo, mut hi, mut best) = (sg.first, sg.last, sg.first);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let mut p = mid;
        let mut ft = None;
        while p <= hi {
            if let Some(t) = st.get(seg_rowid(sg.id, p)).and_then(first_term) {
                ft = Some(t);
                break;
            }
            p = match p.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
        match ft {
            Some(t) if t <= key => {
                best = p;
                lo = match p.checked_add(1) {
                    Some(n) => n,
                    None => break,
                };
            }
            _ => {
                hi = match mid.checked_sub(1) {
                    Some(n) => n,
                    None => break,
                };
            }
        }
    }
    best
}

/// Inside a doclist that goes on at page `pg`: the last page from
/// `pg` on, up to the doclist's end (the next page holding a term),
/// whose first rowid (stored whole at the header's offset) is at most
/// `rowid` — the page the walk may go on from.
fn rowid_page(st: &Store, sg: &Seg, pg: u32, rowid: i64) -> Result<Option<u32>, Corrupt> {
    let mut best = None;
    let mut p = pg;
    while p <= sg.last {
        let a = st.get(seg_rowid(sg.id, p)).ok_or(Corrupt)?;
        if a.len() < 4 {
            return Err(Corrupt);
        }
        let (ptr, szleaf) = (get_u16(a, 0), get_u16(a, 2));
        let terms = szleaf < a.len();
        let end = if terms {
            let mut i = szleaf;
            gv(a, &mut i).ok_or(Corrupt)? as usize
        } else {
            szleaf
        };
        if ptr != 0 && ptr < end {
            let mut i = ptr;
            if (gv(a, &mut i).ok_or(Corrupt)? as i64) > rowid {
                break;
            }
            best = Some(p);
        }
        if terms {
            break;
        }
        p = match p.checked_add(1) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(best)
}

/// How far a write past a page's end may grow it (a leaf page is at
/// most 64 KiB; anything further is corrupt input, left unwritten).
const MAX_GROW: usize = 1 << 16;

/// A page with the pin's zero padding past its end
/// (`FTS5_DATA_PADDING`), so reads past `nn` see zeros as there.
struct Pg {
    a: AVec<u8>,
}

impl Pg {
    fn new(b: &[u8]) -> Pg {
        let mut a = copy(b);
        grow(&mut a, b.len() + 20);
        Pg { a }
    }

    fn at(&self, i: usize) -> u8 {
        self.a.as_slice().get(i).copied().unwrap_or(0)
    }

    /// `fts5GetVarint`: the value and its length.
    fn var(&self, i: usize) -> (u64, usize) {
        let mut k = i;
        match gv(self.a.as_slice(), &mut k) {
            Some(v) => (v, k - i),
            None => (0, 1),
        }
    }

    /// `sqlite3Fts5PutVarint` at `i`: the bytes written.
    fn put_var(&mut self, i: usize, v: u64) -> usize {
        let mut b = AVec::new();
        pv(&mut b, v);
        self.put(i, b.as_slice());
        b.len()
    }

    fn put(&mut self, i: usize, b: &[u8]) {
        let end = i.saturating_add(b.len());
        if end > self.a.len().saturating_add(MAX_GROW) {
            return;
        }
        if self.a.len() < end {
            grow(&mut self.a, end);
        }
        if let Some(s) = self.a.as_mut_slice().get_mut(i..end) {
            s.copy_from_slice(b);
        }
    }

    /// `memmove(&a[to], &a[from], n)`.
    fn mov(&mut self, to: usize, from: usize, n: usize) {
        let need = from.max(to).saturating_add(n);
        if need > self.a.len().saturating_add(MAX_GROW) {
            return;
        }
        if self.a.len() < need {
            grow(&mut self.a, need);
        }
        if need <= self.a.len() {
            self.a.as_mut_slice().copy_within(from..from + n, to);
        }
    }

    fn u16(&self, i: usize) -> usize {
        get_u16(self.a.as_slice(), i)
    }

    fn set_u16(&mut self, i: usize, v: usize) {
        put_u16(&mut self.a, i, v);
    }

    fn bytes(&self, n: usize) -> AVec<u8> {
        copy(self.a.as_slice().get(..n).unwrap_or(self.a.as_slice()))
    }
}

/// `fts5SecureDeleteIdxEntry`.
fn idx_entry_delete(st: &mut Store, segid: u32, pgno: u32) {
    if pgno != 1 {
        st.log.push(Op::IdxDelPg(segid, pgno));
    }
}

/// `fts5SecureDeleteOverflow`: the removed entry's position list
/// continues on page `pgno` and beyond; returns `bLastInDoclist`.
fn overflow(st: &mut Store, sg: &Seg, pgno: u32, none: bool) -> Result<bool, Corrupt> {
    let mut last = true;
    for pg in pgno..=sg.last {
        let id = seg_rowid(sg.id, pg);
        let p = Pg::new(st.get(id).ok_or(Corrupt)?);
        let nn = p.a.len() - 20;
        let szleaf = p.u16(2);
        let mut inext = p.u16(0);
        if inext != 0 {
            last = false;
        }
        if inext == 0 && szleaf != nn {
            inext = p.var(szleaf).0 as u32 as usize;
        }
        if inext == 0 {
            if !none {
                st.put(id, copy(&[0, 0, 0, 4]));
            }
        } else if none {
            break;
        } else if inext >= szleaf || nn < szleaf || inext < 4 {
            return Err(Corrupt);
        } else {
            let nshift = inext - 4;
            let mut aidx = AVec::new();
            if nn > szleaf {
                let (first, n1) = p.var(szleaf);
                let first = first as u32 as usize;
                if first < inext {
                    return Err(Corrupt);
                }
                pv(&mut aidx, (first - nshift) as u64);
                let i1 = szleaf + n1;
                if i1 < nn {
                    aidx.extend_from_slice(p.a.as_slice().get(i1..nn).ok_or(Corrupt)?);
                }
            }
            let npg = szleaf - nshift;
            let mut out = copy(p.a.as_slice().get(..4).ok_or(Corrupt)?);
            out.extend_from_slice(p.a.as_slice().get(4 + nshift..szleaf).ok_or(Corrupt)?);
            put_u16(&mut out, 2, npg);
            if get_u16(out.as_slice(), 0) != 0 {
                put_u16(&mut out, 0, 4);
            }
            out.extend_from_slice(aidx.as_slice());
            st.put(id, out);
            break;
        }
    }
    Ok(last)
}

/// `fts5DoSecureDelete`: remove the entry `l` stands on from segment
/// `sg` (term `key`).
fn do_secure_delete(
    st: &mut Store,
    sg: &Seg,
    l: &Loc,
    key: &[u8],
    detail: Detail,
) -> Result<(), Corrupt> {
    let none = detail == Detail::None;
    let segid = sg.id;
    let mut p = Pg::new(st.get(seg_rowid(segid, l.leaf_pg)).ok_or(Corrupt)?);
    let npg0 = p.a.len() - 20;
    let mut ipgidx = p.u16(2);
    if ipgidx > npg0 {
        return Err(Corrupt);
    }
    let aidx = copy(p.a.as_slice().get(ipgidx..npg0).ok_or(Corrupt)?);
    let nidx = aidx.len();
    let idx_var = |i: usize| -> (usize, usize) {
        let mut k = i;
        match gv(aidx.as_slice(), &mut k) {
            Some(v) => (v as u32 as usize, k - i),
            None => (0, 1),
        }
    };

    // iStart, iDelta, iNextOff.
    let mut istart = if l.leaf_pg == l.term_pg {
        l.term_off
    } else {
        p.u16(0)
    };
    let (mut idelta, n) = p.var(istart);
    let mut isop = istart + n;
    let mut inextoff;
    if none {
        while isop < l.leaf_off {
            if p.at(isop) == 0 {
                isop += 1;
            }
            if p.at(isop) == 0 {
                isop += 1;
            }
            istart = isop;
            let (d, n) = p.var(istart);
            idelta = d;
            isop = istart + n;
        }
        inextoff = isop;
        if inextoff < l.eod && p.at(inextoff) == 0 {
            inextoff += 1;
        }
        if inextoff < l.eod && p.at(inextoff) == 0 {
            inextoff += 1;
        }
    } else {
        let (np, n) = p.var(isop);
        let mut npos = np as u32 as usize;
        isop += n;
        while isop < l.leaf_off {
            istart = isop.saturating_add(npos / 2);
            let (d, n) = p.var(istart);
            idelta = d;
            isop = istart + n;
            let (np, n) = p.var(isop);
            npos = np as u32 as usize;
            isop += n;
        }
        inextoff = l.leaf_off.saturating_add(l.npos);
    }
    let mut ioff = istart;

    let mut blast = false;
    if inextoff >= ipgidx {
        blast = overflow(st, sg, l.leaf_pg + 1, none)?;
        inextoff = ipgidx;
    }
    if !l.bdel {
        if inextoff != ipgidx {
            let (mut i, mut keyoff) = (0usize, 0usize);
            while i < nidx {
                let (v, n) = idx_var(i);
                i += n;
                keyoff = keyoff.saturating_add(v);
                if keyoff == inextoff {
                    blast = true;
                }
            }
        }
        if p.u16(0) == istart && (blast || inextoff == ipgidx) {
            p.set_u16(0, 0);
        }
    }

    let mut idelkeyoff = 0usize;
    if l.bdel {
        ioff += p.put_var(ioff, idelta);
        p.put(ioff, &[0x01]);
        ioff += 1;
    } else if !blast {
        if inextoff != ipgidx {
            let (nd, n) = p.var(inextoff);
            inextoff += n;
            ioff += p.put_var(ioff, idelta.wrapping_add(nd));
        }
    } else if l.leaf_pg == l.term_pg && istart == l.term_off {
        // The entry was its term's only one: the term goes too.
        let (mut i, mut ikey, mut keyoff) = (0usize, 0usize, 0usize);
        while i < nidx {
            let (v, n) = idx_var(i);
            i += n;
            if keyoff.saturating_add(v) > istart {
                break;
            }
            keyoff += v;
            ikey += 1;
        }
        idelkeyoff = keyoff;
        ioff = keyoff;
        if inextoff != ipgidx {
            // The next term on the page takes the removed term's key.
            idelkeyoff = inextoff;
            let (np2, n) = p.var(inextoff);
            inextoff += n;
            let (ns2, n) = p.var(inextoff);
            inextoff += n;
            let (nprefix2, nsuffix2) = (np2 as u32 as usize, ns2 as u32 as usize);
            let mut kk = keyoff;
            let mut nprefix = 0usize;
            if ikey != 1 {
                let (v, n) = p.var(kk);
                kk += n;
                nprefix = v as u32 as usize;
            }
            let (v, n) = p.var(kk);
            kk += n;
            let _ = v;
            nprefix = nprefix.min(nprefix2);
            let nsuffix = nprefix2.wrapping_add(nsuffix2).wrapping_sub(nprefix);
            if kk.saturating_add(nsuffix) > ipgidx || inextoff.saturating_add(nsuffix2) > ipgidx {
                return Err(Corrupt);
            }
            if ikey != 1 {
                ioff += p.put_var(ioff, nprefix as u64);
            }
            ioff += p.put_var(ioff, nsuffix as u64);
            if nprefix2 > key.len() {
                return Err(Corrupt);
            } else if nprefix2 > nprefix {
                p.put(ioff, key.get(nprefix..nprefix2).ok_or(Corrupt)?);
                ioff += nprefix2 - nprefix;
            }
            p.mov(ioff, inextoff, nsuffix2);
            ioff += nsuffix2;
            inextoff += nsuffix2;
        }
    } else if istart == 4 {
        // The entry may have been its doclist's only one, the term at
        // the end of an earlier page.
        let mut ipgno = l.leaf_pg.saturating_sub(1);
        while ipgno > l.term_pg {
            let empty = st
                .get(seg_rowid(segid, ipgno))
                .is_some_and(|b| b.len() == 4);
            if !empty {
                break;
            }
            ipgno -= 1;
        }
        if ipgno == l.term_pg {
            let iid = seg_rowid(segid, l.term_pg);
            if let Some(b) = st.get(iid) {
                let mut t = Pg::new(b);
                let tnn = t.a.len() - 20;
                let tsz = t.u16(2);
                if tsz == l.term_off && tsz <= tnn {
                    let ntidx0 = tnn - tsz;
                    let (mut iti, mut itoff) = (0usize, 0usize);
                    loop {
                        let (v, n) = t.var(tsz + iti);
                        itoff = itoff.saturating_add(v as u32 as usize);
                        if iti + n >= ntidx0 {
                            break;
                        }
                        iti += n;
                    }
                    let ntidx = iti;
                    t.mov(itoff, tsz, ntidx);
                    t.set_u16(2, itoff);
                    st.put(iid, t.bytes(itoff + ntidx));
                    if ntidx == 0 {
                        idx_entry_delete(st, segid, l.term_pg);
                    }
                }
            }
        }
    }

    // The page: the bytes after the entry moved down, the page index
    // rebuilt without the removed key.
    // (`nShift` can be negative: a detail=none entry with delete flags
    // makes the pin's walk overshoot, and it grows the page by the
    // difference — reproduced as it writes it.)
    let nmove = npg0.checked_sub(inextoff).ok_or(Corrupt)?;
    let nshift = inextoff as i64 - ioff as i64;
    p.mov(ioff, inextoff, nmove);
    let np = ipgidx as i64 - nshift;
    if np < 4 || np > npg0 as i64 + 16 {
        return Err(Corrupt);
    }
    ipgidx = np as usize;
    let mut npg = ipgidx;
    p.set_u16(2, ipgidx);
    let (mut i, mut keyin, mut prevout) = (0usize, 0usize, 0i64);
    while i < nidx {
        let (v, n) = idx_var(i);
        i += n;
        keyin = keyin.saturating_add(v);
        if keyin != idelkeyoff {
            let keyout = keyin as i64 - if keyin > ioff { nshift } else { 0 };
            npg += p.put_var(npg, keyout.wrapping_sub(prevout) as u64);
            prevout = keyout;
        }
    }
    if ipgidx == npg && nidx > 0 && l.leaf_pg != 1 {
        idx_entry_delete(st, segid, l.leaf_pg);
    }
    st.put(seg_rowid(segid, l.leaf_pg), p.bytes(npg));
    Ok(())
}

/// `fts5FlushSecureDelete`: remove (`key`, `rowid`) from the newest
/// segment of `s` holding it, unless that segment's tombstone hash
/// already hides the row (review fix: the entry was cut out).
fn secure_delete(
    st: &mut Store,
    s: &Structure,
    key: &[u8],
    rowid: i64,
    detail: Detail,
    x: &mut SdFlush,
) -> Result<(), Corrupt> {
    if !x.v5 {
        x.v5 = true;
        x.bumped = true;
    }
    for sg in s.newest_first().iter() {
        if let Some(l) = locate(st, sg, key, rowid, detail)? {
            // The pin finds the entry with a segment iterator, which
            // skips a rowid in the segment's tombstone hash
            // (`fts5MultiIterIsDeleted`): a tombstoned entry stays.
            if sg.npgt > 0 && tomb_hit(tomb_pages(st, sg)?.as_slice(), rowid) {
                return Ok(());
            }
            return do_secure_delete(st, sg, &l, key, detail);
        }
    }
    Ok(())
}

/// One pending term's doclist flushed in secure-delete mode (the
/// `bSecureDelete` branch of `fts5FlushOneHash`): its deletes edit
/// the segments of `s`, the rest is written to `w`, the term only
/// once something of it is.
pub(super) fn flush_term(
    st: &mut Store,
    w: &mut Writer,
    s: &Structure,
    key: &[u8],
    dl: &[u8],
    detail: Detail,
    x: &mut SdFlush,
) -> Result<(), Corrupt> {
    let pgsz = w.pgsz;
    let ndl = dl.len();
    let mut written = false;
    let (mut rowid, mut prev, mut off) = (0i64, 0i64, 0usize);
    while off < ndl {
        let d = gv(dl, &mut off).ok_or(Corrupt)?;
        rowid = rowid.wrapping_add(d as i64);
        if detail == Detail::None {
            if off < ndl && dl.get(off) == Some(&0) {
                secure_delete(st, s, key, rowid, detail, x)?;
                off += 1;
                // A delete and a new entry: the rowid is written again.
                if off < ndl && dl.get(off) == Some(&0) {
                    off += 1;
                } else {
                    continue;
                }
            }
        } else if dl.get(off).is_some_and(|b| b & 1 != 0) {
            secure_delete(st, s, key, rowid, detail, x)?;
            if dl.get(off) == Some(&1) {
                off += 1;
                continue;
            }
        }
        if !written {
            w.append_term(st, key);
            written = true;
        }
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
            if off < ndl && dl.get(off) == Some(&0) {
                w.buf.push(0);
                off += 1;
                if off < ndl && dl.get(off) == Some(&0) {
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
            let npos = (sz >> 1) as usize;
            let ncopy = if sz & 1 == 1 {
                pv(&mut w.buf, (npos as u64) * 2);
                off = k;
                npos
            } else {
                (k - off) + npos
            };
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
    Ok(())
}
