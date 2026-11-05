//! Table B-tree layer (SPEC §5.4): page/cell codec, scan cursor,
//! insert with leaf/interior/root splits, overflow chains, and
//! repack-on-write defragmentation.
//!
//! Mutation strategy (v0.1): a changed page is rebuilt from its
//! parsed cell list and rewritten whole. That makes defragmentation
//! and freeblock handling correct by construction — live cells are
//! repacked, freeblocks and fragmented bytes vanish — at O(page)
//! cost per insert. The format's logical rules (sorted pointer
//! array, divider keys, overflow thresholds) are all honoured.

use crate::arena::AVec;
use crate::error::{Error, Result, SchemaKind};
use crate::pager::{Pager, be_u16, be_u32, put_be_u16, put_be_u32};
use crate::record::{decode_varint, encode_varint};
use crate::storage::Storage;

#[cfg(feature = "indexes")]
use crate::record::{KeyOrder, compare_coll, compare_coll_in};
#[cfg(feature = "indexes")]
use crate::record::{OwnedValue, Value, decode_record, encode_record};

pub const KIND_INTERIOR_INDEX: u8 = 0x02;
pub const KIND_INTERIOR_TABLE: u8 = 0x05;
pub const KIND_LEAF_INDEX: u8 = 0x0A;
pub const KIND_LEAF_TABLE: u8 = 0x0D;

#[derive(Debug, Clone, Copy)]
pub struct PageHead {
    pub kind: u8,
    pub first_freeblock: u16,
    pub ncell: u16,
    /// Absolute offset where cell content begins (65536 for 0).
    pub content_start: usize,
    pub frag: u8,
    pub rightmost: u32,
    pub hdr_off: usize,
    pub hdr_len: usize,
}

impl PageHead {
    pub fn is_leaf(&self) -> bool {
        self.kind == KIND_LEAF_TABLE || self.kind == KIND_LEAF_INDEX
    }
    pub fn cell_ptrs_off(&self) -> usize {
        self.hdr_off + self.hdr_len
    }
}

pub fn head<E>(bytes: &[u8], page: u32) -> Result<PageHead, E> {
    let hdr_off = if page == 1 { 100 } else { 0 };
    if bytes.len() < hdr_off + 8 {
        return Err(Error::CorruptPage { page });
    }
    let kind = bytes[hdr_off];
    if !matches!(
        kind,
        KIND_INTERIOR_INDEX | KIND_INTERIOR_TABLE | KIND_LEAF_INDEX | KIND_LEAF_TABLE
    ) {
        return Err(Error::CorruptPage { page });
    }
    let leaf = kind == KIND_LEAF_TABLE || kind == KIND_LEAF_INDEX;
    let hdr_len = if leaf { 8 } else { 12 };
    if bytes.len() < hdr_off + hdr_len {
        return Err(Error::CorruptPage { page });
    }
    let cs = be_u16(bytes, hdr_off + 5);
    let content_start = if cs == 0 { 65536 } else { usize::from(cs) };
    Ok(PageHead {
        kind,
        first_freeblock: be_u16(bytes, hdr_off + 1),
        ncell: be_u16(bytes, hdr_off + 3),
        content_start,
        frag: bytes[hdr_off + 7],
        rightmost: if leaf { 0 } else { be_u32(bytes, hdr_off + 8) },
        hdr_off,
        hdr_len,
    })
}

fn cell_ptr<E>(bytes: &[u8], head: &PageHead, i: usize, page: u32) -> Result<usize, E> {
    let off = head.cell_ptrs_off() + 2 * i;
    if off + 2 > bytes.len() {
        return Err(Error::CorruptPage { page });
    }
    Ok(usize::from(be_u16(bytes, off)))
}

/// Local-payload byte count for a table leaf cell (SQFMT §1.6):
/// X = U-35; if P > X, K = M + ((P-M) mod (U-4)) with
/// M = ((U-12)*32/255)-23; keep K if K <= X else M.
pub fn table_leaf_local(usable: usize, payload_len: usize) -> usize {
    let x = usable - 35;
    if payload_len <= x {
        return payload_len;
    }
    let m = (usable - 12) * 32 / 255 - 23;
    let k = m + (payload_len - m) % (usable - 4);
    if k <= x { k } else { m }
}

#[derive(Clone)]
pub struct LeafCell {
    pub rowid: i64,
    /// The full on-page cell image (varints + local payload + the
    /// 4-byte overflow pointer when present).
    pub bytes: AVec<u8>,
    pub payload_len: usize,
    pub local_len: usize,
    pub overflow: u32,
}

impl LeafCell {
    /// The local payload bytes, from the cell's own image.
    pub fn local_bytes(&self) -> &[u8] {
        let Ok((_, n1)) = decode_varint::<()>(self.bytes.as_slice()) else {
            return &[];
        };
        let Ok((_, n2)) = decode_varint::<()>(&self.bytes.as_slice()[n1..]) else {
            return &[];
        };
        &self.bytes.as_slice()[n1 + n2..n1 + n2 + self.local_len]
    }
}

fn parse_leaf_cell<E>(bytes: &[u8], at: usize, usable: usize, page: u32) -> Result<LeafCell, E> {
    // `at` comes from the page's cell-pointer array — untrusted.
    let cell_bytes = bytes.get(at..).ok_or(Error::CorruptPage { page })?;
    let (plen, n1) = decode_varint::<E>(cell_bytes)?;
    let rest = cell_bytes.get(n1..).ok_or(Error::CorruptPage { page })?;
    let (rowid, n2) = decode_varint::<E>(rest)?;
    let plen = plen as usize;
    let local = table_leaf_local(usable, plen);
    let start = at + n1 + n2;
    let end = start
        .checked_add(local)
        .ok_or(Error::CorruptPage { page })?;
    if end > bytes.len() {
        return Err(Error::CorruptPage { page });
    }
    let mut overflow = 0u32;
    let mut total = n1 + n2 + local;
    if plen > local {
        if end + 4 > bytes.len() {
            return Err(Error::CorruptPage { page });
        }
        overflow = be_u32(bytes, end);
        total += 4;
    }
    let mut img = AVec::with_capacity(total);
    img.extend_from_slice(&bytes[at..at + total]);
    Ok(LeafCell {
        rowid: rowid as i64,
        bytes: img,
        payload_len: plen,
        local_len: local,
        overflow,
    })
}

/// Metadata for one leaf cell, pointing into a page snapshot the
/// cursor owns — the scan cache parses a page with O(1)
/// allocations instead of one image copy per cell (SPEC §4.1).
#[derive(Clone, Copy)]
pub struct CellMeta {
    pub rowid: i64,
    pub payload_len: usize,
    pub local_off: usize,
    pub local_len: usize,
    pub overflow: u32,
}

/// Parse a leaf page's cell metadata (no payload copies).
pub fn leaf_cells_meta<E>(bytes: &[u8], page: u32, usable: usize) -> Result<AVec<CellMeta>, E> {
    let h = head::<E>(bytes, page)?;
    if h.kind == KIND_LEAF_INDEX || h.kind == KIND_INTERIOR_INDEX {
        return Err(Error::Unsupported);
    }
    if h.kind != KIND_LEAF_TABLE {
        return Err(Error::CorruptPage { page });
    }
    let mut out = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let at = cell_ptr::<E>(bytes, &h, i, page)?;
        let cell_bytes = bytes.get(at..).ok_or(Error::CorruptPage { page })?;
        let (plen, n1) = decode_varint::<E>(cell_bytes)?;
        let rest = cell_bytes.get(n1..).ok_or(Error::CorruptPage { page })?;
        let (rowid, n2) = decode_varint::<E>(rest)?;
        let plen = plen as usize;
        let local = table_leaf_local(usable, plen);
        let start = at + n1 + n2;
        let end = start
            .checked_add(local)
            .ok_or(Error::CorruptPage { page })?;
        if end > bytes.len() {
            return Err(Error::CorruptPage { page });
        }
        let mut overflow = 0u32;
        if plen > local {
            if end + 4 > bytes.len() {
                return Err(Error::CorruptPage { page });
            }
            overflow = be_u32(bytes, end);
        }
        out.push(CellMeta {
            rowid: rowid as i64,
            payload_len: plen,
            local_off: start,
            local_len: local,
            overflow,
        });
    }
    Ok(out)
}

/// Parse all cells of a table leaf page from a page image.
pub fn leaf_cells<E>(bytes: &[u8], page: u32, usable: usize) -> Result<AVec<LeafCell>, E> {
    let h = head::<E>(bytes, page)?;
    if h.kind == KIND_LEAF_INDEX || h.kind == KIND_INTERIOR_INDEX {
        return Err(Error::Unsupported);
    }
    if h.kind != KIND_LEAF_TABLE {
        return Err(Error::CorruptPage { page });
    }
    let mut out = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let ptr = cell_ptr::<E>(bytes, &h, i, page)?;
        out.push(parse_leaf_cell::<E>(bytes, ptr, usable, page)?);
    }
    Ok(out)
}

/// Parse an interior table page: (child, divider-key) pairs plus the
/// rightmost child. A pair's key is the largest rowid in that child's
/// subtree (SPEC §5.4).
pub fn interior_cells<E>(bytes: &[u8], page: u32) -> Result<(AVec<(u32, i64)>, u32), E> {
    let h = head::<E>(bytes, page)?;
    if h.kind == KIND_LEAF_INDEX || h.kind == KIND_INTERIOR_INDEX {
        return Err(Error::Unsupported);
    }
    if h.kind != KIND_INTERIOR_TABLE {
        return Err(Error::CorruptPage { page });
    }
    let mut out = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let ptr = cell_ptr::<E>(bytes, &h, i, page)?;
        if ptr + 4 > bytes.len() {
            return Err(Error::CorruptPage { page });
        }
        let child = be_u32(bytes, ptr);
        let (key, _) = decode_varint::<E>(&bytes[ptr + 4..])?;
        out.push((child, key as i64));
    }
    Ok((out, h.rightmost))
}

fn build_leaf_image(page: u32, page_size: usize, cells: &[LeafCell], base: &[u8]) -> AVec<u8> {
    let mut img = vec_zeroed(page_size);
    if page == 1 {
        img.as_mut_slice()[0..100].copy_from_slice(&base[0..100]);
    }
    let hdr_off = if page == 1 { 100 } else { 0 };
    let out = img.as_mut_slice();
    let mut content = page_size;
    let mut ptrs = AVec::new();
    for c in cells {
        content -= c.bytes.len();
        out[content..content + c.bytes.len()].copy_from_slice(c.bytes.as_slice());
        ptrs.push(content as u16);
    }
    out[hdr_off] = KIND_LEAF_TABLE;
    put_be_u16(out, hdr_off + 1, 0);
    put_be_u16(out, hdr_off + 3, cells.len() as u16);
    put_be_u16(out, hdr_off + 5, content_code(content));
    out[hdr_off + 7] = 0;
    for (i, p) in ptrs.iter().enumerate() {
        put_be_u16(out, hdr_off + 8 + 2 * i, *p);
    }
    img
}

fn build_interior_image(
    page: u32,
    page_size: usize,
    pairs: &[(u32, i64)],
    rightmost: u32,
    base: &[u8],
) -> AVec<u8> {
    let mut img = vec_zeroed(page_size);
    if page == 1 {
        img.as_mut_slice()[0..100].copy_from_slice(&base[0..100]);
    }
    let hdr_off = if page == 1 { 100 } else { 0 };
    let out = img.as_mut_slice();
    let mut content = page_size;
    let mut ptrs = AVec::new();
    for (child, key) in pairs {
        let mut cell = AVec::new();
        for b in child.to_be_bytes() {
            cell.push(b);
        }
        encode_varint(*key as u64, &mut cell);
        content -= cell.len();
        out[content..content + cell.len()].copy_from_slice(cell.as_slice());
        ptrs.push(content as u16);
    }
    out[hdr_off] = KIND_INTERIOR_TABLE;
    put_be_u16(out, hdr_off + 1, 0);
    put_be_u16(out, hdr_off + 3, pairs.len() as u16);
    put_be_u16(out, hdr_off + 5, content_code(content));
    out[hdr_off + 7] = 0;
    put_be_u32(out, hdr_off + 8, rightmost);
    for (i, p) in ptrs.iter().enumerate() {
        put_be_u16(out, hdr_off + 12 + 2 * i, *p);
    }
    img
}

fn content_code(content: usize) -> u16 {
    if content == 65536 { 0 } else { content as u16 }
}

fn vec_zeroed(n: usize) -> AVec<u8> {
    let mut a = AVec::new();
    for _ in 0..n {
        a.push(0);
    }
    a
}

fn copy_bytes(b: &[u8]) -> AVec<u8> {
    let mut a = AVec::with_capacity(b.len());
    a.extend_from_slice(b);
    a
}

/// Bytes a leaf page needs for `cells` (header + pointer array +
/// content), checked against usable size.
fn leaf_need(cells: &[LeafCell], hdr_off: usize) -> usize {
    let content: usize = cells.iter().map(|c| c.bytes.len()).sum();
    hdr_off + 8 + 2 * cells.len() + content
}

fn interior_need(pairs: &[(u32, i64)], hdr_off: usize) -> usize {
    let mut content = 0usize;
    for (_, k) in pairs {
        let mut v = AVec::new();
        encode_varint(*k as u64, &mut v);
        content += 4 + v.len();
    }
    hdr_off + 12 + 2 * pairs.len() + content
}

// ----------------------------------------------------------------
// Reading
// ----------------------------------------------------------------

/// Reassemble a full payload from its local part + overflow chain.
pub async fn assemble_payload<S: Storage>(
    pager: &mut Pager<S>,
    local: &[u8],
    total_len: usize,
    first_overflow: u32,
) -> Result<AVec<u8>, S::Error> {
    let mut out = AVec::new();
    assemble_payload_into(pager, local, total_len, first_overflow, &mut out).await?;
    Ok(out)
}

/// `assemble_payload` into a caller-owned, reusable buffer (the
/// steady-state scan path; SPEC §4.1).
pub async fn assemble_payload_into<S: Storage>(
    pager: &mut Pager<S>,
    local: &[u8],
    total_len: usize,
    first_overflow: u32,
    out: &mut AVec<u8>,
) -> Result<(), S::Error> {
    out.clear();
    for b in local {
        out.push(*b);
    }
    let usable = pager.usable() as usize;
    let per_page = usable - 4;
    let mut next = first_overflow;
    let mut hops = 0u32;
    while out.len() < total_len {
        if next == 0 {
            return Err(Error::CorruptRecord);
        }
        hops += 1;
        if hops > pager.page_count() + 1 {
            return Err(Error::OverflowChainCycle);
        }
        let idx = pager.load(next).await?;
        let (following, chunk) = {
            let d = pager.frame_data(idx);
            if d.len() < 4 {
                return Err(Error::CorruptPage { page: next });
            }
            let following = be_u32(d, 0);
            let want = total_len - out.len();
            let take = want.min(per_page);
            if 4 + take > d.len() {
                return Err(Error::CorruptPage { page: next });
            }
            (following, copy_bytes(&d[4..4 + take]))
        };
        for b in chunk.iter() {
            out.push(*b);
        }
        next = following;
    }
    // A UTF-16 database's record reads as UTF-8 above this layer
    // (v0.16, `utf16`; `utf16::transcode_record`).
    #[cfg(feature = "utf16")]
    if let Some(be) = crate::utf16::byte_order(pager.text_enc()) {
        let raw = core::mem::take(out);
        crate::utf16::transcode_record::<S::Error>(raw.as_slice(), be, false, out)?;
    }
    Ok(())
}

/// The bytes a record is stored as (v0.16, `utf16`): None in a
/// UTF-8 database (the record itself), else its TEXT values in the
/// file's UTF-16.
#[cfg(feature = "utf16")]
fn stored_record<E>(enc: crate::record::TextEnc, rec: &[u8]) -> Result<Option<AVec<u8>>, E> {
    match crate::utf16::byte_order(enc) {
        None => Ok(None),
        Some(be) => {
            let mut out = AVec::new();
            crate::utf16::transcode_record::<E>(rec, be, true, &mut out)?;
            Ok(Some(out))
        }
    }
}

enum ScanFrame {
    Unresolved { page: u32 },
    Interior { page: u32, next_child: usize },
    Leaf { page: u32, next_cell: usize },
}

/// A full-scan cursor over a table B-tree (SPEC §5.4). State lives
/// in the cursor (hence in its owning `Statement`), never in a
/// future, so it spans suspensions intact (§5.1.2).
pub struct ScanCursor {
    stack: AVec<ScanFrame>,
    done: bool,
    /// Snapshot + cell-metadata cache for the leaf currently being
    /// yielded from: a page is loaded and parsed once, not once
    /// per row (SPEC §4.1's steady-state scan-loop gate).
    leaf_cache_page: u32,
    leaf_img: AVec<u8>,
    leaf_meta: AVec<CellMeta>,
    /// Rows below this rowid are skipped, interior pages descending
    /// straight to it (v0.16, `expr_index`: a rowid range scan);
    /// cleared once a row at or past it is reached.
    #[cfg(feature = "expr_index")]
    min: Option<i64>,
    /// The table's root, and — once the first row is asked for and
    /// the root is a WITHOUT ROWID table's clustered tree (v0.16,
    /// `without_rowid`) — the cursor over that tree.
    #[cfg(feature = "without_rowid")]
    root: u32,
    #[cfg(feature = "without_rowid")]
    wr: Option<(usize, alloc::boxed::Box<IndexCursor>)>,
    #[cfg(feature = "without_rowid")]
    wr_checked: bool,
}

impl ScanCursor {
    pub fn new(root: u32) -> ScanCursor {
        let mut stack = AVec::new();
        stack.push(ScanFrame::Unresolved { page: root });
        ScanCursor {
            stack,
            done: false,
            leaf_cache_page: 0,
            leaf_img: AVec::new(),
            leaf_meta: AVec::new(),
            #[cfg(feature = "expr_index")]
            min: None,
            #[cfg(feature = "without_rowid")]
            root,
            #[cfg(feature = "without_rowid")]
            wr: None,
            #[cfg(feature = "without_rowid")]
            wr_checked: false,
        }
    }

    /// A scan starting at the first row whose rowid is `min` or more
    /// (v0.16, `expr_index`).
    #[cfg(feature = "expr_index")]
    pub fn from_rowid(root: u32, min: i64) -> ScanCursor {
        let mut c = ScanCursor::new(root);
        c.min = Some(min);
        c
    }

    /// Advance to the next row: (rowid, full payload).
    pub async fn next_row<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
    ) -> Result<Option<(i64, AVec<u8>)>, S::Error> {
        let mut buf = AVec::new();
        match self.next_row_into(pager, &mut buf).await? {
            Some(rowid) => Ok(Some((rowid, buf))),
            None => Ok(None),
        }
    }

    /// Advance to the next row, assembling its payload into the
    /// caller's reusable buffer; returns the rowid.
    pub async fn next_row_into<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
        buf: &mut AVec<u8>,
    ) -> Result<Option<i64>, S::Error> {
        if self.done {
            return Ok(None);
        }
        // A WITHOUT ROWID table (v0.16): its clustered tree in key
        // order, each row in the table's column order under a
        // handle standing in for the rowid.
        #[cfg(feature = "without_rowid")]
        {
            if !self.wr_checked {
                self.wr_checked = true;
                if let Some(t) = pager.wr.table_of(self.root) {
                    let cur = crate::wr::clustered_cursor(pager, t);
                    self.wr = Some((t, alloc::boxed::Box::new(cur)));
                }
            }
            if let Some((t, cur)) = self.wr.as_mut() {
                // The layouts may have been rebuilt since (a schema
                // change part-way through the statement).
                *t = pager.wr.table_of(self.root).ok_or(Error::Unsupported)?;
                let r = crate::wr::scan_next(pager, *t, cur, buf).await?;
                if r.is_none() {
                    self.done = true;
                }
                return Ok(r);
            }
        }
        let usable = pager.usable() as usize;
        loop {
            let (page, mode) = match self.stack.as_slice().last() {
                Some(ScanFrame::Unresolved { page }) => (*page, 0),
                Some(ScanFrame::Interior { page, .. }) => (*page, 1),
                Some(ScanFrame::Leaf { page, .. }) => (*page, 2),
                None => {
                    self.done = true;
                    return Ok(None);
                }
            };
            let idx = pager.load(page).await?;
            // Read the header straight from the frame: parsing
            // completes before any further pager call that could
            // evict it, and parsed cells are cursor-owned clones.
            let h = head::<S::Error>(pager.frame_data(idx), page)?;
            if mode == 0 {
                // Classify and replace the unresolved frame.
                self.stack.pop();
                if h.is_leaf() {
                    if h.kind != KIND_LEAF_TABLE {
                        return Err(Error::Unsupported);
                    }
                    self.stack.push(ScanFrame::Leaf { page, next_cell: 0 });
                } else {
                    self.stack.push(ScanFrame::Interior {
                        page,
                        next_child: 0,
                    });
                }
                continue;
            }
            if mode == 2 {
                let ci = match self.stack.as_slice().last() {
                    Some(ScanFrame::Leaf { next_cell, .. }) => *next_cell,
                    _ => unreachable!(),
                };
                if ci >= usize::from(h.ncell) {
                    self.stack.pop();
                    continue;
                }
                if self.leaf_cache_page != page {
                    // Snapshot for the parse: assemble calls below
                    // may evict this frame mid-parse otherwise.
                    self.leaf_img = copy_bytes(pager.frame_data(idx));
                    self.leaf_meta =
                        leaf_cells_meta::<S::Error>(self.leaf_img.as_slice(), page, usable)?;
                    self.leaf_cache_page = page;
                }
                let cell = self.leaf_meta.as_slice()[ci];
                let (rowid, plen, ovf) = (cell.rowid, cell.payload_len, cell.overflow);
                #[cfg(feature = "expr_index")]
                if let Some(m) = self.min {
                    if rowid < m {
                        if let Some(ScanFrame::Leaf { next_cell, .. }) =
                            self.stack.as_mut_slice().last_mut()
                        {
                            *next_cell = ci + 1;
                        }
                        continue;
                    }
                    self.min = None;
                }
                let local =
                    &self.leaf_img.as_slice()[cell.local_off..cell.local_off + cell.local_len];
                // Checkpoint progress BEFORE the payload awaits
                // (SPEC §5.1.1): a dropped future re-produces this
                // row rather than skipping it — the executor only
                // advances its own copy after a completed step.
                assemble_payload_into(pager, local, plen, ovf, buf).await?;
                if let Some(ScanFrame::Leaf { next_cell, .. }) =
                    self.stack.as_mut_slice().last_mut()
                {
                    *next_cell = ci + 1;
                }
                return Ok(Some(rowid));
            }
            // Interior: descend into the next child.
            let ci = match self.stack.as_slice().last() {
                Some(ScanFrame::Interior { next_child, .. }) => *next_child,
                _ => unreachable!(),
            };
            let (pairs, rightmost) = interior_cells::<S::Error>(pager.frame_data(idx), page)?;
            // A rowid range scan's first descent (v0.16): straight to
            // the child that may hold its first row.
            #[cfg(feature = "expr_index")]
            let ci = match self.min {
                Some(m) if ci == 0 => pairs
                    .iter()
                    .position(|(_, k)| *k >= m)
                    .unwrap_or(pairs.len()),
                _ => ci,
            };
            if ci > pairs.len() {
                self.stack.pop();
                continue;
            }
            let child = if ci < pairs.len() {
                pairs.as_slice()[ci].0
            } else {
                rightmost
            };
            if let Some(ScanFrame::Interior { next_child, .. }) =
                self.stack.as_mut_slice().last_mut()
            {
                *next_child = ci + 1;
            }
            self.stack.push(ScanFrame::Unresolved { page: child });
        }
    }
}

/// The rowid a new row without an explicit one takes in the table
/// rooted at `root` (v0.16, `v16`): one past the largest, 1 for an
/// empty table, and when the largest is 9223372036854775807 the
/// smallest unused positive rowid — the pin picks a random unused
/// one, and any unused rowid serves (v0.16 review fix: ANALYZE and
/// INSERT failed there). `DatabaseFull` when every positive rowid
/// is taken. Without `v16` such an insert fails.
#[cfg(feature = "v16")]
pub async fn new_rowid<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<i64, S::Error> {
    // A WITHOUT ROWID table's new row (v0.16): a fresh handle.
    #[cfg(feature = "without_rowid")]
    if pager.wr.table_of(root).is_some() {
        return Ok(pager.wr.fresh(root));
    }
    let max = seek_max_rowid(pager, root).await?;
    if let Some(n) = max.map_or(Some(1), |m| m.checked_add(1)) {
        return Ok(n);
    }
    let mut cur = ScanCursor::new(root);
    let mut buf = AVec::new();
    let mut want = 1i64;
    while let Some(id) = cur.next_row_into(pager, &mut buf).await? {
        if id > want {
            break;
        }
        if id == want {
            want = want.checked_add(1).ok_or(Error::Schema {
                kind: SchemaKind::DatabaseFull,
            })?;
        }
    }
    Ok(want)
}

/// Largest rowid in the table rooted at `root`, if any.
pub async fn seek_max_rowid<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
) -> Result<Option<i64>, S::Error> {
    // A WITHOUT ROWID table has no rowids (v0.16): its rows take
    // fresh handles (`new_rowid`).
    #[cfg(feature = "without_rowid")]
    if pager.wr.table_of(root).is_some() {
        return Ok(None);
    }
    let usable = pager.usable() as usize;
    let mut page = root;
    loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_TABLE {
                return Err(Error::Unsupported);
            }
            if h.ncell == 0 {
                return Ok(None);
            }
            let cells = leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            return Ok(Some(cells.as_slice()[cells.len() - 1].rowid));
        }
        let (_, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
        page = rightmost;
    }
}

#[cfg(feature = "expr_index")]
/// One level of `seek_rowid`.
enum SeekStep {
    Child(u32),
    Missing,
    Found {
        local_off: usize,
        local_len: usize,
        payload_len: usize,
        overflow: u32,
    },
}

#[cfg(feature = "expr_index")]
/// The rowid key of table-page cell `i` (and, on a leaf, the
/// offset just past its varints and its payload length; on an
/// interior page, its child).
fn cell_key<E>(
    bytes: &[u8],
    h: &PageHead,
    i: usize,
    page: u32,
    leaf: bool,
) -> Result<(i64, usize, u64), E> {
    let at = cell_ptr::<E>(bytes, h, i, page)?;
    if leaf {
        let c = bytes.get(at..).ok_or(Error::CorruptPage { page })?;
        let (plen, n1) = decode_varint::<E>(c)?;
        let (key, n2) = decode_varint::<E>(c.get(n1..).ok_or(Error::CorruptPage { page })?)?;
        Ok((key as i64, at + n1 + n2, plen))
    } else {
        let child = bytes
            .get(at..at.saturating_add(4))
            .and_then(|c| <[u8; 4]>::try_from(c).ok())
            .map(u32::from_be_bytes)
            .ok_or(Error::CorruptPage { page })?;
        let (key, _) = decode_varint::<E>(bytes.get(at + 4..).ok_or(Error::CorruptPage { page })?)?;
        Ok((key as i64, 0, u64::from(child)))
    }
}

#[cfg(feature = "expr_index")]
fn seek_step<E>(bytes: &[u8], page: u32, usable: usize, rowid: i64) -> Result<SeekStep, E> {
    let h = head::<E>(bytes, page)?;
    let leaf = h.is_leaf();
    if h.kind
        != (if leaf {
            KIND_LEAF_TABLE
        } else {
            KIND_INTERIOR_TABLE
        })
    {
        return Err(Error::Unsupported);
    }
    // The first cell whose key is >= rowid.
    let (mut lo, mut hi) = (0usize, usize::from(h.ncell));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (k, ..) = cell_key::<E>(bytes, &h, mid, page, leaf)?;
        if k < rowid {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if !leaf {
        if lo >= usize::from(h.ncell) {
            return Ok(SeekStep::Child(h.rightmost));
        }
        let (_, _, child) = cell_key::<E>(bytes, &h, lo, page, false)?;
        return Ok(SeekStep::Child(child as u32));
    }
    if lo >= usize::from(h.ncell) {
        return Ok(SeekStep::Missing);
    }
    let (k, start, plen) = cell_key::<E>(bytes, &h, lo, page, true)?;
    if k != rowid {
        return Ok(SeekStep::Missing);
    }
    let plen = usize::try_from(plen).map_err(|_| Error::CorruptPage { page })?;
    let local = table_leaf_local(usable, plen);
    let end = start
        .checked_add(local)
        .ok_or(Error::CorruptPage { page })?;
    let overflow = if plen > local {
        bytes
            .get(end..end.saturating_add(4))
            .and_then(|c| <[u8; 4]>::try_from(c).ok())
            .map(u32::from_be_bytes)
            .ok_or(Error::CorruptPage { page })?
    } else {
        0
    };
    Ok(SeekStep::Found {
        local_off: start,
        local_len: local,
        payload_len: plen,
        overflow,
    })
}

/// Find one row by rowid.
pub async fn seek_rowid<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
) -> Result<Option<AVec<u8>>, S::Error> {
    // A WITHOUT ROWID table (v0.16): the row the handle names.
    #[cfg(feature = "without_rowid")]
    if let Some(t) = pager.wr.table_of(root) {
        return crate::wr::seek(pager, t, rowid).await;
    }
    // v0.16 review fix (`expr_index`): index-driven plans seek once
    // per row they find, so the leaf seeks by binary search.
    #[cfg(feature = "expr_index")]
    return seek_rowid_fast(pager, root, rowid).await;
    #[cfg(not(feature = "expr_index"))]
    {
        let usable = pager.usable() as usize;
        let mut page = root;
        loop {
            let idx = pager.load(page).await?;
            let snap = copy_bytes(pager.frame_data(idx));
            let h = head::<S::Error>(snap.as_slice(), page)?;
            if h.is_leaf() {
                if h.kind != KIND_LEAF_TABLE {
                    return Err(Error::Unsupported);
                }
                let cells = leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
                for c in cells.iter() {
                    if c.rowid == rowid {
                        let local = copy_bytes(c.local_bytes());
                        return Ok(Some(
                            assemble_payload(pager, local.as_slice(), c.payload_len, c.overflow)
                                .await?,
                        ));
                    }
                }
                return Ok(None);
            }
            let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
            let mut child = rightmost;
            for (c, k) in pairs.iter() {
                if rowid <= *k {
                    child = *c;
                    break;
                }
            }
            page = child;
        }
    }
}
/// `seek_rowid` by binary search over the borrowed frame (v0.16
/// review fix, `expr_index`): no page snapshot and no per-cell parse,
/// only the found cell's payload copied; the descent depth is bounded,
/// so a cyclic file errors.
#[cfg(feature = "expr_index")]
async fn seek_rowid_fast<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
) -> Result<Option<AVec<u8>>, S::Error> {
    let usable = pager.usable() as usize;
    let mut page = root;
    let mut hops = 0u32;
    loop {
        hops = hops.saturating_add(1);
        if hops > 64 {
            return Err(Error::CorruptPage { page });
        }
        let idx = pager.load(page).await?;
        let step = seek_step::<S::Error>(pager.frame_data(idx), page, usable, rowid)?;
        match step {
            SeekStep::Child(c) => page = c,
            SeekStep::Missing => return Ok(None),
            SeekStep::Found {
                local_off,
                local_len,
                payload_len,
                overflow,
            } => {
                let local = {
                    let d = pager.frame_data(idx);
                    let end = local_off
                        .checked_add(local_len)
                        .ok_or(Error::CorruptPage { page })?;
                    copy_bytes(d.get(local_off..end).ok_or(Error::CorruptPage { page })?)
                };
                return Ok(Some(
                    assemble_payload(pager, local.as_slice(), payload_len, overflow).await?,
                ));
            }
        }
    }
}

// ----------------------------------------------------------------
// Insertion
// ----------------------------------------------------------------

/// Write overflow pages for the tail of `payload` beyond its local
/// part; returns the first overflow page (0 = none). Newly allocated
/// pages are dirty and unjournaled — rollback truncates them away.
async fn write_overflow<S: Storage>(
    pager: &mut Pager<S>,
    payload: &[u8],
    local_len: usize,
) -> Result<u32, S::Error> {
    if payload.len() <= local_len {
        return Ok(0);
    }
    let usable = pager.usable() as usize;
    let per = usable - 4;
    let tail = &payload[local_len..];
    let npages = tail.len().div_ceil(per);
    let mut pages = AVec::new();
    for _ in 0..npages {
        let (p, _) = pager.alloc_page().await?;
        pages.push(p);
    }
    for (i, p) in pages.iter().enumerate() {
        let p = *p;
        // The page is fresh (dirty, unjournaled), but a long chain
        // can outgrow the page cache: `alloc_page` then spilled
        // its zeroed frame and `load` reads it back *clean*, so it
        // must be re-marked dirty or the chain write is lost on
        // the next eviction (v0.16 review fix: a record with more
        // than ~16 overflow pages wrote a truncated chain).
        let idx = pager.load(p).await?;
        pager.mark_frame_dirty(idx);
        let next = if i + 1 < pages.len() {
            pages.as_slice()[i + 1]
        } else {
            0
        };
        let start = i * per;
        let end = ((i + 1) * per).min(tail.len());
        let d = pager.frame_data_mut(idx);
        put_be_u32(d, 0, next);
        d[4..4 + (end - start)].copy_from_slice(&tail[start..end]);
    }
    Ok(pages.as_slice()[0])
}

/// Replace `page`'s image with `img` (caller ensures the page is
/// dirty/fresh and journaled as required).
async fn set_page_image<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
    img: &[u8],
) -> Result<(), S::Error> {
    let idx = pager.load(page).await?;
    pager.frame_data_mut(idx).copy_from_slice(img);
    Ok(())
}

/// Initialise a freshly allocated page as an empty table leaf —
/// a new table's root (its frame from `alloc_page` is zeroed, and
/// a zero page is not a valid leaf until the header is written).
pub async fn create_empty_root<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    // dirty_page, not load + mark_frame_dirty: the page must be
    // journaled and registered in the commit write set even
    // when it already exists (the rebuild path resets a live
    // interior root; mark_frame_dirty alone would silently drop
    // the new image at commit — found by v0.5 delete tests).
    let idx = pager.dirty_page(page).await?;
    let base = copy_bytes(pager.frame_data(idx));
    let empty: &[LeafCell] = &[];
    let img = build_leaf_image(page, page_size, empty, base.as_slice());
    pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
    Ok(())
}

/// Insert (rowid, payload) into the table rooted at `root`.
/// The root page number never changes (a root split rewrites the
/// root page as an interior over two fresh children).
pub async fn btree_insert<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
    payload: &[u8],
) -> Result<(), S::Error> {
    // A WITHOUT ROWID table (v0.16): the handle's row becomes
    // `payload`, stored in the clustered tree under its key.
    #[cfg(feature = "without_rowid")]
    if let Some(t) = pager.wr.table_of(root) {
        return crate::wr::put(pager, t, rowid, payload).await;
    }
    #[cfg(feature = "utf16")]
    let conv = stored_record::<S::Error>(pager.text_enc(), payload)?;
    #[cfg(feature = "utf16")]
    let payload = conv.as_ref().map_or(payload, |c| c.as_slice());
    let usable = pager.usable() as usize;
    let page_size = pager.page_size() as usize;
    // ---- descent, remembering (interior page, child index) ----
    let mut path: AVec<(u32, usize)> = AVec::new();
    let mut is_rightmost = true;
    let mut page = root;
    let (mut leaf_page, leaf_cells, insert_pos) = loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_TABLE {
                return Err(Error::Unsupported);
            }
            let cells = leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            let mut pos = cells.len();
            for (i, c) in cells.iter().enumerate() {
                if c.rowid == rowid {
                    return Err(Error::Schema {
                        kind: SchemaKind::DuplicateRowid,
                    });
                }
                if c.rowid > rowid {
                    pos = i;
                    break;
                }
            }
            break (page, cells, pos);
        }
        let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
        let mut child = rightmost;
        let mut ci = pairs.len();
        for (i, (c, k)) in pairs.iter().enumerate() {
            if rowid <= *k {
                child = *c;
                ci = i;
                break;
            }
        }
        if ci != pairs.len() {
            is_rightmost = false;
        }
        path.push((page, ci));
        page = child;
    };

    // ---- build the new cell (overflow first: page numbers of the
    // descent do not move when new pages are allocated) ----
    let local_len = table_leaf_local(usable, payload.len());
    let overflow_head = write_overflow(pager, payload, local_len).await?;
    let mut cell_bytes = AVec::new();
    encode_varint(payload.len() as u64, &mut cell_bytes);
    encode_varint(rowid as u64, &mut cell_bytes);
    for b in &payload[..local_len] {
        cell_bytes.push(*b);
    }
    if overflow_head != 0 {
        for b in overflow_head.to_be_bytes() {
            cell_bytes.push(b);
        }
    }
    let new_cell = LeafCell {
        rowid,
        bytes: cell_bytes,
        payload_len: payload.len(),
        local_len,
        overflow: overflow_head,
    };

    // ---- merged cell list ----
    let mut all: AVec<LeafCell> = AVec::new();
    for (i, c) in leaf_cells.iter().enumerate() {
        if i == insert_pos {
            all.push(new_cell.clone());
        }
        all.push(c.clone());
    }
    if insert_pos >= leaf_cells.len() {
        all.push(new_cell);
    }

    let mut hdr_off = if leaf_page == 1 { 100 } else { 0 };
    if hdr_off != 0 && leaf_need(all.as_slice(), hdr_off) > usable {
        // Page 1 as the root leaf overflows (v0.16 fix): as the
        // pin's `balance_deeper`, page 1 becomes an interior page
        // with no cells over one fresh child, which then takes the
        // cells — or is balanced as any leaf under page 1 is (never
        // by the quick split below). The quick split used to take
        // the last cell of an empty page 1 (a subtract-with-overflow
        // panic on a first CREATE TABLE whose schema text is about
        // 3950 bytes).
        let (cp, _) = pager.alloc_page().await?;
        let idx = pager.dirty_page(1).await?;
        let base = copy_bytes(pager.frame_data(idx));
        let img = build_interior_image(1, page_size, &[], cp, base.as_slice());
        pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
        path.push((1, 0));
        leaf_page = cp;
        hdr_off = 0;
    }
    if leaf_need(all.as_slice(), hdr_off) <= usable {
        let idx = pager.dirty_page(leaf_page).await?;
        let base = copy_bytes(pager.frame_data(idx));
        let img = build_leaf_image(leaf_page, page_size, all.as_slice(), base.as_slice());
        set_page_image(pager, leaf_page, img.as_slice()).await?;
        return Ok(());
    }

    // ---- the leaf overflows: balance (v0.15) ----
    if is_rightmost && insert_pos == leaf_cells.len() && (root != 1 || path.len() > 1) {
        // Append into the tree's rightmost leaf: the pin's quick
        // split — the old leaf is left untouched and the new cell
        // goes alone onto a fresh page (probed, see the v0.15
        // section comment below).
        let new_cell = all.as_slice()[insert_pos].clone();
        if path.is_empty() {
            let (lp, _) = pager.alloc_page().await?;
            write_leaf_page(pager, lp, leaf_cells.as_slice(), true).await?;
            let (rp, _) = pager.alloc_page().await?;
            write_leaf_page(pager, rp, &[new_cell], true).await?;
            let divider = leaf_cells.as_slice()[leaf_cells.len() - 1].rowid;
            let idx = pager.dirty_page(root).await?;
            let base = copy_bytes(pager.frame_data(idx));
            let pairs = [(lp, divider)];
            let img = build_interior_image(root, page_size, &pairs, rp, base.as_slice());
            set_page_image(pager, root, img.as_slice()).await?;
            return Ok(());
        }
        let (rp, _) = pager.alloc_page().await?;
        write_leaf_page(pager, rp, &[new_cell], true).await?;
        let divider = leaf_cells.as_slice()[leaf_cells.len() - 1].rowid;
        let up = splice_table_parent(pager, &path, leaf_page, rp, divider).await?;
        return cascade_table_up(pager, root, &path, up).await;
    }
    if path.is_empty() {
        // Root leaf: balance its cells over two fresh children;
        // the root page becomes an interior over them.
        let weights: AVec<usize> = {
            let mut w = AVec::new();
            for c in all.iter() {
                w.push(c.bytes.len() + 2);
            }
            w
        };
        let mut groups = distribute_insert(all.as_slice(), weights.as_slice(), 2, usable - 8, 1);
        if groups.iter().any(|g| leaf_need(g.as_slice(), 0) > usable) {
            groups = greedy_leaf_groups(all.as_slice(), usable);
        }
        // Usually two children; three when a cell of nearly a page
        // lands between two halves of a full leaf (the greedy
        // groups; the pin's balance_deeper then splits as well).
        let n = groups.len();
        if n < 2 {
            return Err(Error::Unsupported);
        }
        let mut pairs: AVec<(u32, i64)> = AVec::new();
        let mut right = 0u32;
        for (i, g) in groups.iter().enumerate() {
            let Some(last) = g.as_slice().last() else {
                return Err(Error::Unsupported);
            };
            let (pg, _) = pager.alloc_page().await?;
            write_leaf_page(pager, pg, g.as_slice(), true).await?;
            if i + 1 < n {
                pairs.push((pg, last.rowid));
            } else {
                right = pg;
            }
        }
        let idx = pager.dirty_page(root).await?;
        let base = copy_bytes(pager.frame_data(idx));
        let img = build_interior_image(root, page_size, pairs.as_slice(), right, base.as_slice());
        set_page_image(pager, root, img.as_slice()).await?;
        return Ok(());
    }
    let (parent, trigger) = *path.as_slice().last().expect("non-root leaf has a parent");
    let up = ins_balance_table_leaf_window(pager, parent, trigger, leaf_page, all).await?;
    cascade_table_up(pager, root, &path, up).await
}

// ----------------------------------------------------------------
// Insert-side balance (v0.15) — probed behaviourally against the
// pinned 3.54.1 oracle before implementation (clean-room:
// file-format docs + oracle I/O only; dataset of 1,826 insert
// balance events plus 129 quick splits, machine-collected with
// exact per-page cell costs):
// - An insert that overflows a leaf (or, by cascade, an interior
//   page) balances a window of up to three consecutive siblings
//   under the parent — the same window the delete path uses
//   (`window_range`). The window's combined cells are
//   redistributed over the smallest page count that holds them
//   at full capacity (greedy in-order packing; the count matched
//   the pin on every one of the 1,826 events).
// - Within-window distribution is `distribute` (the v0.14 rule)
//   when the page count does not grow. When the balance allocates
//   one new page (count = window + 1, window >= 2), the pin
//   instead gives the new (last) page a suffix of the combined
//   sequence whose cost is closest to half a page's content
//   capacity, and distributes the remainder over the old pages
//   with `distribute`. (Fitted: the plain quota rule scored 0%
//   exact on grow events; the suffix decomposition scores ~68%
//   event-exact and within one cell on the rest of the
//   boundaries; page counts are exact throughout because they
//   depend only on window totals.)
// - Appending to the tree's rightmost leaf takes a quick split:
//   the old leaf is left untouched and the new cell goes alone
//   onto a fresh page. (129/129 observed append splits; index
//   B-trees have no quick split — their appends balance like
//   every other insert.)
// - A balance whose parent model no longer fits cascades: the
//   parent is balanced among ITS siblings with its new model
//   carried as a preset, up to a root balance that rewrites the
//   root page as a one-divider interior over two fresh children.
// ----------------------------------------------------------------

/// Distribute `items` into `n` groups for an INSERT balance:
/// `distribute` itself, except when the balance grows the window
/// by one page (`n == win + 1`, `win >= 2`) — then the last group
/// is a suffix of the combined sequence costing closest to half
/// the content capacity `cap` (ties take the smaller suffix), and
/// the remaining items are distributed over the `win` old pages.
fn distribute_insert<T: Clone>(
    items: &[T],
    weights: &[usize],
    n: usize,
    cap: usize,
    win: usize,
) -> AVec<AVec<T>> {
    if n == win + 1 && win >= 2 && items.len() > n {
        let target = cap / 2;
        let max_cnt = items.len() - win;
        let mut best_cnt = 1usize;
        let mut best_key = (usize::MAX, usize::MAX);
        let mut s = 0usize;
        for (i, w) in weights.iter().rev().enumerate() {
            if i >= max_cnt {
                break;
            }
            s += w;
            let key = (s.abs_diff(target), s);
            if key < best_key {
                best_key = key;
                best_cnt = i + 1;
            }
        }
        let cut = items.len() - best_cnt;
        let mut groups = distribute(&items[..cut], &weights[..cut], win, cap);
        let mut suffix = AVec::new();
        for it in &items[cut..] {
            suffix.push(it.clone());
        }
        groups.push(suffix);
        return groups;
    }
    distribute(items, weights, n, cap)
}

/// Greedy in-order packing of leaf cells at full capacity (the
/// page count it produces is the balance's page count; it is
/// also the fallback when a quota group would not fit).
fn greedy_leaf_groups(all: &[LeafCell], usable: usize) -> AVec<AVec<LeafCell>> {
    let mut groups: AVec<AVec<LeafCell>> = AVec::new();
    let mut cur: AVec<LeafCell> = AVec::new();
    for c in all.iter() {
        let mut trial = cur.clone();
        trial.push(c.clone());
        if !cur.is_empty() && leaf_need(trial.as_slice(), 0) > usable {
            groups.push(cur);
            cur = AVec::new();
        }
        cur.push(c.clone());
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

/// Greedy in-order packing of interior items ((child, divider
/// key) pairs): a page holding items a..b stores dividers for
/// a..b-1, so adding an item costs the previous last item's
/// divider (4 + varint + pointer slot).
fn greedy_interior_groups(all: &[(u32, i64)], usable: usize) -> AVec<AVec<(u32, i64)>> {
    let mut groups: AVec<AVec<(u32, i64)>> = AVec::new();
    let mut cur: AVec<(u32, i64)> = AVec::new();
    let mut cur_need = 12usize;
    for item in all.iter() {
        if let Some(prev) = cur.as_slice().last() {
            let add = 4 + varint_len(prev.1) + 2;
            if cur_need + add > usable {
                groups.push(cur);
                cur = AVec::new();
                cur_need = 12;
            } else {
                cur_need += add;
            }
        }
        cur.push(*item);
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

/// Stored-page need of one interior group (dividers for all but
/// the last item, plus header and pointer slots).
fn interior_group_need(g: &[(u32, i64)]) -> usize {
    let mut need = 12usize;
    for t in &g[..g.len().saturating_sub(1)] {
        need += 4 + varint_len(t.1) + 2;
    }
    need
}

/// Outcome of one insert-side window balance (table tree).
enum InsUp {
    /// The balance completed; every ancestor page is written.
    Done,
    /// The balance completed at this level, but the parent page
    /// `page`'s new model (pairs, rightmost) no longer fits:
    /// the cascade balances `page` among its own siblings with
    /// this model as the preset.
    Overflow {
        page: u32,
        pairs: AVec<(u32, i64)>,
        rightmost: u32,
    },
}

/// Splice a freshly split child (`left` gained sibling `right`
/// under divider `divider`) into the parent at the top of
/// `path` (the quick split's parent step; the leaf itself is
/// already written). Returns the cascade outcome.
async fn splice_table_parent<S: Storage>(
    pager: &mut Pager<S>,
    path: &AVec<(u32, usize)>,
    left: u32,
    right: u32,
    divider: i64,
) -> Result<InsUp, S::Error> {
    let usable = pager.usable() as usize;
    let (parent, ci) = *path
        .as_slice()
        .last()
        .expect("non-root splice has a parent");
    let (pairs, _rightmost, children) = table_children(pager, parent).await?;
    debug_assert_eq!(children.as_slice()[ci], left, "descent path child mismatch");
    let mut new_children: AVec<u32> = AVec::new();
    for (i, c) in children.iter().enumerate() {
        new_children.push(*c);
        if i == ci {
            new_children.push(right);
        }
    }
    let mut new_pairs: AVec<(u32, i64)> = AVec::new();
    for i in 0..new_children.len() - 1 {
        let key = if i == ci {
            divider
        } else if i < ci {
            pairs.as_slice()[i].1
        } else {
            pairs.as_slice()[i - 1].1
        };
        new_pairs.push((new_children.as_slice()[i], key));
    }
    let new_rightmost = *new_children.as_slice().last().expect("nonempty children");
    let hdr_off = if parent == 1 { 100 } else { 0 };
    if interior_need(new_pairs.as_slice(), hdr_off) <= usable {
        write_interior_page(pager, parent, new_pairs.as_slice(), new_rightmost, false).await?;
        return Ok(InsUp::Done);
    }
    Ok(InsUp::Overflow {
        page: parent,
        pairs: new_pairs,
        rightmost: new_rightmost,
    })
}

/// Drive an insert cascade upward: while a balance reports its
/// parent model overflowing, balance that parent among its
/// siblings (preset = the model); an overflowing root is
/// finished by the root interior balance.
async fn cascade_table_up<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    path: &AVec<(u32, usize)>,
    first: InsUp,
) -> Result<(), S::Error> {
    let mut up = first;
    loop {
        let InsUp::Overflow {
            page,
            pairs,
            rightmost,
        } = up
        else {
            return Ok(());
        };
        if page == root {
            return ins_root_table_interior(pager, root, pairs, rightmost).await;
        }
        let pos = path
            .iter()
            .position(|(p, _)| *p == page)
            .ok_or(Error::Unsupported)?;
        if pos == 0 {
            return Err(Error::Unsupported);
        }
        let (gp, gtrigger) = path.as_slice()[pos - 1];
        up = ins_balance_table_interior_window(pager, gp, gtrigger, page, pairs, rightmost).await?;
    }
}

/// Rebuild an interior page's parent model after its children
/// `start..end` were replaced by `n_new` window pages holding
/// `groups` (each group's last item's key is its child's new
/// divider). Shared by the leaf and interior insert windows.
fn table_parent_model(
    pairs: &[(u32, i64)],
    children: &AVec<u32>,
    start: usize,
    end: usize,
    pages_out: &[u32],
    group_keys: &[i64],
) -> (AVec<(u32, i64)>, u32) {
    let n_new = pages_out.len();
    let mut new_children: AVec<u32> = AVec::new();
    for j in 0..start {
        new_children.push(children.as_slice()[j]);
    }
    for p in pages_out.iter() {
        new_children.push(*p);
    }
    for j in end..children.len() {
        new_children.push(children.as_slice()[j]);
    }
    let mut new_pairs: AVec<(u32, i64)> = AVec::new();
    for i in 0..new_children.len() - 1 {
        let key = if (start..start + n_new).contains(&i) {
            group_keys[i - start]
        } else {
            let old_idx = if i < start {
                i
            } else {
                i + (end - start) - n_new
            };
            pairs[old_idx].1
        };
        new_pairs.push((new_children.as_slice()[i], key));
    }
    let new_rightmost = *new_children.as_slice().last().expect("nonempty children");
    (new_pairs, new_rightmost)
}

/// Leaf-level insert balance: redistribute the window leaves'
/// combined cells (the trigger leaf's list is `preset_cells`,
/// already merged with the inserted cell) over the smallest
/// page count that holds them. Writes the window pages and the
/// parent when it still fits; otherwise reports the parent
/// model as an overflow for the cascade.
async fn ins_balance_table_leaf_window<S: Storage>(
    pager: &mut Pager<S>,
    parent: u32,
    trigger: usize,
    preset_page: u32,
    preset_cells: AVec<LeafCell>,
) -> Result<InsUp, S::Error> {
    let usable = pager.usable() as usize;
    let (pairs, _rightmost, children) = table_children(pager, parent).await?;
    let nchildren = children.len();
    if nchildren == 0 {
        return Err(Error::CorruptPage { page: parent });
    }
    let (start, end) = window_range(nchildren, trigger.min(nchildren - 1));
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children.as_slice()[j]);
        }
        w
    };
    // Gather the window's combined cells, in key order.
    let mut all: AVec<LeafCell> = AVec::new();
    for p in window_pages.iter() {
        let p = *p;
        let cells = if p == preset_page {
            preset_cells.clone()
        } else {
            let idx = pager.load(p).await?;
            let snap = copy_bytes(pager.frame_data(idx));
            let h = head::<S::Error>(snap.as_slice(), p)?;
            if h.kind != KIND_LEAF_TABLE {
                return Err(Error::CorruptPage { page: p });
            }
            leaf_cells::<S::Error>(snap.as_slice(), p, usable)?
        };
        for c in cells.iter() {
            all.push(c.clone());
        }
    }
    // Smallest page count at full capacity (greedy packing).
    let n_new = {
        let mut pages = 1usize;
        let mut cur_need = 8usize;
        for c in all.iter() {
            let add = c.bytes.len() + 2;
            if cur_need + add > usable && cur_need > 8 {
                pages += 1;
                cur_need = 8;
            }
            cur_need += add;
        }
        pages
    };
    let weights: AVec<usize> = {
        let mut w = AVec::new();
        for c in all.iter() {
            w.push(c.bytes.len() + 2);
        }
        w
    };
    let mut groups = distribute_insert(
        all.as_slice(),
        weights.as_slice(),
        n_new,
        usable - 8,
        window_pages.len(),
    );
    if groups.iter().any(|g| leaf_need(g.as_slice(), 0) > usable) {
        groups = greedy_leaf_groups(all.as_slice(), usable);
    }
    let n_new = groups.len();
    // Write the window pages (allocating extra ones at the end of
    // the window when the balance grows), free any excess.
    let mut pages_out: AVec<u32> = window_pages.clone();
    while pages_out.len() < n_new {
        let (np, _) = pager.alloc_page().await?;
        pages_out.push(np);
    }
    for (i, g) in groups.iter().enumerate() {
        let fresh = i >= window_pages.len();
        write_leaf_page(pager, pages_out.as_slice()[i], g.as_slice(), fresh).await?;
    }
    for i in n_new..window_pages.len() {
        pager.free_page(window_pages.as_slice()[i]).await?;
    }
    let group_keys: AVec<i64> = {
        let mut k = AVec::new();
        for g in groups.iter() {
            k.push(g.as_slice()[g.len() - 1].rowid);
        }
        k
    };
    let (new_pairs, new_rightmost) = table_parent_model(
        pairs.as_slice(),
        &children,
        start,
        end,
        &pages_out.as_slice()[..n_new],
        group_keys.as_slice(),
    );
    let hdr_off = if parent == 1 { 100 } else { 0 };
    if interior_need(new_pairs.as_slice(), hdr_off) <= usable {
        write_interior_page(pager, parent, new_pairs.as_slice(), new_rightmost, false).await?;
        return Ok(InsUp::Done);
    }
    Ok(InsUp::Overflow {
        page: parent,
        pairs: new_pairs,
        rightmost: new_rightmost,
    })
}

/// Interior-level insert balance: redistribute the window
/// interior pages' combined grandchildren over the smallest
/// page count that holds them. The trigger page's model comes
/// from the preset (its on-disk image is the pre-balance model).
#[allow(clippy::too_many_arguments)]
async fn ins_balance_table_interior_window<S: Storage>(
    pager: &mut Pager<S>,
    parent: u32,
    trigger: usize,
    preset_page: u32,
    preset_pairs: AVec<(u32, i64)>,
    preset_rightmost: u32,
) -> Result<InsUp, S::Error> {
    let usable = pager.usable() as usize;
    let (pairs, _rightmost, children) = table_children(pager, parent).await?;
    let nchildren = children.len();
    if nchildren == 0 {
        return Err(Error::CorruptPage { page: parent });
    }
    let (start, end) = window_range(nchildren, trigger.min(nchildren - 1));
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children.as_slice()[j]);
        }
        w
    };
    // Gather the window's combined grandchildren, each with its
    // divider key (its subtree's maximum rowid).
    let mut all: AVec<(u32, i64)> = AVec::new();
    for (wi, p) in window_pages.iter().enumerate() {
        let p = *p;
        let (wpairs, wchildren) = if p == preset_page {
            let mut ch = AVec::new();
            for (c, _) in preset_pairs.iter() {
                ch.push(*c);
            }
            ch.push(preset_rightmost);
            (preset_pairs.clone(), ch)
        } else {
            let (wp, _wr, wc) = table_children(pager, p).await?;
            let h_kind = {
                let idx = pager.load(p).await?;
                head::<S::Error>(pager.frame_data(idx), p)?.kind
            };
            if h_kind != KIND_INTERIOR_TABLE {
                return Err(Error::CorruptPage { page: p });
            }
            (wp, wc)
        };
        for (j, gc) in wchildren.iter().enumerate() {
            let gc = *gc;
            let key = if j < wpairs.len() {
                wpairs.as_slice()[j].1
            } else if start + wi < pairs.len() {
                // This window page's rightmost child's maximum is
                // the divider the parent stores for the page.
                pairs.as_slice()[start + wi].1
            } else {
                match seek_max_rowid(pager, p).await? {
                    Some(m) => m,
                    None => return Err(Error::CorruptPage { page: p }),
                }
            };
            all.push((gc, key));
        }
    }
    let weights: AVec<usize> = {
        let mut w = AVec::new();
        for (_, key) in all.iter() {
            w.push(4 + varint_len(*key) + 2);
        }
        w
    };
    let n_new = greedy_interior_groups(all.as_slice(), usable).len();
    let mut groups = distribute_insert(
        all.as_slice(),
        weights.as_slice(),
        n_new,
        usable - 12,
        window_pages.len(),
    );
    if groups
        .iter()
        .any(|g| interior_group_need(g.as_slice()) > usable)
    {
        groups = greedy_interior_groups(all.as_slice(), usable);
    }
    let n_new = groups.len();
    let mut pages_out: AVec<u32> = window_pages.clone();
    while pages_out.len() < n_new {
        let (np, _) = pager.alloc_page().await?;
        pages_out.push(np);
    }
    for (i, g) in groups.iter().enumerate() {
        let gpairs: AVec<(u32, i64)> = {
            let mut v = AVec::new();
            for t in 0..g.len() - 1 {
                v.push(g.as_slice()[t]);
            }
            v
        };
        let gright = g.as_slice()[g.len() - 1].0;
        let fresh = i >= window_pages.len();
        write_interior_page(
            pager,
            pages_out.as_slice()[i],
            gpairs.as_slice(),
            gright,
            fresh,
        )
        .await?;
    }
    for i in n_new..window_pages.len() {
        pager.free_page(window_pages.as_slice()[i]).await?;
    }
    let group_keys: AVec<i64> = {
        let mut k = AVec::new();
        for g in groups.iter() {
            k.push(g.as_slice()[g.len() - 1].1);
        }
        k
    };
    let (new_pairs, new_rightmost) = table_parent_model(
        pairs.as_slice(),
        &children,
        start,
        end,
        &pages_out.as_slice()[..n_new],
        group_keys.as_slice(),
    );
    let hdr_off = if parent == 1 { 100 } else { 0 };
    if interior_need(new_pairs.as_slice(), hdr_off) <= usable {
        write_interior_page(pager, parent, new_pairs.as_slice(), new_rightmost, false).await?;
        return Ok(InsUp::Done);
    }
    Ok(InsUp::Overflow {
        page: parent,
        pairs: new_pairs,
        rightmost: new_rightmost,
    })
}

/// Finish an insert cascade that overflowed the root's model:
/// redistribute the root's grandchildren over two fresh
/// interior pages and rewrite the root as a one-divider
/// interior over them (the root page number never changes).
async fn ins_root_table_interior<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    pairs: AVec<(u32, i64)>,
    rightmost: u32,
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    let page_size = pager.page_size() as usize;
    let mut all: AVec<(u32, i64)> = AVec::new();
    for t in pairs.iter() {
        all.push(*t);
    }
    let max = match seek_max_rowid(pager, rightmost).await? {
        Some(m) => m,
        None => return Err(Error::CorruptPage { page: root }),
    };
    all.push((rightmost, max));
    let weights: AVec<usize> = {
        let mut w = AVec::new();
        for (_, key) in all.iter() {
            w.push(4 + varint_len(*key) + 2);
        }
        w
    };
    let mut groups = distribute_insert(all.as_slice(), weights.as_slice(), 2, usable - 12, 1);
    if groups
        .iter()
        .any(|g| interior_group_need(g.as_slice()) > usable)
    {
        groups = greedy_interior_groups(all.as_slice(), usable);
    }
    if groups.len() != 2 {
        return Err(Error::Unsupported);
    }
    let up_key = groups.as_slice()[0].as_slice()[groups.as_slice()[0].len() - 1].1;
    let (lp, _) = pager.alloc_page().await?;
    let (rp, _) = pager.alloc_page().await?;
    for (i, pg) in [lp, rp].iter().enumerate() {
        let g = &groups.as_slice()[i];
        let gpairs: AVec<(u32, i64)> = {
            let mut v = AVec::new();
            for t in 0..g.len() - 1 {
                v.push(g.as_slice()[t]);
            }
            v
        };
        let gright = g.as_slice()[g.len() - 1].0;
        write_interior_page(pager, *pg, gpairs.as_slice(), gright, true).await?;
    }
    let idx = pager.dirty_page(root).await?;
    let base = copy_bytes(pager.frame_data(idx));
    let one = [(lp, up_key)];
    let img = build_interior_image(root, page_size, &one, rp, base.as_slice());
    set_page_image(pager, root, img.as_slice()).await?;
    Ok(())
}

/// Write `cells` as the complete leaf image of `page`.
/// Journalling: an existing page (`fresh == false`) is journaled
/// via `dirty_page` before any byte changes; a page fresh from
/// `alloc_page` is already dirty and must NOT be journaled (its
/// on-disk "original" does not exist; rollback truncates it).
async fn write_leaf_page<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
    cells: &[LeafCell],
    fresh: bool,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    let idx = if fresh {
        let idx = pager.load(page).await?;
        pager.mark_frame_dirty(idx);
        idx
    } else {
        pager.dirty_page(page).await?
    };
    let base = copy_bytes(pager.frame_data(idx));
    let img = build_leaf_image(page, page_size, cells, base.as_slice());
    pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
    Ok(())
}

async fn write_interior_page<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
    pairs: &[(u32, i64)],
    rightmost: u32,
    fresh: bool,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    let idx = if fresh {
        let idx = pager.load(page).await?;
        pager.mark_frame_dirty(idx);
        idx
    } else {
        pager.dirty_page(page).await?
    };
    let base = copy_bytes(pager.frame_data(idx));
    let img = build_interior_image(page, page_size, pairs, rightmost, base.as_slice());
    pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
    Ok(())
}

// ----------------------------------------------------------------
// Table B-tree row deletion (v0.3 for sqlite_schema rows; v0.5
// general DELETE/UPDATE/DROP TABLE use) and, since v0.14,
// balance-by-redistribution and merge on the delete path.
//
// One format invariant shapes the design (found by differential
// testing against the oracle in v0.5): a non-root B-tree page may
// never be empty — SQLite rebalances to maintain that, and the
// oracle's integrity_check rejects trees that violate it.
//
// v0.14 balance, probed behaviourally against the pinned 3.54.1
// oracle before implementation (clean-room: file-format docs +
// oracle I/O only):
// - A delete that leaves a non-root page with total free space
//   greater than two thirds of the usable page size triggers a
//   balance of that page with its siblings. (Probed crossings:
//   usable 512 -> free 342 triggers, 339 does not; 1024 -> 686
//   triggers, 680 does not; 4096 -> the crossing sits between
//   2645 and 2756. All fit `free > usable * 2 / 3`.) An insert
//   into an underfull page does NOT balance (also probed).
// - The balance window is up to three consecutive sibling pages
//   under one parent, containing the underfull page, centred
//   where possible and clamped at the parent's edges (probed:
//   first/middle/last leaf windows are [0..3], [i-1..i+2] and
//   [n-3..n] respectively).
// - The window's combined cells are redistributed over the
//   smallest number of pages that hold them at full capacity;
//   when that is fewer than the window size, the excess pages
//   return to the freelist (merge). When an interior root is
//   left with a single child, the child is copied into the root
//   page and freed (the tree's height shrinks).
// - The within-window distribution follows cumulative quotas
//   fitted on a machine-collected dataset of 156 oracle balance
//   events (exact pre-balance per-page cell costs): with S the
//   window's total cost (cell bytes + 2 per cell), P one page's
//   content capacity, and n the result page count, page i's
//   quota boundary is Q_i = i * (S + P) / (n + 1). A two-page
//   merge splits evenly (17/17 events exact); for three-page
//   results the first boundary is check-then-add at Q_1 and the
//   second sits at the item closest to Q_2 (115/139 events
//   exact, the rest within one cell). Page-count and freelist
//   traces match the pin step-for-step on every pinned
//   workload; the residual +/-1-cell distribution misses are
//   recorded in STATUS.md. The distribution lives in
//   `distribute`, shared by the table and index windows.
// - The degenerate case the v0.5 design met — a delete that
//   would empty a whole non-root subtree — still falls back to
//   rebuilding the tree from its surviving rows (root page
//   number, rowids and row contents all preserved, so indexes
//   stay valid). Overflow pages of a removed row return to the
//   freelist.
// ----------------------------------------------------------------

/// Length of the varint encoding of `v` (the divider-key cost of
/// a table interior cell).
fn varint_len(v: i64) -> usize {
    let mut t = AVec::new();
    encode_varint(v as u64, &mut t);
    t.len()
}

/// The pinned engine's underflow rule (v0.14, probed above): a
/// page whose total free space exceeds two thirds of the usable
/// size is balanced with its siblings after a delete.
fn page_underfull(free: usize, usable: usize) -> bool {
    free > usable * 2 / 3
}

fn table_leaf_free(usable: usize, page: u32, cells: &[LeafCell]) -> usize {
    let hdr_off = if page == 1 { 100 } else { 0 };
    usable.saturating_sub(leaf_need(cells, hdr_off))
}

fn table_interior_free(usable: usize, page: u32, pairs: &[(u32, i64)]) -> usize {
    let hdr_off = if page == 1 { 100 } else { 0 };
    usable.saturating_sub(interior_need(pairs, hdr_off))
}

/// The balance window within a parent's `nchildren` children for
/// a triggering child at `trigger`: up to three consecutive
/// children, centred on the trigger where possible.
fn window_range(nchildren: usize, trigger: usize) -> (usize, usize) {
    if nchildren <= 3 {
        return (0, nchildren);
    }
    let start = if trigger == 0 {
        0
    } else if trigger + 1 >= nchildren {
        nchildren - 3
    } else {
        trigger - 1
    };
    (start, start + 3)
}

/// Distribute `items` (with per-item `weights`) in order into
/// `n` non-empty groups, evenly by weight: each group is filled
/// to at least the ceiling of its fair share of the remaining
/// weight before the next group starts. Requires
/// `items.len() >= n >= 1`.
fn distribute<T: Clone>(items: &[T], weights: &[usize], n: usize, usable: usize) -> AVec<AVec<T>> {
    debug_assert!(n >= 1 && items.len() >= n);
    let total: usize = weights.iter().sum();
    let mut groups: AVec<AVec<T>> = AVec::new();
    let mut idx = 0usize;
    let mut cum = 0usize;
    for g in 0..n {
        let mut group = AVec::new();
        if g + 1 == n {
            while idx < items.len() {
                group.push(items[idx].clone());
                idx += 1;
            }
            groups.push(group);
            break;
        }
        // Items that must remain so every later group is
        // non-empty.
        let reserve = n - 1 - g;
        if n == 2 {
            // Two-page result: split evenly by weight — fill
            // while the running total is below half (probed:
            // exact on every observed merge).
            while items.len() - idx > reserve && (group.is_empty() || cum * 2 < total) {
                cum += weights[idx];
                group.push(items[idx].clone());
                idx += 1;
            }
        } else if g == 0 {
            // First boundary of a three-page result: cumulative
            // quota (total + usable) / (n + 1), check-then-add —
            // the group overshoots its quota by less than one
            // item (probed from the 3.54.1 oracle; see the
            // section comment).
            while items.len() - idx > reserve
                && (group.is_empty() || cum * (n + 1) < total + usable)
            {
                cum += weights[idx];
                group.push(items[idx].clone());
                idx += 1;
            }
        } else {
            // Second boundary: place it at the item closest to
            // the cumulative quota 2 * (total + usable) / (n + 1)
            // — stop before an item that would carry the total
            // strictly further past the quota than it currently
            // falls short.
            while items.len() - idx > reserve
                && (group.is_empty()
                    || (2 * cum + weights[idx]) * (n + 1) <= 2 * (total + usable) * (g + 1))
            {
                cum += weights[idx];
                group.push(items[idx].clone());
                idx += 1;
            }
        }
        groups.push(group);
    }
    groups
}

/// Outcome of one window balance.
enum TableBalance {
    /// The window was redistributed (and possibly merged);
    /// `children_removed` counts how many children the parent
    /// lost (0 for a pure redistribution).
    Done { children_removed: usize },
    /// A degenerate shape was detected during the gather phase,
    /// before any page was written: the caller falls back to
    /// rebuilding the whole tree from its surviving rows.
    Rebuild,
}

/// Load a table interior page's model: (divider pairs, rightmost
/// child, full children list in order).
async fn table_children<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
) -> Result<(AVec<(u32, i64)>, u32, AVec<u32>), S::Error> {
    let idx = pager.load(page).await?;
    let snap = copy_bytes(pager.frame_data(idx));
    let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
    let mut children = AVec::new();
    for (c, _) in pairs.iter() {
        children.push(*c);
    }
    children.push(rightmost);
    Ok((pairs, rightmost, children))
}

/// Balance the children of table interior page `parent` around
/// child index `trigger` (the child a delete just left
/// underfull). The children may be leaf pages (leaf level) or
/// interior pages (a cascade level); the first window child's
/// kind decides. `preset` carries the trigger leaf's post-delete
/// cell list at the leaf level, where the on-disk page still
/// holds the deleted row.
async fn balance_table_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    trigger: usize,
    preset: Option<(u32, AVec<LeafCell>)>,
) -> Result<TableBalance, S::Error> {
    let (pairs, _rightmost, children) = table_children(pager, parent).await?;
    let nchildren = children.len();
    if nchildren == 0 {
        return Err(Error::CorruptPage { page: parent });
    }
    let (start, end) = window_range(nchildren, trigger.min(nchildren - 1));
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children.as_slice()[j]);
        }
        w
    };
    // Decide leaf vs interior level from the first window child.
    let first = window_pages.as_slice()[0];
    let first_kind = {
        let idx = pager.load(first).await?;
        head::<S::Error>(pager.frame_data(idx), first)?.kind
    };
    match first_kind {
        KIND_LEAF_TABLE => {
            balance_table_leaf_window(
                pager,
                root,
                parent,
                pairs.as_slice(),
                &children,
                start,
                end,
                &window_pages,
                preset,
            )
            .await
        }
        KIND_INTERIOR_TABLE => {
            balance_table_interior_window(
                pager,
                root,
                parent,
                pairs.as_slice(),
                &children,
                start,
                end,
                &window_pages,
            )
            .await
        }
        _ => Err(Error::CorruptPage { page: first }),
    }
}

/// Leaf-level window balance: redistribute the window leaves'
/// combined cells over the smallest page count that holds them.
#[allow(clippy::too_many_arguments)]
async fn balance_table_leaf_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    pairs: &[(u32, i64)],
    children: &AVec<u32>,
    start: usize,
    end: usize,
    window_pages: &AVec<u32>,
    preset: Option<(u32, AVec<LeafCell>)>,
) -> Result<TableBalance, S::Error> {
    let usable = pager.usable() as usize;
    // Gather the window's combined cells, in key order.
    let mut all: AVec<LeafCell> = AVec::new();
    for p in window_pages.iter() {
        let p = *p;
        let cells = match &preset {
            Some((pp, kept)) if *pp == p => kept.clone(),
            _ => {
                let idx = pager.load(p).await?;
                let snap = copy_bytes(pager.frame_data(idx));
                let h = head::<S::Error>(snap.as_slice(), p)?;
                if h.kind != KIND_LEAF_TABLE {
                    return Err(Error::CorruptPage { page: p });
                }
                leaf_cells::<S::Error>(snap.as_slice(), p, usable)?
            }
        };
        for c in cells.iter() {
            all.push(c.clone());
        }
    }
    if all.is_empty() && end - start == children.len() {
        if parent == root {
            // The whole tree's rows are gone: the root
            // becomes an empty leaf, the children are freed.
            for p in window_pages.iter() {
                pager.free_page(*p).await?;
            }
            create_empty_root(pager, root).await?;
            return Ok(TableBalance::Done {
                children_removed: children.len(),
            });
        }
        return Ok(TableBalance::Rebuild);
    }
    // A fully empty window inside a larger parent falls
    // through: the window pages are freed below via the
    // n_new = 0 path.
    // Smallest page count that holds the combined cells at full
    // capacity (greedy in-order packing; child pages have no
    // 100-byte header offset).
    let n_new = if all.is_empty() {
        0
    } else {
        let mut pages = 1usize;
        let mut cur_need = 8usize;
        for c in all.iter() {
            let add = c.bytes.len() + 2;
            if cur_need + add > usable && cur_need > 8 {
                pages += 1;
                cur_need = 8;
            }
            cur_need += add;
        }
        pages
    };
    let groups: AVec<AVec<LeafCell>> = if n_new == 0 {
        AVec::new()
    } else {
        let weights: AVec<usize> = {
            let mut w = AVec::new();
            for c in all.iter() {
                w.push(c.bytes.len() + 2);
            }
            w
        };
        let groups = distribute(all.as_slice(), weights.as_slice(), n_new, usable - 8);
        // The quota distribution is a layout target, not a
        // capacity proof: with chunky cells a group could
        // exceed its page. Fall back to the greedy packing
        // (whose page count is `n_new` by construction) if any
        // group does not fit.
        if groups.iter().any(|g| leaf_need(g.as_slice(), 0) > usable) {
            let mut greedy: AVec<AVec<LeafCell>> = AVec::new();
            let mut cur: AVec<LeafCell> = AVec::new();
            for c in all.iter() {
                let mut trial = cur.clone();
                trial.push(c.clone());
                if !cur.is_empty() && leaf_need(trial.as_slice(), 0) > usable {
                    greedy.push(cur);
                    cur = AVec::new();
                }
                cur.push(c.clone());
            }
            if !cur.is_empty() {
                greedy.push(cur);
            }
            greedy
        } else {
            groups
        }
    };
    let n_new = groups.len();
    // Write the surviving pages, then free the excess.
    for (i, g) in groups.iter().enumerate() {
        write_leaf_page(pager, window_pages.as_slice()[i], g.as_slice(), false).await?;
    }
    for i in n_new..window_pages.len() {
        pager.free_page(window_pages.as_slice()[i]).await?;
    }
    // Rebuild the parent's pairs over the spliced child list.
    let mut new_children: AVec<u32> = AVec::new();
    for j in 0..start {
        new_children.push(children.as_slice()[j]);
    }
    for i in 0..n_new {
        new_children.push(window_pages.as_slice()[i]);
    }
    for j in end..children.len() {
        new_children.push(children.as_slice()[j]);
    }
    if new_children.is_empty() {
        // Only reachable for a non-root parent whose whole
        // subtree emptied — detected here, still before the
        // parent page itself was written, but the window pages
        // have already been freed; the caller's rebuild collects
        // surviving rows from what remains reachable, which is
        // exactly the rows outside this subtree... plus this
        // parent must not be left pointing at freed pages. That
        // shape is prevented by the gather-phase check: an empty
        // window covering every child of a non-root parent
        // returns Rebuild before any write (above).
        return Err(Error::CorruptPage { page: parent });
    }
    let mut new_pairs: AVec<(u32, i64)> = AVec::new();
    for i in 0..new_children.len() - 1 {
        let child = new_children.as_slice()[i];
        let key = if (start..start + n_new).contains(&i) {
            let g = &groups.as_slice()[i - start];
            g.as_slice()[g.len() - 1].rowid
        } else {
            // The child's old divider key, located by its old
            // position in the parent's pair list.
            let old_idx = if i < start {
                i
            } else {
                i + (end - start) - n_new
            };
            pairs[old_idx].1
        };
        new_pairs.push((child, key));
    }
    let new_rightmost = *new_children.as_slice().last().expect("nonempty");
    write_interior_page(pager, parent, new_pairs.as_slice(), new_rightmost, false).await?;
    Ok(TableBalance::Done {
        children_removed: (end - start) - n_new,
    })
}

/// Interior-level window balance: redistribute the window
/// interior pages' combined children over the smallest page
/// count that holds them. Divider keys are recomputed from each
/// child's subtree maximum (a table divider is exactly that).
#[allow(clippy::too_many_arguments)]
async fn balance_table_interior_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    pairs: &[(u32, i64)],
    children: &AVec<u32>,
    start: usize,
    end: usize,
    window_pages: &AVec<u32>,
) -> Result<TableBalance, S::Error> {
    let usable = pager.usable() as usize;
    // Gather the window's combined grandchildren, each with its
    // divider key (its subtree's maximum rowid).
    let mut all: AVec<(u32, i64)> = AVec::new();
    for (wi, p) in window_pages.iter().enumerate() {
        let p = *p;
        let (wpairs, wright, wchildren) = table_children(pager, p).await?;
        let h_kind = {
            let idx = pager.load(p).await?;
            head::<S::Error>(pager.frame_data(idx), p)?.kind
        };
        if h_kind != KIND_INTERIOR_TABLE {
            return Err(Error::CorruptPage { page: p });
        }
        for (j, gc) in wchildren.iter().enumerate() {
            let gc = *gc;
            let key = if j < wpairs.len() {
                wpairs.as_slice()[j].1
            } else if start + wi < pairs.len() {
                // This window page's rightmost child's maximum is
                // the divider the parent stores for the page.
                pairs[start + wi].1
            } else {
                // The parent's rightmost subtree: probe it.
                match seek_max_rowid(pager, p).await? {
                    Some(m) => m,
                    None => return Err(Error::CorruptPage { page: p }),
                }
            };
            let _ = wright;
            all.push((gc, key));
        }
    }
    if all.is_empty() && end - start == children.len() {
        if parent == root {
            for p in window_pages.iter() {
                pager.free_page(*p).await?;
            }
            create_empty_root(pager, root).await?;
            return Ok(TableBalance::Done {
                children_removed: children.len(),
            });
        }
        return Ok(TableBalance::Rebuild);
    }
    // Smallest page count at full capacity. A page holding
    // grandchildren a..b stores dividers for a..b-1, so adding
    // one more grandchild costs the previous last grandchild's
    // divider (4 + varint) plus a pointer slot.
    let n_new = if all.is_empty() {
        0
    } else {
        let mut pages = 1usize;
        let mut cur_need = 12usize;
        let mut cur_last: Option<i64> = None;
        for (_, key) in all.iter() {
            if let Some(prev) = cur_last {
                let add = 4 + varint_len(prev) + 2;
                if cur_need + add > usable {
                    pages += 1;
                    cur_need = 12;
                } else {
                    cur_need += add;
                }
            }
            cur_last = Some(*key);
        }
        pages
    };
    let mut groups: AVec<AVec<(u32, i64)>> = if n_new == 0 {
        AVec::new()
    } else {
        let weights: AVec<usize> = {
            let mut w = AVec::new();
            for (_, key) in all.iter() {
                w.push(4 + varint_len(*key) + 2);
            }
            w
        };
        distribute(all.as_slice(), weights.as_slice(), n_new, usable - 12)
    };
    // Repair pass: even-by-weight grouping can overflow a page
    // whose last grandchild's divider was counted in its weight
    // but is not stored on it (or vice versa); shift boundary
    // grandchildren forward until every page fits. If the last
    // page still overflows, fall back to a rebuild — the gather
    // phase has not written anything yet.
    for gi in 0..groups.len() {
        loop {
            let g = &groups.as_slice()[gi];
            if g.len() <= 1 {
                break;
            }
            let gpairs: AVec<(u32, i64)> = {
                let mut v = AVec::new();
                for t in 0..g.len() - 1 {
                    v.push(g.as_slice()[t]);
                }
                v
            };
            if interior_need(gpairs.as_slice(), 0) <= usable {
                break;
            }
            if gi + 1 >= groups.len() {
                return Ok(TableBalance::Rebuild);
            }
            let moved = groups.as_mut_slice()[gi].pop().expect("len>1");
            let next = &mut groups.as_mut_slice()[gi + 1];
            let mut rebuilt: AVec<(u32, i64)> = AVec::new();
            rebuilt.push(moved);
            for t in next.iter() {
                rebuilt.push(*t);
            }
            *next = rebuilt;
        }
    }
    // Write the surviving interior pages, free the excess.
    for (i, g) in groups.iter().enumerate() {
        let gpairs: AVec<(u32, i64)> = {
            let mut v = AVec::new();
            for t in 0..g.len() - 1 {
                v.push(g.as_slice()[t]);
            }
            v
        };
        let gright = g.as_slice()[g.len() - 1].0;
        write_interior_page(
            pager,
            window_pages.as_slice()[i],
            gpairs.as_slice(),
            gright,
            false,
        )
        .await?;
    }
    for i in n_new..window_pages.len() {
        pager.free_page(window_pages.as_slice()[i]).await?;
    }
    // Rebuild the parent's pairs over the spliced child list.
    let mut new_children: AVec<u32> = AVec::new();
    for j in 0..start {
        new_children.push(children.as_slice()[j]);
    }
    for i in 0..n_new {
        new_children.push(window_pages.as_slice()[i]);
    }
    for j in end..children.len() {
        new_children.push(children.as_slice()[j]);
    }
    if new_children.is_empty() {
        return Err(Error::CorruptPage { page: parent });
    }
    let mut new_pairs: AVec<(u32, i64)> = AVec::new();
    for i in 0..new_children.len() - 1 {
        let child = new_children.as_slice()[i];
        let key = if (start..start + n_new).contains(&i) {
            let g = &groups.as_slice()[i - start];
            g.as_slice()[g.len() - 1].1
        } else {
            let old_idx = if i < start {
                i
            } else {
                i + (end - start) - n_new
            };
            pairs[old_idx].1
        };
        new_pairs.push((child, key));
    }
    let new_rightmost = *new_children.as_slice().last().expect("nonempty");
    write_interior_page(pager, parent, new_pairs.as_slice(), new_rightmost, false).await?;
    Ok(TableBalance::Done {
        children_removed: (end - start) - n_new,
    })
}

/// Collapse a table root that has been left with a single
/// child: copy the child's image into the root page (whose
/// number — and, for page 1, header — never changes) and free
/// the child. Loops while the new root is still a one-child
/// interior page.
async fn collapse_table_root<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    loop {
        let idx = pager.load(root).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), root)?;
        if h.kind != KIND_INTERIOR_TABLE {
            return Ok(());
        }
        let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), root)?;
        if !pairs.is_empty() {
            return Ok(());
        }
        let child = rightmost;
        if child == 0 || child == root {
            return Err(Error::CorruptPage { page: root });
        }
        let cidx = pager.load(child).await?;
        let csnap = copy_bytes(pager.frame_data(cidx));
        let ch = head::<S::Error>(csnap.as_slice(), child)?;
        // Page 1 holds 100 bytes fewer than its child: a child that
        // does not fit stays, under a root with no cell (the pin's
        // `balance_nonroot` shrinks only when `hdrOffset <= nFree`;
        // v0.16 fix, ungated: the cells were packed over the
        // pointer array, corrupting `sqlite_schema` — a DROP TABLE
        // or ALTER of a 512-byte-page file with a two-leaf schema).
        let hdr_off = if root == 1 { 100 } else { 0 };
        match ch.kind {
            KIND_LEAF_TABLE => {
                let cells = leaf_cells::<S::Error>(csnap.as_slice(), child, usable)?;
                if leaf_need(cells.as_slice(), hdr_off) > usable {
                    return Ok(());
                }
                write_leaf_page(pager, root, cells.as_slice(), false).await?;
            }
            KIND_INTERIOR_TABLE => {
                let (cpairs, cright) = interior_cells::<S::Error>(csnap.as_slice(), child)?;
                if interior_need(cpairs.as_slice(), hdr_off) > usable {
                    return Ok(());
                }
                write_interior_page(pager, root, cpairs.as_slice(), cright, false).await?;
            }
            _ => return Err(Error::CorruptPage { page: child }),
        }
        pager.free_page(child).await?;
    }
}

/// Free a removed table leaf cell's overflow chain, if any.
async fn free_table_overflow<S: Storage>(
    pager: &mut Pager<S>,
    removed: &LeafCell,
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    if removed.overflow == 0 {
        return Ok(());
    }
    let per = usable - 4;
    let mut next = removed.overflow;
    let mut remaining = removed.payload_len - removed.local_len;
    let mut hops = 0u32;
    while next != 0 {
        hops += 1;
        if hops > pager.page_count() + 1 {
            return Err(Error::OverflowChainCycle);
        }
        let idx = pager.load(next).await?;
        let following = be_u32(pager.frame_data(idx), 0);
        pager.free_page(next).await?;
        next = following;
        if remaining <= per {
            break;
        }
        remaining -= per;
    }
    Ok(())
}

/// Rebuild the table B-tree rooted at `root` without the row
/// `skip`: collect the surviving rows, free every page but the
/// root, reset the root to an empty leaf, and re-insert.
async fn btree_rebuild_without<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    skip: i64,
) -> Result<(), S::Error> {
    let mut keep: AVec<(i64, AVec<u8>)> = AVec::new();
    {
        let mut cur = ScanCursor::new(root);
        while let Some((rowid, payload)) = cur.next_row(pager).await? {
            if rowid != skip {
                // Payloads are carried verbatim: the rebuild
                // is byte-exact, short records stay short.
                keep.push((rowid, payload));
            }
        }
    }
    let pages = collect_table_pages(pager, root).await?;
    for p in pages.iter() {
        if *p != root {
            pager.free_page(*p).await?;
        }
    }
    create_empty_root(pager, root).await?;
    for (rowid, payload) in keep.iter() {
        btree_insert(pager, root, *rowid, payload.as_slice()).await?;
    }
    Ok(())
}

/// Overwrite row `rowid` of the table rooted at `root` in place
/// when its stored payload is exactly `payload.len()` bytes (v0.16,
/// `wal`): the pin's `btreeOverwriteCell`, which writes (and so
/// dirties, logging a WAL frame) only the local part and the overflow
/// pages whose bytes differ. `Ok(false)`: no such row, or a payload
/// of another size, which the caller deletes and inserts. A WITHOUT
/// ROWID row is only recognised unchanged (`Ok(true)`, nothing
/// written).
#[cfg(feature = "wal")]
pub async fn btree_overwrite<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
    payload: &[u8],
) -> Result<bool, S::Error> {
    #[cfg(feature = "without_rowid")]
    if pager.wr.table_of(root).is_some() {
        return Ok(seek_rowid(pager, root, rowid)
            .await?
            .is_some_and(|b| b.as_slice() == payload));
    }
    #[cfg(feature = "utf16")]
    let conv = stored_record::<S::Error>(pager.text_enc(), payload)?;
    #[cfg(feature = "utf16")]
    let payload = conv.as_ref().map_or(payload, |c| c.as_slice());
    let usable = pager.usable() as usize;
    let mut page = root;
    let mut hops = 0u32;
    let cell = loop {
        hops = hops.saturating_add(1);
        if hops > 64 {
            return Err(Error::CorruptPage { page });
        }
        let idx = pager.load(page).await?;
        let d = pager.frame_data(idx);
        if head::<S::Error>(d, page)?.is_leaf() {
            let cells = leaf_cells_meta::<S::Error>(d, page, usable)?;
            match cells.iter().find(|c| c.rowid == rowid) {
                Some(c) => break *c,
                None => return Ok(false),
            }
        }
        let (pairs, rightmost) = interior_cells::<S::Error>(d, page)?;
        page = pairs
            .iter()
            .find(|(_, k)| rowid <= *k)
            .map_or(rightmost, |(c, _)| *c);
    };
    if cell.payload_len != payload.len() || cell.local_len > payload.len() {
        return Ok(false);
    }
    let (local, mut rest) = payload.split_at(cell.local_len);
    let end = cell
        .local_off
        .checked_add(cell.local_len)
        .ok_or(Error::CorruptPage { page })?;
    let idx = pager.load(page).await?;
    if pager.frame_data(idx).get(cell.local_off..end) != Some(local) {
        let idx = pager.dirty_page(page).await?;
        pager
            .frame_data_mut(idx)
            .get_mut(cell.local_off..end)
            .ok_or(Error::CorruptPage { page })?
            .copy_from_slice(local);
    }
    let per = usable.checked_sub(4).ok_or(Error::CorruptPage { page })?;
    let mut ov = cell.overflow;
    while !rest.is_empty() {
        if ov == 0 || per == 0 {
            return Err(Error::CorruptPage { page });
        }
        let (chunk, tail) = rest.split_at(rest.len().min(per));
        let idx = pager.load(ov).await?;
        let d = pager.frame_data(idx);
        let next = d
            .get(..4)
            .map(|b| be_u32(b, 0))
            .ok_or(Error::CorruptPage { page: ov })?;
        let span = 4..4 + chunk.len();
        if d.get(span.clone()) != Some(chunk) {
            let idx = pager.dirty_page(ov).await?;
            pager
                .frame_data_mut(idx)
                .get_mut(span)
                .ok_or(Error::CorruptPage { page: ov })?
                .copy_from_slice(chunk);
        }
        rest = tail;
        ov = next;
    }
    Ok(true)
}

/// Delete the row `rowid` from the table rooted at `root`.
/// Returns whether a row was present.
pub async fn btree_delete<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
) -> Result<bool, S::Error> {
    // A WITHOUT ROWID table (v0.16): the handle's row leaves the
    // clustered tree.
    #[cfg(feature = "without_rowid")]
    if let Some(t) = pager.wr.table_of(root) {
        return crate::wr::delete(pager, t, rowid).await;
    }
    let usable = pager.usable() as usize;
    // Descent, remembering (interior page, child index) pairs —
    // the balance cascade walks this path back up.
    let mut path: AVec<(u32, usize)> = AVec::new();
    let mut page = root;
    let (leaf_page, cells, pos) = loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_TABLE {
                return Err(Error::Unsupported);
            }
            let cells = leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            let pos = cells.iter().position(|c| c.rowid == rowid);
            break (page, cells, pos);
        }
        let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
        let mut child = rightmost;
        let mut ci = pairs.len();
        for (i, (c, k)) in pairs.iter().enumerate() {
            if rowid <= *k {
                child = *c;
                ci = i;
                break;
            }
        }
        path.push((page, ci));
        page = child;
    };
    let Some(pos) = pos else {
        return Ok(false);
    };
    let removed = cells.as_slice()[pos].clone();
    let mut kept: AVec<LeafCell> = AVec::new();
    for (i, c) in cells.iter().enumerate() {
        if i != pos {
            kept.push(c.clone());
        }
    }
    if leaf_page == root {
        // A root leaf may legally empty; no siblings exist.
        write_leaf_page(pager, leaf_page, kept.as_slice(), false).await?;
        free_table_overflow(pager, &removed).await?;
        return Ok(true);
    }
    if !page_underfull(table_leaf_free(usable, leaf_page, kept.as_slice()), usable) {
        write_leaf_page(pager, leaf_page, kept.as_slice(), false).await?;
        free_table_overflow(pager, &removed).await?;
        return Ok(true);
    }
    // Underfull: balance the leaf's window. The leaf page itself
    // has not been written yet — the balance gathers the kept
    // cells from `preset`, and a Rebuild verdict therefore still
    // sees the untouched tree (the row included) on disk.
    let (parent, trigger) = *path.as_slice().last().expect("non-root leaf has a parent");
    let outcome =
        balance_table_window(pager, root, parent, trigger, Some((leaf_page, kept))).await?;
    let mut removed_children = match outcome {
        TableBalance::Rebuild => {
            btree_rebuild_without(pager, root, rowid).await?;
            return Ok(true);
        }
        TableBalance::Done { children_removed } => children_removed,
    };
    free_table_overflow(pager, &removed).await?;
    // Cascade: while the parent that just lost children is
    // itself underfull, balance it among its own siblings; a
    // root that ends with one child collapses.
    let mut cur = path.len() - 1; // path index of the page whose children were just balanced
    loop {
        let cur_page = path.as_slice()[cur].0;
        if cur_page == root {
            collapse_table_root(pager, root).await?;
            break;
        }
        if removed_children == 0 {
            break;
        }
        // Is the current page (an interior page that lost
        // children) itself underfull?
        let idx = pager.load(cur_page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), cur_page)?;
        if h.kind != KIND_INTERIOR_TABLE {
            break;
        }
        let (cpairs, _) = interior_cells::<S::Error>(snap.as_slice(), cur_page)?;
        if !page_underfull(
            table_interior_free(usable, cur_page, cpairs.as_slice()),
            usable,
        ) {
            break;
        }
        if cur == 0 {
            // cur_page is the root (handled above) — unreachable,
            // but never loop forever on a foreign shape.
            break;
        }
        let (gp, gtrigger) = path.as_slice()[cur - 1];
        let outcome = balance_table_window(pager, root, gp, gtrigger, None).await?;
        match outcome {
            TableBalance::Rebuild => {
                // The lower levels are already consistently
                // rebalanced and the row is gone; a whole-tree
                // rebuild from the surviving rows is still a
                // correct (if blunt) finish.
                btree_rebuild_without(pager, root, rowid).await?;
                return Ok(true);
            }
            TableBalance::Done { children_removed } => {
                removed_children = children_removed;
            }
        }
        cur -= 1;
    }
    Ok(true)
}

// ----------------------------------------------------------------
// Index B-trees (v0.3, SPEC §2/§5.4) — feature `indexes`.
//
// An index key is a record of the indexed columns followed by the
// rowid (SQFMT §2.1/§2.6, verified against the pinned oracle).
// Index B-trees are true B-trees, not B+-trees: a key stored in an
// interior page is a real entry and appears nowhere else (also
// oracle-verified: after splits, the promoted key is absent from
// both leaves). Interior cell = u32 child + key payload; the child
// holds keys strictly below the cell's key. Overflow uses the
// index thresholds of SQFMT §1.6.
// ----------------------------------------------------------------

/// Local-payload byte count for an index cell (SQFMT §1.6):
/// X = ((U-12)*64/255)-23; if P > X, M = ((U-12)*32/255)-23 and
/// K = M + ((P-M) mod (U-4)); keep K if K <= X else M.
#[cfg(any(feature = "indexes", feature = "v05"))]
pub fn index_local(usable: usize, payload_len: usize) -> usize {
    let x = (usable - 12) * 64 / 255 - 23;
    if payload_len <= x {
        return payload_len;
    }
    let m = (usable - 12) * 32 / 255 - 23;
    let k = m + (payload_len - m) % (usable - 4);
    if k <= x { k } else { m }
}

/// A cell takes at least 4 bytes of its page, as on the pin (its
/// `fillInCell` pads a shorter one with zeros; v0.16: only a WITHOUT
/// ROWID table's records, with no rowid, are that short). `after` is
/// what follows the cell on its page, read for the padding bytes.
#[cfg(any(feature = "indexes", feature = "v05"))]
#[inline(always)]
fn pad_cell(img: &mut AVec<u8>, after: &[u8]) {
    #[cfg(feature = "without_rowid")]
    {
        let mut k = 0usize;
        while img.len() < 4 {
            img.push(after.get(k).copied().unwrap_or(0));
            k += 1;
        }
    }
    let _ = (img, after);
}

/// One parsed index cell (leaf or interior).
#[cfg(any(feature = "indexes", feature = "v05"))]
#[derive(Clone)]
pub struct IndexCell {
    /// Left child page (interior cells only; 0 in leaf cells).
    pub child: u32,
    /// The full on-page cell image (child prefix when interior,
    /// then varint payload length, local payload, overflow ptr).
    pub bytes: AVec<u8>,
    pub payload_len: usize,
    pub local_len: usize,
    pub overflow: u32,
    /// Offset of the local payload inside `bytes`.
    pub payload_off: usize,
}

#[cfg(any(feature = "indexes", feature = "v05"))]
impl IndexCell {
    pub fn local_bytes(&self) -> &[u8] {
        &self.bytes.as_slice()[self.payload_off..self.payload_off + self.local_len]
    }
}

#[cfg(any(feature = "indexes", feature = "v05"))]
fn parse_index_cell<E>(
    bytes: &[u8],
    at: usize,
    usable: usize,
    page: u32,
    interior: bool,
) -> Result<IndexCell, E> {
    let mut off = at;
    let mut child = 0u32;
    if interior {
        if at + 4 > bytes.len() {
            return Err(Error::CorruptPage { page });
        }
        child = be_u32(bytes, at);
        off += 4;
    }
    let cell_bytes = bytes.get(off..).ok_or(Error::CorruptPage { page })?;
    let (plen, n1) = decode_varint::<E>(cell_bytes)?;
    let plen = plen as usize;
    let local = index_local(usable, plen);
    let start = off + n1;
    let end = start
        .checked_add(local)
        .ok_or(Error::CorruptPage { page })?;
    if end > bytes.len() {
        return Err(Error::CorruptPage { page });
    }
    let mut overflow = 0u32;
    let mut total = (off - at) + n1 + local;
    if plen > local {
        if end + 4 > bytes.len() {
            return Err(Error::CorruptPage { page });
        }
        overflow = be_u32(bytes, end);
        total += 4;
    }
    let mut img = AVec::with_capacity(total);
    img.extend_from_slice(&bytes[at..at + total]);
    pad_cell(&mut img, bytes.get(at + total..).unwrap_or(&[]));
    Ok(IndexCell {
        child,
        bytes: img,
        payload_len: plen,
        local_len: local,
        overflow,
        payload_off: (off - at) + n1,
    })
}

/// Parse all cells of an index leaf page.
#[cfg(any(feature = "indexes", feature = "v05"))]
pub fn index_leaf_cells<E>(bytes: &[u8], page: u32, usable: usize) -> Result<AVec<IndexCell>, E> {
    let h = head::<E>(bytes, page)?;
    if h.kind != KIND_LEAF_INDEX {
        return Err(Error::CorruptPage { page });
    }
    let mut out = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let ptr = cell_ptr::<E>(bytes, &h, i, page)?;
        out.push(parse_index_cell::<E>(bytes, ptr, usable, page, false)?);
    }
    Ok(out)
}

/// Parse an index interior page: cells (each with its left child)
/// plus the rightmost child.
#[cfg(any(feature = "indexes", feature = "v05"))]
pub fn index_interior_cells<E>(
    bytes: &[u8],
    page: u32,
    usable: usize,
) -> Result<(AVec<IndexCell>, u32), E> {
    let h = head::<E>(bytes, page)?;
    if h.kind != KIND_INTERIOR_INDEX {
        return Err(Error::CorruptPage { page });
    }
    let mut out = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let ptr = cell_ptr::<E>(bytes, &h, i, page)?;
        out.push(parse_index_cell::<E>(bytes, ptr, usable, page, true)?);
    }
    Ok((out, h.rightmost))
}

#[cfg(feature = "indexes")]
fn build_index_image(
    page: u32,
    page_size: usize,
    cells: &[IndexCell],
    rightmost: u32,
    interior: bool,
    base: &[u8],
) -> Option<AVec<u8>> {
    let hdr_off = if page == 1 { 100 } else { 0 };
    let hdr_len = if interior { 12 } else { 8 };
    // A cell list that does not fit the page is a balance bug, never
    // something to write: refuse it instead of underflowing.
    if index_page_need(cells, hdr_off, interior) > page_size || base.len() < hdr_off {
        return None;
    }
    let mut img = vec_zeroed(page_size);
    if page == 1 {
        img.as_mut_slice()[0..100].copy_from_slice(&base[0..100]);
    }
    let out = img.as_mut_slice();
    let mut content = page_size;
    let mut ptrs = AVec::new();
    for c in cells {
        content -= c.bytes.len();
        out[content..content + c.bytes.len()].copy_from_slice(c.bytes.as_slice());
        ptrs.push(content as u16);
    }
    out[hdr_off] = if interior {
        KIND_INTERIOR_INDEX
    } else {
        KIND_LEAF_INDEX
    };
    put_be_u16(out, hdr_off + 1, 0);
    put_be_u16(out, hdr_off + 3, cells.len() as u16);
    put_be_u16(out, hdr_off + 5, content_code(content));
    out[hdr_off + 7] = 0;
    if interior {
        put_be_u32(out, hdr_off + 8, rightmost);
    }
    for (i, p) in ptrs.iter().enumerate() {
        put_be_u16(out, hdr_off + hdr_len + 2 * i, *p);
    }
    Some(img)
}

#[cfg(feature = "indexes")]
fn index_page_need(cells: &[IndexCell], hdr_off: usize, interior: bool) -> usize {
    let content: usize = cells.iter().map(|c| c.bytes.len()).sum();
    hdr_off + if interior { 12 } else { 8 } + 2 * cells.len() + content
}

/// Write `cells` as the complete image of index page `page`
/// (leaf when `interior` is false). Fresh pages (from
/// `alloc_page`) are already dirty and unjournaled; existing
/// pages are journaled via `dirty_page` first.
#[cfg(feature = "indexes")]
async fn write_index_page<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
    cells: &[IndexCell],
    rightmost: u32,
    interior: bool,
    fresh: bool,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    let idx = if fresh {
        let idx = pager.load(page).await?;
        pager.mark_frame_dirty(idx);
        idx
    } else {
        pager.dirty_page(page).await?
    };
    let base = copy_bytes(pager.frame_data(idx));
    let img = build_index_image(page, page_size, cells, rightmost, interior, base.as_slice())
        .ok_or(Error::CorruptPage { page })?;
    pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
    Ok(())
}

/// Initialise a freshly allocated page as an empty index leaf —
/// a new index's root.
#[cfg(feature = "indexes")]
pub async fn create_empty_index_root<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    let idx = pager.load(page).await?;
    pager.mark_frame_dirty(idx);
    let base = copy_bytes(pager.frame_data(idx));
    let empty: &[IndexCell] = &[];
    let img = build_index_image(page, page_size, empty, 0, false, base.as_slice())
        .ok_or(Error::CorruptPage { page })?;
    pager.frame_data_mut(idx).copy_from_slice(img.as_slice());
    Ok(())
}

/// A decoded index entry: the key columns' values followed by the
/// rowid (an Integer) as the last value.
#[cfg(feature = "indexes")]
#[derive(Clone)]
pub struct IndexEntry {
    pub values: AVec<OwnedValue>,
}

#[cfg(feature = "indexes")]
impl IndexEntry {
    pub fn rowid<E>(&self) -> Result<i64, E> {
        match self.values.as_slice().last() {
            Some(OwnedValue::Integer(r)) => Ok(*r),
            _ => Err(Error::CorruptRecord),
        }
    }
}

/// How the entries of the index B-tree rooted at `root` decode:
/// `desc.len() + 1` fields, the last the rowid, or — 0 — every field
/// of a record with no rowid (v0.16, `without_rowid`: a WITHOUT
/// ROWID table's clustered tree or one of its secondary indexes,
/// whose callers pass the tree's full comparison order).
#[cfg(feature = "indexes")]
fn entry_fields<S: Storage>(pager: &Pager<S>, root: u32, desc: &[KeyOrder]) -> usize {
    #[cfg(feature = "without_rowid")]
    if pager.wr.is_raw(root) {
        return 0;
    }
    let _ = (pager, root);
    desc.len() + 1
}

/// Decode an index key payload (a record of `nfields` values; with
/// `nfields == 0`, a record of any length with no rowid — see
/// `entry_fields`).
#[cfg(feature = "indexes")]
pub fn decode_index_entry<E>(payload: &[u8], nfields: usize) -> Result<IndexEntry, E> {
    #[cfg(feature = "without_rowid")]
    if nfields == 0 {
        let vals = decode_record::<E>(payload, usize::MAX)?;
        let mut out = AVec::new();
        for v in vals.iter() {
            out.push(OwnedValue::from_value(*v));
        }
        return Ok(IndexEntry { values: out });
    }
    let vals = decode_record::<E>(payload, nfields)?;
    if vals.len() != nfields {
        return Err(Error::CorruptRecord);
    }
    let mut out = AVec::new();
    for v in vals.iter() {
        out.push(OwnedValue::from_value(*v));
    }
    let entry = IndexEntry { values: out };
    // The trailing field is the rowid and must be an integer.
    let _ = entry.rowid::<E>()?;
    Ok(entry)
}

/// Every key payload of the index rooted at `root`, raw, in key
/// order (v0.16, `v16`: VACUUM copies indexes byte for byte).
#[cfg(all(feature = "v16", feature = "indexes"))]
pub async fn index_payloads<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
) -> Result<AVec<AVec<u8>>, S::Error> {
    let usable = pager.usable() as usize;
    let mut out: AVec<AVec<u8>> = AVec::new();
    // (page, next action): an interior page's actions alternate
    // descend-child / emit-divider, as `IndexCursor` walks them.
    let mut stack: AVec<(u32, usize)> = AVec::new();
    stack.push((root, 0));
    let mut steps = 0u64;
    while let Some((page, seq)) = stack.pop() {
        steps = steps.saturating_add(1);
        if steps
            > u64::from(pager.page_count())
                .saturating_mul(1024)
                .saturating_add(64)
        {
            return Err(Error::CorruptPage { page });
        }
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_INDEX {
                return Err(Error::CorruptPage { page });
            }
            for c in index_leaf_cells::<S::Error>(snap.as_slice(), page, usable)?.iter() {
                out.push(index_cell_payload(pager, c).await?);
            }
            continue;
        }
        if h.kind != KIND_INTERIOR_INDEX {
            return Err(Error::CorruptPage { page });
        }
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
        let n = cells.len();
        if seq > 2 * n {
            continue;
        }
        stack.push((page, seq + 1));
        if seq % 2 == 1 {
            let Some(c) = cells.as_slice().get((seq - 1) / 2) else {
                return Err(Error::CorruptPage { page });
            };
            out.push(index_cell_payload(pager, c).await?);
        } else {
            let child = match cells.as_slice().get(seq / 2) {
                Some(c) => c.child,
                None => rightmost,
            };
            if child == 0 || child == page {
                return Err(Error::CorruptPage { page });
            }
            stack.push((child, 0));
        }
    }
    Ok(out)
}

/// Reassemble a cell's full key payload (local part + overflow).
#[cfg(feature = "indexes")]
async fn index_cell_payload<S: Storage>(
    pager: &mut Pager<S>,
    cell: &IndexCell,
) -> Result<AVec<u8>, S::Error> {
    let local = copy_bytes(cell.local_bytes());
    assemble_payload(pager, local.as_slice(), cell.payload_len, cell.overflow).await
}

/// Compare two decoded keys field by field under each key
/// column's collation (v0.16); `desc[j].desc` reverses
/// column j's comparison (the rowid field, last, is never in
/// `desc` and always compares ascending), in a database of text
/// encoding `enc`. NULL ordering follows
/// `compare_values` (NULL smallest; reversed under DESC, which is
/// what the oracle does).
#[cfg(feature = "indexes")]
pub fn compare_entries(
    a: &[OwnedValue],
    b: &[OwnedValue],
    desc: &[KeyOrder],
    enc: crate::record::TextEnc,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for j in 0..a.len().min(b.len()) {
        let o = desc.get(j).copied().unwrap_or_default();
        let c = compare_coll_in(&a[j].as_value(), &b[j].as_value(), o.coll, enc);
        let c = if o.desc { c.reverse() } else { c };
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// Build the on-page image for a cell carrying `source`'s key
/// payload, reusing its overflow chain (the local length depends
/// only on usable size and payload length, so it is identical on
/// every page of the tree). `source_prefix` is the length of the
/// child prefix in the source image (4 for an interior cell, 0
/// for a leaf cell); `child` is the new cell's child (promotion
/// into an interior page), or None to produce a leaf-form image.
#[cfg(feature = "indexes")]
fn recell(child: Option<u32>, source: &IndexCell, source_prefix: usize) -> IndexCell {
    let mut bytes = AVec::new();
    if let Some(c) = child {
        for b in c.to_be_bytes() {
            bytes.push(b);
        }
    }
    bytes.extend_from_slice(&source.bytes.as_slice()[source_prefix..]);
    pad_cell(&mut bytes, &[]);
    IndexCell {
        child: child.unwrap_or(0),
        payload_off: source.payload_off - source_prefix + if child.is_some() { 4 } else { 0 },
        bytes,
        payload_len: source.payload_len,
        local_len: source.local_len,
        overflow: source.overflow,
    }
}

/// Partition a combined index sequence into page groups plus
/// one lifted divider per boundary, for an INSERT balance
/// (v0.15): group boundaries follow `distribute_insert`; every
/// group is capacity-checked (its page holds the group minus
/// the lifted last item). On any misfit, falls back to
/// `partition_index` (the v0.14 greedy/quota partition).
#[cfg(feature = "indexes")]
fn partition_index_ins(
    items: &[IndexCell],
    n_new: usize,
    usable: usize,
    interior: bool,
    win: usize,
) -> (AVec<AVec<IndexCell>>, AVec<IndexCell>) {
    let weights: AVec<usize> = {
        let mut w = AVec::new();
        for c in items.iter() {
            w.push(c.bytes.len() + 2);
        }
        w
    };
    let phantom = usable - if interior { 12 } else { 8 };
    let groups = distribute_insert(items, weights.as_slice(), n_new, phantom, win);
    let mut pages: AVec<AVec<IndexCell>> = AVec::new();
    let mut dividers: AVec<IndexCell> = AVec::new();
    let mut ok = groups.len() == n_new;
    if ok {
        for (i, g) in groups.iter().enumerate() {
            let mut g = g.clone();
            if i + 1 < groups.len() {
                match g.pop() {
                    Some(d) => dividers.push(d),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if g.is_empty() || index_page_need(g.as_slice(), 0, interior) > usable {
                ok = false;
                break;
            }
            pages.push(g);
        }
    }
    if ok {
        return (pages, dividers);
    }
    greedy_index_partition_ins(items, usable, interior)
}

/// Greedy index partition for inserts: pack pages in order; when the
/// next item does not fit and more items follow, that item lifts as
/// the divider between the pages (it consumes no leaf space -- the
/// accounting the pin's page counts follow). The last item never
/// lifts; it starts a fresh page instead.
#[cfg(feature = "indexes")]
fn greedy_index_partition_ins(
    items: &[IndexCell],
    usable: usize,
    interior: bool,
) -> (AVec<AVec<IndexCell>>, AVec<IndexCell>) {
    let hdr = if interior { 12 } else { 8 };
    let mut pages: AVec<AVec<IndexCell>> = AVec::new();
    let mut dividers: AVec<IndexCell> = AVec::new();
    let mut cur: AVec<IndexCell> = AVec::new();
    let mut need = hdr;
    for (i, it) in items.iter().enumerate() {
        let add = it.bytes.len() + 2;
        if !cur.is_empty() && need + add > usable {
            if i + 1 < items.len() {
                dividers.push(it.clone());
                pages.push(core::mem::take(&mut cur));
                need = hdr;
                continue;
            }
            pages.push(core::mem::take(&mut cur));
            need = hdr;
        }
        cur.push(it.clone());
        need += add;
    }
    if !cur.is_empty() {
        pages.push(cur);
    }
    (pages, dividers)
}

/// Minimum page count for an index insert balance: the page count of
/// the greedy divider-aware packing.
#[cfg(feature = "indexes")]
fn index_min_pages_ins(items: &[IndexCell], usable: usize, interior: bool) -> usize {
    greedy_index_partition_ins(items, usable, interior).0.len()
}

/// Outcome of one insert-side window balance (index tree).
#[cfg(feature = "indexes")]
enum InsUpIdx {
    /// The balance completed; every ancestor page is written.
    Done,
    /// The balance completed at this level, but the parent page
    /// `page`'s new model (cells, rightmost) no longer fits: the
    /// cascade balances `page` among its own siblings with this
    /// model as the preset.
    Overflow {
        page: u32,
        cells: AVec<IndexCell>,
        rightmost: u32,
    },
}

/// Leaf-level insert balance for an index: redistribute the
/// window leaves' combined entry sequence (window entries
/// interleaved with the parent dividers between them; the
/// trigger leaf's list is `preset_cells`, already merged with
/// the inserted entry) over the smallest page count that holds
/// it. Mirrors the v0.14 delete window's gather/splice, with
/// growth and a parent-overflow report for the cascade.
#[cfg(feature = "indexes")]
#[allow(clippy::too_many_arguments)]
async fn ins_balance_index_leaf_window<S: Storage>(
    pager: &mut Pager<S>,
    parent: u32,
    trigger: usize,
    preset_page: u32,
    preset_cells: AVec<IndexCell>,
) -> Result<InsUpIdx, S::Error> {
    let usable = pager.usable() as usize;
    let (pcells, _rightmost, children) = index_children(pager, parent).await?;
    let (start, end) = window_range(children.len(), trigger.min(children.len() - 1));
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children[j]);
        }
        w
    };
    // Gather: window leaves' entries in order, interleaved with
    // the parent dividers between them (converted to leaf form).
    let mut all: AVec<IndexCell> = AVec::new();
    for (wi, wp) in window_pages.iter().enumerate() {
        let wp = *wp;
        let cells = if wp == preset_page {
            preset_cells.clone()
        } else {
            let idx = pager.load(wp).await?;
            let snap = copy_bytes(pager.frame_data(idx));
            index_leaf_cells::<S::Error>(snap.as_slice(), wp, usable)?
        };
        for c in cells.iter() {
            all.push(c.clone());
        }
        if wi + 1 < window_pages.len() {
            all.push(recell(None, &pcells[start + wi], 4));
        }
    }
    let n_new = index_min_pages_ins(all.as_slice(), usable, false);
    let (pages, dividers) =
        partition_index_ins(all.as_slice(), n_new, usable, false, window_pages.len());
    let n_new = pages.len();
    let mut pages_out: AVec<u32> = window_pages.clone();
    while pages_out.len() < n_new {
        let (np, _) = pager.alloc_page().await?;
        pages_out.push(np);
    }
    for (i, g) in pages.iter().enumerate() {
        let fresh = i >= window_pages.len();
        write_index_page(
            pager,
            pages_out.as_slice()[i],
            g.as_slice(),
            0,
            false,
            fresh,
        )
        .await?;
    }
    for wp in window_pages.iter().skip(n_new) {
        pager.free_page(*wp).await?;
    }
    let new_pages: AVec<u32> = {
        let mut v = AVec::new();
        for wp in pages_out.iter().take(n_new) {
            v.push(*wp);
        }
        v
    };
    let (nc, nright) = index_parent_after(
        pcells.as_slice(),
        children.as_slice(),
        start,
        end,
        new_pages.as_slice(),
        dividers.as_slice(),
        0,
    );
    if index_page_need(nc.as_slice(), 0, true) <= usable {
        write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
        return Ok(InsUpIdx::Done);
    }
    Ok(InsUpIdx::Overflow {
        page: parent,
        cells: nc,
        rightmost: nright,
    })
}

/// Interior-level insert balance for an index: redistribute the
/// window interior pages' combined entry sequence over the
/// smallest page count that holds it. The trigger page's model
/// comes from the preset (its on-disk image is the pre-balance
/// model).
#[cfg(feature = "indexes")]
#[allow(clippy::too_many_arguments)]
async fn ins_balance_index_interior_window<S: Storage>(
    pager: &mut Pager<S>,
    parent: u32,
    trigger: usize,
    preset_page: u32,
    preset_cells: AVec<IndexCell>,
    preset_rightmost: u32,
) -> Result<InsUpIdx, S::Error> {
    let usable = pager.usable() as usize;
    let (pcells, _rightmost, children) = index_children(pager, parent).await?;
    let (start, end) = window_range(children.len(), trigger.min(children.len() - 1));
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children[j]);
        }
        w
    };
    // Gather: window interior pages' entries in order, each
    // page's rightmost pointer becoming the child of the
    // following parent divider item; the last page's rightmost
    // is the sequence's trailing child.
    let mut all: AVec<IndexCell> = AVec::new();
    let mut trailing = 0u32;
    for (wi, wp) in window_pages.iter().enumerate() {
        let wp = *wp;
        let (cells, rightmost) = if wp == preset_page {
            (preset_cells.clone(), preset_rightmost)
        } else {
            let idx = pager.load(wp).await?;
            let snap = copy_bytes(pager.frame_data(idx));
            index_interior_cells::<S::Error>(snap.as_slice(), wp, usable)?
        };
        for c in cells.iter() {
            all.push(c.clone());
        }
        if wi + 1 < window_pages.len() {
            let mut d = pcells[start + wi].clone();
            patch_child(&mut d, rightmost);
            all.push(d);
        } else {
            trailing = rightmost;
        }
    }
    let n_new = index_min_pages_ins(all.as_slice(), usable, true);
    let (pages, dividers) =
        partition_index_ins(all.as_slice(), n_new, usable, true, window_pages.len());
    let n_new = pages.len();
    let mut pages_out: AVec<u32> = window_pages.clone();
    while pages_out.len() < n_new {
        let (np, _) = pager.alloc_page().await?;
        pages_out.push(np);
    }
    for (i, g) in pages.iter().enumerate() {
        let right = if i + 1 < n_new {
            dividers.as_slice()[i].child
        } else {
            trailing
        };
        let fresh = i >= window_pages.len();
        write_index_page(
            pager,
            pages_out.as_slice()[i],
            g.as_slice(),
            right,
            true,
            fresh,
        )
        .await?;
    }
    for wp in window_pages.iter().skip(n_new) {
        pager.free_page(*wp).await?;
    }
    let new_pages: AVec<u32> = {
        let mut v = AVec::new();
        for wp in pages_out.iter().take(n_new) {
            v.push(*wp);
        }
        v
    };
    let (nc, nright) = index_parent_after(
        pcells.as_slice(),
        children.as_slice(),
        start,
        end,
        new_pages.as_slice(),
        dividers.as_slice(),
        4,
    );
    if index_page_need(nc.as_slice(), 0, true) <= usable {
        write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
        return Ok(InsUpIdx::Done);
    }
    Ok(InsUpIdx::Overflow {
        page: parent,
        cells: nc,
        rightmost: nright,
    })
}

/// Finish an index insert cascade that overflowed the root's
/// model: redistribute the root's entries over two fresh
/// interior pages and rewrite the root as a one-entry interior
/// over them.
#[cfg(feature = "indexes")]
async fn ins_root_index_interior<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    cells: AVec<IndexCell>,
    rightmost: u32,
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    let (pages, dividers) = partition_index_ins(cells.as_slice(), 2, usable, true, 1);
    if pages.len() != 2 || dividers.len() != 1 {
        return Err(Error::Unsupported);
    }
    let (lp, _) = pager.alloc_page().await?;
    let (rp, _) = pager.alloc_page().await?;
    write_index_page(
        pager,
        lp,
        pages.as_slice()[0].as_slice(),
        dividers.as_slice()[0].child,
        true,
        true,
    )
    .await?;
    write_index_page(
        pager,
        rp,
        pages.as_slice()[1].as_slice(),
        rightmost,
        true,
        true,
    )
    .await?;
    let mut one: AVec<IndexCell> = AVec::new();
    one.push(recell(Some(lp), &dividers.as_slice()[0], 4));
    write_index_page(pager, root, one.as_slice(), rp, true, false).await?;
    Ok(())
}

/// Drive an index insert cascade upward, mirroring
/// `cascade_table_up`.
#[cfg(feature = "indexes")]
async fn cascade_index_up<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    path: &[(u32, usize)],
    first: InsUpIdx,
) -> Result<(), S::Error> {
    let mut up = first;
    loop {
        let InsUpIdx::Overflow {
            page,
            cells,
            rightmost,
        } = up
        else {
            return Ok(());
        };
        if page == root {
            return ins_root_index_interior(pager, root, cells, rightmost).await;
        }
        let pos = path
            .iter()
            .position(|(p, _)| *p == page)
            .ok_or(Error::Unsupported)?;
        if pos == 0 {
            return Err(Error::Unsupported);
        }
        let (gp, gtrigger) = path[pos - 1];
        up = ins_balance_index_interior_window(pager, gp, gtrigger, page, cells, rightmost).await?;
    }
}

/// Binary-search a sorted cell list for the first cell above
/// `probe`; returns `cells.len()` when none is. An equal cell is a
/// `DuplicateRowid` error: it is never excluded from the search
/// range, so it is always probed before the range empties.
#[cfg(feature = "indexes")]
async fn index_search_pos<S: Storage>(
    pager: &mut Pager<S>,
    cells: &[IndexCell],
    probe: &IndexEntry,
    nfields: usize,
    desc: &[KeyOrder],
) -> Result<usize, S::Error> {
    use core::cmp::Ordering;
    let (mut lo, mut hi) = (0usize, cells.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let p = index_cell_payload(pager, &cells[mid]).await?;
        let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
        match compare_entries(
            probe.values.as_slice(),
            e.values.as_slice(),
            desc,
            pager.text_enc(),
        ) {
            Ordering::Equal => {
                return Err(Error::Schema {
                    kind: SchemaKind::DuplicateRowid,
                });
            }
            Ordering::Less => hi = mid,
            Ordering::Greater => lo = mid + 1,
        }
    }
    Ok(lo)
}

/// Insert the key `payload` (a full index record) into the index
/// rooted at `root`. The root page number never changes.
/// Duplicate full keys (same columns AND rowid) are a
/// `DuplicateRowid` schema error — they can only arise from an
/// engine bug, since UNIQUE conflicts are pre-checked by the
/// session layer on the key columns alone.
#[cfg(feature = "without_rowid")]
pub async fn index_insert<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    payload: &[u8],
    desc: &[KeyOrder],
) -> Result<(), S::Error> {
    // A WITHOUT ROWID table's trees (v0.16): its clustered tree
    // takes its rows through `btree_insert`; a secondary index's
    // entry trades the rowid for the row's key columns.
    match pager.wr.view_of(root) {
        crate::wr::WrView::Pk(_) => return Ok(()),
        crate::wr::WrView::Sec(i) => {
            let (raw, orders) = pager.wr.sec_payload::<S::Error>(i, payload)?;
            return index_insert_core(pager, root, raw.as_slice(), orders.as_slice()).await;
        }
        _ => {}
    }
    index_insert_core(pager, root, payload, desc).await
}

/// Without WITHOUT ROWID tables there is nothing to translate.
#[cfg(all(feature = "indexes", not(feature = "without_rowid")))]
pub use self::index_insert_core as index_insert;

/// `index_insert` without the WITHOUT ROWID translation: `payload`
/// is the tree's own record and `desc` its full comparison order.
#[cfg(feature = "indexes")]
pub async fn index_insert_core<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    payload: &[u8],
    desc: &[KeyOrder],
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    let nfields = entry_fields(pager, root, desc);
    let probe = decode_index_entry::<S::Error>(payload, nfields)?;
    // ---- descent, remembering (interior page, child index) ----
    // The child index counts into the page's children array
    // (cells' children in order, then the rightmost pointer).
    let mut path: AVec<(u32, usize)> = AVec::new();
    let mut page = root;
    let (leaf_page, leaf_cells, leaf_pos) = loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_INDEX {
                return Err(Error::Unsupported);
            }
            let cells = index_leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            // The leaf is sorted: the probe goes before the first cell
            // above it, and a duplicate can only be a cell the binary
            // search lands on (an equal cell is never excluded).
            let pos = index_search_pos(pager, cells.as_slice(), &probe, nfields, desc).await?;
            break (page, cells, pos);
        }
        if h.kind != KIND_INTERIOR_INDEX {
            return Err(Error::Unsupported);
        }
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
        let child_idx = index_search_pos(pager, cells.as_slice(), &probe, nfields, desc).await?;
        let child = if child_idx < cells.len() {
            cells.as_slice()[child_idx].child
        } else {
            rightmost
        };
        path.push((page, child_idx));
        page = child;
    };

    // ---- build the new leaf cell (overflow first) ----
    #[cfg(feature = "utf16")]
    let conv = stored_record::<S::Error>(pager.text_enc(), payload)?;
    #[cfg(feature = "utf16")]
    let payload = conv.as_ref().map_or(payload, |c| c.as_slice());
    let local_len = index_local(usable, payload.len());
    let overflow_head = write_overflow(pager, payload, local_len).await?;
    let mut cell_bytes = AVec::new();
    encode_varint(payload.len() as u64, &mut cell_bytes);
    for b in &payload[..local_len] {
        cell_bytes.push(*b);
    }
    if overflow_head != 0 {
        for b in overflow_head.to_be_bytes() {
            cell_bytes.push(b);
        }
    }
    let payload_off = cell_bytes.len() - local_len - if overflow_head != 0 { 4 } else { 0 };
    pad_cell(&mut cell_bytes, &[]);
    let new_cell = IndexCell {
        child: 0,
        payload_off,
        bytes: cell_bytes,
        payload_len: payload.len(),
        local_len,
        overflow: overflow_head,
    };

    // ---- merge into the leaf in key order ----
    // The parsed leaf cells are not needed afterwards: reuse them.
    let mut all = leaf_cells;
    all.insert(leaf_pos, new_cell);

    let hdr_off = 0usize; // index roots are never page 1
    if index_page_need(all.as_slice(), hdr_off, false) <= usable {
        write_index_page(pager, leaf_page, all.as_slice(), 0, false, false).await?;
        return Ok(());
    }

    // ---- the leaf overflows: balance (v0.15; no quick split
    // for index B-trees — probed: appends balance like any other
    // insert) ----
    if path.is_empty() {
        // Root leaf: partition its entries over two fresh leaves;
        // the root becomes an interior holding the promoted key.
        let (pages, dividers) = partition_index_ins(all.as_slice(), 2, usable, false, 1);
        if pages.len() != 2 || dividers.len() != 1 {
            return Err(Error::Unsupported);
        }
        let (lp, _) = pager.alloc_page().await?;
        let (rp, _) = pager.alloc_page().await?;
        write_index_page(pager, lp, pages.as_slice()[0].as_slice(), 0, false, true).await?;
        write_index_page(pager, rp, pages.as_slice()[1].as_slice(), 0, false, true).await?;
        let mut cells: AVec<IndexCell> = AVec::new();
        cells.push(recell(Some(lp), &dividers.as_slice()[0], 0));
        write_index_page(pager, root, cells.as_slice(), rp, true, false).await?;
        return Ok(());
    }
    let (parent, trigger) = *path.as_slice().last().expect("non-root leaf has a parent");
    let up = ins_balance_index_leaf_window(pager, parent, trigger, leaf_page, all).await?;
    cascade_index_up(pager, root, path.as_slice(), up).await
}

// ----------------------------------------------------------------
// Index traversal: ordered cursor with lower-bound seek (v0.3).
// ----------------------------------------------------------------

/// One component of a seek probe: `Min` sorts below every value
/// (including NULL) — it pads a prefix probe to full key length.
#[cfg(feature = "indexes")]
#[derive(Clone)]
pub enum KeyPart {
    Min,
    Val(OwnedValue),
}

#[cfg(feature = "indexes")]
fn cmp_entry_probe(
    entry: &[OwnedValue],
    probe: &[KeyPart],
    desc: &[KeyOrder],
    enc: crate::record::TextEnc,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for j in 0..entry.len().min(probe.len()) {
        match &probe[j] {
            KeyPart::Min => return Ordering::Greater,
            KeyPart::Val(v) => {
                let o = desc.get(j).copied().unwrap_or_default();
                let c = compare_coll_in(&entry[j].as_value(), &v.as_value(), o.coll, enc);
                let c = if o.desc { c.reverse() } else { c };
                if c != Ordering::Equal {
                    return c;
                }
            }
        }
    }
    Ordering::Equal
}

/// One index page held by a cursor: the page image plus entries
/// decoded on first use, so a seek decodes only the cells its
/// binary search visits and a scan decodes only what it yields.
#[cfg(feature = "indexes")]
struct LazyPage {
    snap: AVec<u8>,
    page: u32,
    interior: bool,
    ncell: usize,
    rightmost: u32,
    slots: AVec<Option<IndexEntry>>,
}

#[cfg(feature = "indexes")]
impl LazyPage {
    fn new<E>(snap: AVec<u8>, page: u32, interior: bool) -> Result<LazyPage, E> {
        let h = head::<E>(snap.as_slice(), page)?;
        let ncell = usize::from(h.ncell);
        let mut slots = AVec::with_capacity(ncell);
        for _ in 0..ncell {
            slots.push(None);
        }
        Ok(LazyPage {
            snap,
            page,
            interior,
            ncell,
            rightmost: h.rightmost,
            slots,
        })
    }

    /// Decode entry `i` if it is not yet.
    async fn fill<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
        nfields: usize,
        i: usize,
    ) -> Result<(), S::Error> {
        if matches!(self.slots.as_slice().get(i), Some(Some(_))) {
            return Ok(());
        }
        let usable = pager.usable() as usize;
        let h = head::<S::Error>(self.snap.as_slice(), self.page)?;
        let ptr = cell_ptr::<S::Error>(self.snap.as_slice(), &h, i, self.page)?;
        let cell = parse_index_cell::<S::Error>(
            self.snap.as_slice(),
            ptr,
            usable,
            self.page,
            self.interior,
        )?;
        // `assemble_payload` directly, not `index_cell_payload`: each
        // nested async call adds its future to the caller's in a debug
        // build, and this path sits under every index seek and scan
        // (the `Config::stack_budget` model, tests/depth_v16.rs).
        let local = copy_bytes(cell.local_bytes());
        let p = assemble_payload(pager, local.as_slice(), cell.payload_len, cell.overflow).await?;
        let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
        let slot = self
            .slots
            .as_mut_slice()
            .get_mut(i)
            .ok_or(Error::CorruptPage { page: self.page })?;
        *slot = Some(e);
        Ok(())
    }

    /// Entry `i`, once `fill` has decoded it. A plain function, so
    /// callers await `fill` directly: one async level fewer under
    /// each seek and scan step.
    fn get<E>(&self, i: usize) -> Result<&IndexEntry, E> {
        match self.slots.as_slice().get(i) {
            Some(Some(e)) => Ok(e),
            _ => Err(Error::CorruptPage { page: self.page }),
        }
    }

    /// Child `i` of an interior page (`ncell` is the rightmost).
    fn child<E>(&self, i: usize) -> Result<u32, E> {
        if i == self.ncell {
            return Ok(self.rightmost);
        }
        let h = head::<E>(self.snap.as_slice(), self.page)?;
        let ptr = cell_ptr::<E>(self.snap.as_slice(), &h, i, self.page)?;
        if ptr + 4 > self.snap.len() {
            return Err(Error::CorruptPage { page: self.page });
        }
        Ok(be_u32(self.snap.as_slice(), ptr))
    }

    /// The pin's per-page binary search (`sqlite3BtreeIndexMoveto`):
    /// probe the middle cell, halve, stop on an equal entry. Returns
    /// the equal entry's index and `true`, or the lower bound the
    /// search ended at (`lwr`: on an ordered page, the first entry
    /// greater than `probe`) and `false`.
    /// Only the entries the search compares are decoded.
    async fn search<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
        nfields: usize,
        probe: &[KeyPart],
        desc: &[KeyOrder],
    ) -> Result<(usize, bool), S::Error> {
        let (mut lo, mut hi) = (0usize, self.ncell);
        while lo < hi {
            let mid = lo + (hi - 1 - lo) / 2;
            let enc = pager.text_enc();
            self.fill(pager, nfields, mid).await?;
            let e = self.get::<S::Error>(mid)?;
            match cmp_entry_probe(e.values.as_slice(), probe, desc, enc) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Ok((mid, true)),
            }
        }
        Ok((lo, false))
    }
}

#[cfg(feature = "indexes")]
enum IdxFrame {
    Unresolved {
        page: u32,
    },
    Leaf {
        page: LazyPage,
        next: usize,
    },
    Interior {
        page: LazyPage,
        /// Next action: even seq = descend child seq/2 (the
        /// rightmost child is at seq == 2*ncell), odd seq
        /// = yield entry (seq-1)/2; past the end = frame done.
        seq: usize,
    },
}

/// An in-order cursor over an index B-tree (interior keys are
/// real entries and are yielded between their subtrees). State
/// lives in the cursor, never in a future (§5.1.2).
#[cfg(feature = "indexes")]
pub struct IndexCursor {
    stack: AVec<IdxFrame>,
    nfields: usize,
    /// The tree's root (v0.16, `without_rowid`).
    #[cfg(feature = "without_rowid")]
    root: u32,
    /// How entries reach the caller (v0.16, `without_rowid`): a
    /// cursor over a WITHOUT ROWID table's tree yields
    /// `key ... handle` entries, as an index of a rowid table does.
    #[cfg(feature = "without_rowid")]
    view: crate::wr::WrView,
}

#[cfg(feature = "indexes")]
impl IndexCursor {
    /// A cursor over the whole index, in key order.
    pub fn new(root: u32, desc: &[KeyOrder]) -> IndexCursor {
        let mut stack = AVec::new();
        stack.push(IdxFrame::Unresolved { page: root });
        IndexCursor {
            stack,
            nfields: desc.len() + 1,
            #[cfg(feature = "without_rowid")]
            root,
            #[cfg(feature = "without_rowid")]
            view: crate::wr::WrView::Unknown,
        }
    }

    /// A cursor over the whole tree whose entries decode as
    /// `nfields` says (`entry_fields`), never translated.
    pub(crate) fn with_fields(root: u32, desc: &[KeyOrder], nfields: usize) -> IndexCursor {
        let mut c = IndexCursor::new(root, desc);
        c.nfields = nfields;
        #[cfg(feature = "without_rowid")]
        {
            c.view = crate::wr::WrView::Plain;
        }
        c
    }

    /// A cursor positioned at the first entry >= `probe` (a full
    /// key-length probe; pad prefix probes with `KeyPart::Min`).
    pub async fn seek<S: Storage>(
        pager: &mut Pager<S>,
        root: u32,
        desc: &[KeyOrder],
        probe: &[KeyPart],
    ) -> Result<IndexCursor, S::Error> {
        // A WITHOUT ROWID table's tree (v0.16): the probe's rowid
        // part becomes the tree's key-column suffix, and the cursor
        // translates its entries.
        #[cfg(feature = "without_rowid")]
        {
            let view = pager.wr.view_of(root);
            if let crate::wr::WrView::Pk(_) | crate::wr::WrView::Sec(_) = view {
                let (p2, orders) = pager.wr.probe_for(view, probe);
                let (mut cur, _) =
                    Self::seek_found(pager, root, orders.as_slice(), p2.as_slice()).await?;
                cur.view = view;
                return Ok(cur);
            }
        }
        Ok(Self::seek_found(pager, root, desc, probe).await?.0)
    }

    /// `seek`, plus whether the descent met an entry equal to
    /// `probe` — the pin's `sqlite3BtreeIndexMoveto` result (v0.16:
    /// each page is searched by the pin's binary search,
    /// `LazyPage::search`, so a page whose keys are out of order, in a
    /// damaged file, is searched as the pin searches it). When
    /// found, the cursor yields that entry next.
    pub async fn seek_found<S: Storage>(
        pager: &mut Pager<S>,
        root: u32,
        desc: &[KeyOrder],
        probe: &[KeyPart],
    ) -> Result<(IndexCursor, bool), S::Error> {
        let nfields = entry_fields(pager, root, desc);
        let mut cur = IndexCursor::with_fields(root, desc, nfields);
        cur.stack.clear();
        let mut page = root;
        loop {
            let idx = pager.load(page).await?;
            let snap = copy_bytes(pager.frame_data(idx));
            let h = head::<S::Error>(snap.as_slice(), page)?;
            if h.is_leaf() {
                if h.kind != KIND_LEAF_INDEX {
                    return Err(Error::Unsupported);
                }
                let mut lp = LazyPage::new::<S::Error>(snap, page, false)?;
                let (pos, found) = lp.search(pager, nfields, probe, desc).await?;
                cur.stack.push(IdxFrame::Leaf {
                    page: lp,
                    next: pos,
                });
                return Ok((cur, found));
            }
            if h.kind != KIND_INTERIOR_INDEX {
                return Err(Error::Unsupported);
            }
            let mut lp = LazyPage::new::<S::Error>(snap, page, true)?;
            // First divider >= probe (Equal only for a full-key
            // probe: prefix probes pad with Min and can never be
            // Equal to a real entry).
            let (i, equal) = lp.search(pager, nfields, probe, desc).await?;
            if equal {
                // The divider itself is the lower bound; yield it
                // next, then continue with its right subtree.
                cur.stack.push(IdxFrame::Interior {
                    page: lp,
                    seq: 2 * i + 1,
                });
                return Ok((cur, true));
            }
            // Descend into child i; when it is exhausted this
            // frame yields divider i next (or is done, when i is
            // the rightmost child).
            let next_page = lp.child::<S::Error>(i)?;
            cur.stack.push(IdxFrame::Interior {
                page: lp,
                seq: 2 * i + 1,
            });
            page = next_page;
        }
    }

    async fn resolve<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
        page: u32,
    ) -> Result<IdxFrame, S::Error> {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_INDEX {
                return Err(Error::Unsupported);
            }
            return Ok(IdxFrame::Leaf {
                page: LazyPage::new::<S::Error>(snap, page, false)?,
                next: 0,
            });
        }
        if h.kind != KIND_INTERIOR_INDEX {
            return Err(Error::Unsupported);
        }
        Ok(IdxFrame::Interior {
            page: LazyPage::new::<S::Error>(snap, page, true)?,
            seq: 0,
        })
    }

    /// An entry as the caller sees it (v0.16: a WITHOUT ROWID
    /// tree's entries translated, `crate::wr`).
    #[inline(always)]
    fn yield_entry<S: Storage>(
        &self,
        pager: &mut Pager<S>,
        e: IndexEntry,
    ) -> Result<Option<IndexEntry>, S::Error> {
        #[cfg(feature = "without_rowid")]
        if let crate::wr::WrView::Pk(_) | crate::wr::WrView::Sec(_) = self.view {
            return Ok(Some(pager.wr.translate::<S::Error>(self.view, e)?));
        }
        let _ = pager;
        Ok(Some(e))
    }

    /// The next entry in key order, if any.
    pub async fn next_entry<S: Storage>(
        &mut self,
        pager: &mut Pager<S>,
    ) -> Result<Option<IndexEntry>, S::Error> {
        #[cfg(feature = "without_rowid")]
        {
            use crate::wr::WrView;
            if self.view == WrView::Unknown {
                self.view = pager.wr.view_of(self.root);
                if self.view != WrView::Plain {
                    self.nfields = 0;
                }
            }
        }
        loop {
            match self.stack.as_slice().last() {
                None => return Ok(None),
                Some(IdxFrame::Unresolved { page }) => {
                    let page = *page;
                    self.stack.pop();
                    let frame = self.resolve(pager, page).await?;
                    self.stack.push(frame);
                }
                Some(IdxFrame::Leaf { .. }) => {
                    let frame = self.stack.pop().expect("leaf frame");
                    if let IdxFrame::Leaf { mut page, next } = frame {
                        if next >= page.ncell {
                            continue;
                        }
                        page.fill(pager, self.nfields, next).await?;
                        let e = page.get::<S::Error>(next)?.clone();
                        self.stack.push(IdxFrame::Leaf {
                            page,
                            next: next + 1,
                        });
                        return self.yield_entry(pager, e);
                    }
                    unreachable!()
                }
                Some(IdxFrame::Interior { .. }) => {
                    let frame = self.stack.pop().expect("interior frame");
                    if let IdxFrame::Interior { mut page, seq } = frame {
                        if seq > 2 * page.ncell {
                            continue; // frame exhausted
                        }
                        if seq % 2 == 1 {
                            let i = (seq - 1) / 2;
                            page.fill(pager, self.nfields, i).await?;
                            let e = page.get::<S::Error>(i)?.clone();
                            self.stack.push(IdxFrame::Interior { page, seq: seq + 1 });
                            return self.yield_entry(pager, e);
                        }
                        let child = page.child::<S::Error>(seq / 2)?;
                        self.stack.push(IdxFrame::Interior { page, seq: seq + 1 });
                        self.stack.push(IdxFrame::Unresolved { page: child });
                    } else {
                        unreachable!()
                    }
                }
            }
        }
    }
}

/// Whether `entry`'s first `prefix.len()` key fields equal
/// `prefix` (direction-independent field equality, under each
/// key column's collation — v0.16).
#[cfg(feature = "indexes")]
fn prefix_matches(entry: &IndexEntry, prefix: &[OwnedValue], desc: &[KeyOrder]) -> bool {
    for (j, p) in prefix.iter().enumerate() {
        let Some(v) = entry.values.as_slice().get(j) else {
            return false;
        };
        let coll = desc.get(j).map(|o| o.coll).unwrap_or_default();
        if compare_coll(&v.as_value(), &p.as_value(), coll) != core::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}

/// All rowids whose index key starts with `prefix` (the planner's
/// secondary-index lookup; seek-positioned, then a bounded scan).
#[cfg(feature = "indexes")]
pub async fn index_prefix_rowids<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    desc: &[KeyOrder],
    prefix: &[OwnedValue],
) -> Result<AVec<i64>, S::Error> {
    #[cfg(feature = "without_rowid")]
    {
        let view = pager.wr.view_of(root);
        if let crate::wr::WrView::Pk(_) | crate::wr::WrView::Sec(_) = view {
            return crate::wr::prefix_handles(pager, root, view, prefix).await;
        }
    }
    let mut probe: AVec<KeyPart> = AVec::new();
    for v in prefix {
        probe.push(KeyPart::Val(v.clone()));
    }
    while probe.len() < desc.len() + 1 {
        probe.push(KeyPart::Min);
    }
    let mut cur = IndexCursor::seek(pager, root, desc, probe.as_slice()).await?;
    let mut out = AVec::new();
    while let Some(e) = cur.next_entry(pager).await? {
        if !prefix_matches(&e, prefix, desc) {
            break;
        }
        out.push(e.rowid()?);
    }
    Ok(out)
}

/// `PRAGMA integrity_check`'s index probe for one row (v0.16,
/// `v16`): whether the entry `key` + `rowid` is found by the pin's
/// seek (`sqlite3BtreeIndexMoveto`: each page binary-searched, see
/// `LazyPage::search`), and — for a `unique` index — whether the entry
/// after it does not sort above `key` (the pin's `OP_Next` +
/// `OP_IdxGT`: "non-unique entry"). Only the cells the search
/// visits are decoded, as on the pin, so damage elsewhere on a
/// page does not stop the check.
#[cfg(all(feature = "indexes", feature = "v16"))]
pub async fn index_check_entry<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    desc: &[KeyOrder],
    key: &[OwnedValue],
    rowid: i64,
    unique: bool,
) -> Result<(bool, bool), S::Error> {
    let mut full: AVec<OwnedValue> = AVec::new();
    // A WITHOUT ROWID table's secondary index (v0.16): the entry
    // ends with the row's key columns, not a rowid.
    #[cfg(feature = "without_rowid")]
    let wr_orders;
    #[cfg(feature = "without_rowid")]
    let desc = match pager.wr.view_of(root) {
        crate::wr::WrView::Sec(i) => {
            let (f, o) = pager.wr.sec_full::<S::Error>(i, key, rowid)?;
            full = f;
            wr_orders = o;
            wr_orders.as_slice()
        }
        _ => {
            full.extend_from_slice(key);
            full.push(OwnedValue::Integer(rowid));
            desc
        }
    };
    #[cfg(not(feature = "without_rowid"))]
    {
        full.extend_from_slice(key);
        full.push(OwnedValue::Integer(rowid));
    }
    // The in-order successor so far: the divider after the child
    // the descent last went left of.
    let mut succ: Option<(u32, usize)> = None;
    let mut page = root;
    let mut budget = pager.page_count();
    loop {
        budget = budget.checked_sub(1).ok_or(Error::CorruptPage { page })?;
        let snap = page_snap(pager, page).await?;
        let h = head::<S::Error>(snap.as_slice(), page)?;
        let interior = match h.kind {
            KIND_INTERIOR_INDEX => true,
            KIND_LEAF_INDEX => false,
            _ => return Err(Error::CorruptPage { page }),
        };
        let n = usize::from(h.ncell);
        let (mut lo, mut hi) = (0usize, n);
        let mut hit = None;
        while lo < hi {
            let mid = lo + (hi - 1 - lo) / 2;
            let rec = index_payload_at(pager, snap.as_slice(), &h, mid, page).await?;
            match pin_rec_cmp(rec.as_slice(), full.as_slice(), desc, pager.text_enc()) {
                Some(c) if c < 0 => lo = mid + 1,
                Some(c) if c > 0 => hi = mid,
                Some(_) => {
                    hit = Some(mid);
                    break;
                }
                // Corrupt and so "equal": the pin's seek fails.
                None => return Err(Error::CorruptRecord),
            }
        }
        if let Some(i) = hit {
            if !unique {
                return Ok((true, false));
            }
            let next = if interior {
                // The leftmost entry of the subtree right of it.
                let mut p = child_at(pager, snap.as_slice(), &h, i + 1, page).await?;
                loop {
                    budget = budget
                        .checked_sub(1)
                        .ok_or(Error::CorruptPage { page: p })?;
                    let d = page_snap(pager, p).await?;
                    let hh = head::<S::Error>(d.as_slice(), p)?;
                    if hh.kind == KIND_LEAF_INDEX {
                        break if hh.ncell == 0 {
                            None
                        } else {
                            Some(index_payload_at(pager, d.as_slice(), &hh, 0, p).await?)
                        };
                    }
                    if hh.kind != KIND_INTERIOR_INDEX {
                        return Err(Error::CorruptPage { page: p });
                    }
                    p = child_at(pager, d.as_slice(), &hh, 0, p).await?;
                }
            } else if i + 1 < n {
                Some(index_payload_at(pager, snap.as_slice(), &h, i + 1, page).await?)
            } else if let Some((sp, si)) = succ {
                let d = page_snap(pager, sp).await?;
                let hh = head::<S::Error>(d.as_slice(), sp)?;
                Some(index_payload_at(pager, d.as_slice(), &hh, si, sp).await?)
            } else {
                None
            };
            // `OP_IdxGT` over the key columns: a key-prefix tie (the
            // pin's `default_rc` -1) or less is a duplicate.
            let dup = match next {
                Some(rec) => {
                    match pin_rec_cmp_from(rec.as_slice(), key, desc, false, pager.text_enc()) {
                        Some(c) => c <= 0,
                        None => return Err(Error::CorruptRecord),
                    }
                }
                None => false,
            };
            return Ok((true, dup));
        }
        if !interior {
            return Ok((false, false));
        }
        if lo < n {
            succ = Some((page, lo));
        }
        page = child_at(pager, snap.as_slice(), &h, lo, page).await?;
    }
}

/// The pin's index-record comparison (`xRecordCompare`, v0.16,
/// `v16`): index cell payload `rec` against the full key `probe`
/// (key values + rowid) under `desc`, field by field from the raw
/// record — a field's body is read only when its type needs it, so
/// a damaged record compares as the pin's does (its fast paths for
/// an integer or BINARY text first field included). `None` is the
/// pin's corruption verdict (`errCode`), which it reports only if
/// the comparison then reads as equal.
#[cfg(all(feature = "indexes", feature = "v16"))]
fn pin_rec_cmp(
    rec: &[u8],
    probe: &[OwnedValue],
    desc: &[KeyOrder],
    enc: crate::record::TextEnc,
) -> Option<i32> {
    let b = |i: usize| rec.get(i).copied().unwrap_or(0);
    let o0 = desc.first().copied().unwrap_or_default();
    let (r1, r2) = if o0.desc { (1, -1) } else { (-1, 1) };
    if probe.len() <= 13 {
        match probe.first() {
            Some(OwnedValue::Integer(v)) => {
                let st = u32::from(b(1));
                let at = usize::from(b(0) & 0x3f);
                if matches!(st, 1..=6 | 8 | 9) {
                    let lhs = rec_int(rec, at, st);
                    return Some(if *v > lhs {
                        r1
                    } else if *v < lhs {
                        r2
                    } else if probe.len() > 1 {
                        return pin_rec_cmp_from(rec, probe, desc, true, enc);
                    } else {
                        0
                    });
                }
            }
            Some(OwnedValue::Text(z))
                if o0.coll == crate::record::Coll::Binary
                    && enc == crate::record::TextEnc::Utf8 =>
            {
                let (st, _) = rec_varint32(rec, 1);
                if st < 12 {
                    return Some(r1);
                }
                if st & 1 == 0 {
                    return Some(r2);
                }
                let hdr = usize::from(b(0));
                let n = ((st - 12) / 2) as usize;
                if hdr + n > rec.len() {
                    return None;
                }
                let body = rec.get(hdr..hdr + n).unwrap_or_default();
                let k = n.min(z.len());
                return Some(match body.get(..k).cmp(&z.as_slice().get(..k)) {
                    core::cmp::Ordering::Greater => r2,
                    core::cmp::Ordering::Less => r1,
                    core::cmp::Ordering::Equal => match n.cmp(&z.len()) {
                        core::cmp::Ordering::Equal if probe.len() > 1 => {
                            return pin_rec_cmp_from(rec, probe, desc, true, enc);
                        }
                        core::cmp::Ordering::Equal => 0,
                        core::cmp::Ordering::Greater => r2,
                        core::cmp::Ordering::Less => r1,
                    },
                });
            }
            _ => {}
        }
    }
    pin_rec_cmp_from(rec, probe, desc, false, enc)
}

/// `sqlite3VdbeRecordCompareWithSkip` (see `pin_rec_cmp`); `skip`
/// starts at the second field, the first known equal.
#[cfg(all(feature = "indexes", feature = "v16"))]
fn pin_rec_cmp_from(
    rec: &[u8],
    probe: &[OwnedValue],
    desc: &[KeyOrder],
    skip: bool,
    enc: crate::record::TextEnc,
) -> Option<i32> {
    use crate::record::{Coll, Value, compare_coll, compare_coll_in};
    let b = |i: usize| rec.get(i).copied().unwrap_or(0);
    let nkey = rec.len() as u64;
    let (mut idx1, hdr, mut d1, mut i) = if skip {
        let (s1, n) = rec_varint32(rec, 1);
        let hdr = u64::from(b(0));
        (1 + n, hdr, hdr + u64::from(rec_serial_len(s1)), 1usize)
    } else {
        let (hdr, n) = rec_varint32(rec, 0);
        (n, u64::from(hdr), u64::from(hdr), 0usize)
    };
    if d1 > nkey {
        return None;
    }
    let ord = |o: core::cmp::Ordering| o as i32;
    loop {
        let rhs = probe.get(i)?;
        let at = d1 as usize;
        let mut st = u32::from(b(idx1));
        let rc = match rhs {
            OwnedValue::Integer(_) | OwnedValue::Real(_) => {
                if st >= 10 {
                    if st == 10 { -1 } else { 1 }
                } else if st == 0 {
                    -1
                } else {
                    let lhs = if st == 7 {
                        Value::Real(f64::from_bits(rec_int(rec, at, 6) as u64))
                    } else {
                        Value::Integer(rec_int(rec, at, st))
                    };
                    ord(compare_coll(&lhs, &rhs.as_value(), Coll::Binary))
                }
            }
            OwnedValue::Text(z) => {
                st = rec_varint32(rec, idx1).0;
                if st < 12 {
                    -1
                } else if st & 1 == 0 {
                    1
                } else {
                    let n = u64::from((st - 12) / 2);
                    if d1 + n > nkey {
                        return None;
                    }
                    let body = rec.get(at..at + n as usize).unwrap_or_default();
                    let coll = desc.get(i).map(|o| o.coll).unwrap_or_default();
                    ord(compare_coll_in(
                        &Value::Text(body),
                        &Value::Text(z.as_slice()),
                        coll,
                        enc,
                    ))
                }
            }
            OwnedValue::Blob(z) => {
                st = rec_varint32(rec, idx1).0;
                if st < 12 || st & 1 == 1 {
                    -1
                } else {
                    let n = u64::from((st - 12) / 2);
                    if d1 + n > nkey {
                        return None;
                    }
                    let body = rec.get(at..at + n as usize).unwrap_or_default();
                    ord(body.cmp(z.as_slice()))
                }
            }
            OwnedValue::Null => {
                let nan = st == 7 && f64::from_bits(rec_int(rec, at, 6) as u64).is_nan();
                if st == 0 || st == 10 || nan { 0 } else { 1 }
            }
        };
        if rc != 0 {
            let flip = desc.get(i).is_some_and(|o| o.desc);
            return Some(if flip { -rc } else { rc });
        }
        i += 1;
        if i == probe.len() {
            return Some(0);
        }
        d1 += u64::from(rec_serial_len(st));
        if d1 > nkey {
            return Some(0);
        }
        idx1 += if st < 0x80 { 1 } else { rec_varint_len(st) };
        if idx1 as u64 >= hdr {
            return None;
        }
    }
}

/// A big-endian integer of serial type `st` (1..=6, 8, 9) at `at`;
/// bytes past the record read as 0 (the pin reads into its page
/// buffer's slack).
#[cfg(all(feature = "indexes", feature = "v16"))]
fn rec_int(rec: &[u8], at: usize, st: u32) -> i64 {
    let n = match st {
        1..=4 => st as usize,
        5 => 6,
        6 => 8,
        8 => return 0,
        9 => return 1,
        _ => return 0,
    };
    let mut v: i64 = if rec.get(at).is_some_and(|c| *c & 0x80 != 0) {
        -1
    } else {
        0
    };
    for k in 0..n {
        v = (v << 8) | i64::from(rec.get(at + k).copied().unwrap_or(0));
    }
    v
}

/// `getVarint32` at `at` (missing bytes read as 0).
#[cfg(all(feature = "indexes", feature = "v16"))]
fn rec_varint32(rec: &[u8], at: usize) -> (u32, usize) {
    match rec.get(at..).map(decode_varint::<()>) {
        Some(Ok((v, n))) => (u32::try_from(v).unwrap_or(u32::MAX), n),
        _ => (0, 1),
    }
}

#[cfg(all(feature = "indexes", feature = "v16"))]
fn rec_serial_len(t: u32) -> u32 {
    match t {
        0..=4 => t,
        5 => 6,
        6 | 7 => 8,
        8..=11 => 0,
        _ => (t - 12) / 2,
    }
}

#[cfg(all(feature = "indexes", feature = "v16"))]
fn rec_varint_len(v: u32) -> usize {
    match v {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0xfff_ffff => 4,
        _ => 5,
    }
}

/// A copy of page `page`'s image.
#[cfg(all(feature = "indexes", feature = "v16"))]
async fn page_snap<S: Storage>(pager: &mut Pager<S>, page: u32) -> Result<AVec<u8>, S::Error> {
    let idx = pager.load(page).await?;
    Ok(copy_bytes(pager.frame_data(idx)))
}

/// Index page cell `i`'s full payload.
#[cfg(all(feature = "indexes", feature = "v16"))]
async fn index_payload_at<S: Storage>(
    pager: &mut Pager<S>,
    snap: &[u8],
    h: &PageHead,
    i: usize,
    page: u32,
) -> Result<AVec<u8>, S::Error> {
    let usable = pager.usable() as usize;
    let at = cell_ptr::<S::Error>(snap, h, i, page)?;
    let c = parse_index_cell::<S::Error>(snap, at, usable, page, h.kind == KIND_INTERIOR_INDEX)?;
    index_cell_payload(pager, &c).await
}

/// Interior index page child `i` (`ncell` is the rightmost).
#[cfg(all(feature = "indexes", feature = "v16"))]
async fn child_at<S: Storage>(
    pager: &mut Pager<S>,
    snap: &[u8],
    h: &PageHead,
    i: usize,
    page: u32,
) -> Result<u32, S::Error> {
    if i >= usize::from(h.ncell) {
        return Ok(h.rightmost);
    }
    let at = cell_ptr::<S::Error>(snap, h, i, page)?;
    let _ = pager;
    snap.get(at..at + 4)
        .map(|_| be_u32(snap, at))
        .ok_or(Error::CorruptPage { page })
}

/// The first rowid whose index key starts with `prefix`, if any
/// (the UNIQUE pre-check).
#[cfg(feature = "indexes")]
pub async fn index_first_with_prefix<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    desc: &[KeyOrder],
    prefix: &[OwnedValue],
) -> Result<Option<i64>, S::Error> {
    let rowids = index_prefix_rowids(pager, root, desc, prefix).await?;
    Ok(rowids.as_slice().first().copied())
}

/// Every page of the index rooted at `root`: B-tree pages plus
/// all overflow chains (DROP INDEX frees exactly this set).
#[cfg(any(feature = "indexes", feature = "v05"))]
pub async fn collect_index_pages<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
) -> Result<AVec<u32>, S::Error> {
    let usable = pager.usable() as usize;
    let mut out: AVec<u32> = AVec::new();
    let mut stack: AVec<u32> = AVec::new();
    stack.push(root);
    while let Some(page) = stack.pop() {
        out.push(page);
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        let cells: AVec<IndexCell> = match h.kind {
            KIND_LEAF_INDEX => index_leaf_cells::<S::Error>(snap.as_slice(), page, usable)?,
            KIND_INTERIOR_INDEX => {
                let (cells, rightmost) =
                    index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
                for c in cells.iter() {
                    stack.push(c.child);
                }
                if rightmost != 0 {
                    stack.push(rightmost);
                }
                cells
            }
            _ => return Err(Error::CorruptPage { page }),
        };
        for c in cells.iter() {
            let mut next = c.overflow;
            let mut hops = 0u32;
            while next != 0 {
                hops += 1;
                if hops > pager.page_count() + 1 {
                    return Err(Error::OverflowChainCycle);
                }
                out.push(next);
                let oi = pager.load(next).await?;
                next = be_u32(pager.frame_data(oi), 0);
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------
// Table page collection (v0.5) — DROP TABLE frees exactly the set
// collect_table_pages returns: every B-tree page of the table
// plus every overflow chain hanging off its cells.
// ----------------------------------------------------------------

/// Every page of the table rooted at `root`: B-tree pages plus
/// all overflow chains. Ungated like `btree_delete`: DROP TABLE
/// (v05) is the headline caller, but the delete-rebuild path
/// needs it in every configuration that can delete rows.
pub async fn collect_table_pages<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
) -> Result<AVec<u32>, S::Error> {
    // A WITHOUT ROWID table's tree is an index B-tree (v0.16).
    #[cfg(feature = "without_rowid")]
    if pager.wr.table_of(root).is_some() {
        return collect_index_pages(pager, root).await;
    }
    let usable = pager.usable() as usize;
    let mut out: AVec<u32> = AVec::new();
    let mut stack: AVec<u32> = AVec::new();
    stack.push(root);
    while let Some(page) = stack.pop() {
        out.push(page);
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        match h.kind {
            KIND_LEAF_TABLE => {
                let cells = leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
                for c in cells.iter() {
                    let mut next = c.overflow;
                    let mut hops = 0u32;
                    while next != 0 {
                        hops += 1;
                        if hops > pager.page_count() + 1 {
                            return Err(Error::OverflowChainCycle);
                        }
                        out.push(next);
                        let oi = pager.load(next).await?;
                        next = be_u32(pager.frame_data(oi), 0);
                    }
                }
            }
            KIND_INTERIOR_TABLE => {
                let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), page)?;
                for (child, _) in pairs.iter() {
                    stack.push(*child);
                }
                if rightmost != 0 {
                    stack.push(rightmost);
                }
            }
            _ => return Err(Error::CorruptPage { page }),
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------
// Index B-tree deletion (v0.5, feature `indexes`).
//
// Index B-trees are true B-trees: the key being deleted may live
// in an interior page. The classic remedy is applied: replace an
// interior key with its predecessor (the greatest entry of its
// left subtree), then delete that predecessor below — each round
// strictly descends, so the loop terminates at a leaf. No merge
// or redistribution: pages left underfull stay linked, which the
// format allows and `integrity_check` accepts (the same posture
// as table deletion). The one structural corner — a replacement
// key too large for its interior page — falls back to rebuilding
// the index without the entry (correct, and vanishingly rare:
// it needs an oversized predecessor and a full interior page).
// ----------------------------------------------------------------

/// Free one overflow chain, following next-pointers to 0.
#[cfg(feature = "indexes")]
async fn free_overflow_chain<S: Storage>(pager: &mut Pager<S>, first: u32) -> Result<(), S::Error> {
    let mut next = first;
    let mut hops = 0u32;
    while next != 0 {
        hops += 1;
        if hops > pager.page_count() + 1 {
            return Err(Error::OverflowChainCycle);
        }
        let idx = pager.load(next).await?;
        let following = be_u32(pager.frame_data(idx), 0);
        pager.free_page(next).await?;
        next = following;
    }
    Ok(())
}

/// Canonically re-encode a decoded index entry's payload (the
/// record of its values). Byte-different from a foreign file's
/// original encoding is fine: every comparison decodes first.
#[cfg(feature = "indexes")]
fn encode_entry_payload(e: &IndexEntry) -> AVec<u8> {
    let mut vals: AVec<Value<'_>> = AVec::new();
    for v in e.values.iter() {
        vals.push(v.as_value());
    }
    let mut rec = AVec::new();
    encode_record(vals.as_slice(), &mut rec);
    rec
}

// ----------------------------------------------------------------
// Index B-tree delete balancing (v0.14): the same window
// redistribution / merge / cascade / root-collapse machinery as
// the table path, over the index entry sequence. An index
// window's combined sequence interleaves the window pages' own
// entries with the parent dividers between them (each divider
// is an ordinary entry of the tree); re-splitting lifts one
// entry per boundary back into the parent as the new divider.
// Interior pages flatten the same way, each entry carrying its
// left child pointer, plus one trailing rightmost pointer.
// ----------------------------------------------------------------

/// Outcome of one index window balance.
#[cfg(feature = "indexes")]
enum IndexBalance {
    /// The window was redistributed (and possibly merged);
    /// `children_removed` counts how many children the parent
    /// lost.
    Done { children_removed: usize },
    /// Degenerate window (a whole subtree level emptied under a
    /// non-root parent). No window page was written; the caller
    /// falls back to a rebuild.
    Rebuild,
    /// The window's entries need more pages than it has, or the
    /// parent no longer fits its new dividers. No page was
    /// written; the caller runs the insert-side balance, which
    /// allocates pages and cascades a parent overflow upward.
    Grow,
}

/// The cells, rightmost pointer and ordered children of an
/// interior index page.
#[cfg(feature = "indexes")]
async fn index_children<S: Storage>(
    pager: &mut Pager<S>,
    page: u32,
) -> Result<(AVec<IndexCell>, u32, AVec<u32>), S::Error> {
    let usable = pager.usable() as usize;
    let idx = pager.load(page).await?;
    let snap = copy_bytes(pager.frame_data(idx));
    let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
    let mut children: AVec<u32> = AVec::new();
    for c in cells.iter() {
        children.push(c.child);
    }
    children.push(rightmost);
    Ok((cells, rightmost, children))
}

/// Patch an interior cell's child pointer (the cell struct and
/// the first four bytes of its image).
#[cfg(feature = "indexes")]
fn patch_child(cell: &mut IndexCell, child: u32) {
    cell.child = child;
    let nb = child.to_be_bytes();
    cell.bytes.as_mut_slice()[..4].copy_from_slice(&nb);
}

/// Rebuild a parent's (cells, rightmost) after its children
/// `start..end` were replaced by `new_pages` with `dividers`
/// (entries in `divider_prefix`-byte-prefixed form; divider `i`
/// separates new page `i` from new page `i + 1` and is stored
/// with child = new page `i`). An empty `new_pages` drops the
/// window's children and their preceding dividers wholesale.
#[cfg(feature = "indexes")]
fn index_parent_after(
    cells: &[IndexCell],
    children: &[u32],
    start: usize,
    end: usize,
    new_pages: &[u32],
    dividers: &[IndexCell],
    divider_prefix: usize,
) -> (AVec<IndexCell>, u32) {
    let mut nc: AVec<IndexCell> = AVec::new();
    let mut nchildren: AVec<u32> = AVec::new();
    for j in 0..start {
        nchildren.push(children[j]);
        nc.push(cells[j].clone());
    }
    for (i, p) in new_pages.iter().enumerate() {
        nchildren.push(*p);
        if i + 1 < new_pages.len() {
            nc.push(recell(Some(*p), &dividers[i], divider_prefix));
        }
    }
    if !new_pages.is_empty() && end < children.len() {
        // The divider that followed the old window now follows
        // the last new page; only its child pointer changes.
        let mut sep = cells[end - 1].clone();
        patch_child(&mut sep, *new_pages.last().expect("non-empty"));
        nc.push(sep);
    }
    for j in end..children.len() {
        nchildren.push(children[j]);
        if j < cells.len() {
            nc.push(cells[j].clone());
        }
    }
    let rightmost = nchildren.as_slice()[nchildren.len() - 1];
    (nc, rightmost)
}

/// Partition a combined window sequence into page groups plus
/// one lifted divider per boundary (the last item of each group
/// but the last). Tries the pin's quota distribution first and
/// verifies every group fits its page; falls back to greedy
/// maximal packing, which always fits when any partition does.
#[cfg(feature = "indexes")]
fn partition_index(
    items: &[IndexCell],
    n_new: usize,
    usable: usize,
    interior: bool,
) -> (AVec<AVec<IndexCell>>, AVec<IndexCell>) {
    let weights: AVec<usize> = {
        let mut w = AVec::new();
        for c in items.iter() {
            w.push(c.bytes.len() + 2);
        }
        w
    };
    let phantom = usable - if interior { 12 } else { 8 };
    let groups = distribute(items, weights.as_slice(), n_new, phantom);
    // Lift dividers and verify capacity.
    let mut pages: AVec<AVec<IndexCell>> = AVec::new();
    let mut dividers: AVec<IndexCell> = AVec::new();
    let mut ok = true;
    for (i, g) in groups.iter().enumerate() {
        let mut g = g.clone();
        if i + 1 < groups.len() {
            match g.pop() {
                Some(d) => dividers.push(d),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if g.is_empty() || index_page_need(g.as_slice(), 0, interior) > usable {
            ok = false;
            break;
        }
        pages.push(g);
    }
    if ok {
        return (pages, dividers);
    }
    // Greedy maximal packing: fill each page until the next
    // item no longer fits; the page's last item lifts as the
    // divider and the item starts the next page. A one-item
    // page has no item to spare, so there the incoming item
    // lifts as the divider instead.
    let mut pages: AVec<AVec<IndexCell>> = AVec::new();
    let mut dividers: AVec<IndexCell> = AVec::new();
    let mut cur: AVec<IndexCell> = AVec::new();
    for item in items.iter() {
        if cur.is_empty() {
            cur.push(item.clone());
            continue;
        }
        let mut trial = cur.clone();
        trial.push(item.clone());
        if index_page_need(trial.as_slice(), 0, interior) <= usable {
            cur = trial;
        } else if cur.len() >= 2 {
            let d = cur.pop().expect("cur has a divider candidate");
            dividers.push(d);
            pages.push(cur);
            cur = AVec::new();
            cur.push(item.clone());
        } else {
            dividers.push(item.clone());
            pages.push(cur);
            cur = AVec::new();
        }
    }
    if !cur.is_empty() {
        pages.push(cur);
    }
    (pages, dividers)
}

/// Minimum page count for a combined sequence, by greedy
/// maximal packing with divider lifting (mirrors
/// `partition_index`'s fallback arithmetic).
#[cfg(feature = "indexes")]
fn index_min_pages(items: &[IndexCell], usable: usize, interior: bool) -> usize {
    let mut pages = 1usize;
    let mut cur_need = if interior { 12 } else { 8 };
    for item in items.iter() {
        let add = item.bytes.len() + 2;
        if cur_need + add > usable && cur_need > if interior { 12 } else { 8 } {
            pages += 1;
            cur_need = if interior { 12 } else { 8 } + add;
        } else {
            cur_need += add;
        }
    }
    pages
}

/// Balance the window of `parent`'s children around child
/// index `trigger`. `preset` supplies the trigger leaf's kept
/// cells when the caller holds an un-written delete (the leaf
/// on disk still contains the removed entry).
#[cfg(feature = "indexes")]
async fn balance_index_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    trigger: usize,
    preset: Option<(u32, AVec<IndexCell>)>,
) -> Result<IndexBalance, S::Error> {
    let (cells, _rightmost, children) = index_children(pager, parent).await?;
    let (start, end) = window_range(children.len(), trigger);
    let window_pages: AVec<u32> = {
        let mut w = AVec::new();
        for j in start..end {
            w.push(children[j]);
        }
        w
    };
    // Dispatch on the first window child's kind.
    let idx = pager.load(window_pages.as_slice()[0]).await?;
    let snap = copy_bytes(pager.frame_data(idx));
    let h = head::<S::Error>(snap.as_slice(), window_pages.as_slice()[0])?;
    match h.kind {
        KIND_LEAF_INDEX => {
            balance_index_leaf_window(
                pager,
                root,
                parent,
                cells.as_slice(),
                children.as_slice(),
                start,
                end,
                window_pages.as_slice(),
                preset,
            )
            .await
        }
        KIND_INTERIOR_INDEX => {
            balance_index_interior_window(
                pager,
                root,
                parent,
                cells.as_slice(),
                children.as_slice(),
                start,
                end,
                window_pages.as_slice(),
            )
            .await
        }
        _ => Err(Error::Unsupported),
    }
}

#[cfg(feature = "indexes")]
#[allow(clippy::too_many_arguments)]
async fn balance_index_leaf_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    pcells: &[IndexCell],
    children: &[u32],
    start: usize,
    end: usize,
    window_pages: &[u32],
    preset: Option<(u32, AVec<IndexCell>)>,
) -> Result<IndexBalance, S::Error> {
    let usable = pager.usable() as usize;
    // Gather: window leaves' entries in order, interleaved
    // with the parent dividers between them (converted to leaf
    // form).
    let mut all: AVec<IndexCell> = AVec::new();
    for (wi, wp) in window_pages.iter().enumerate() {
        let preset_hit = match &preset {
            Some((pp, kept)) if pp == wp => Some(kept),
            _ => None,
        };
        match preset_hit {
            Some(kept) => {
                for c in kept.iter() {
                    all.push(c.clone());
                }
            }
            None => {
                let idx = pager.load(*wp).await?;
                let snap = copy_bytes(pager.frame_data(idx));
                let cells = index_leaf_cells::<S::Error>(snap.as_slice(), *wp, usable)?;
                for c in cells.iter() {
                    all.push(c.clone());
                }
            }
        }
        if wi + 1 < window_pages.len() {
            all.push(recell(None, &pcells[start + wi], 4));
        }
    }
    if all.is_empty() {
        if start == 0 && end == children.len() {
            if parent == root {
                for wp in window_pages.iter() {
                    pager.free_page(*wp).await?;
                }
                let empty: &[IndexCell] = &[];
                write_index_page(pager, root, empty, 0, false, false).await?;
                return Ok(IndexBalance::Done {
                    children_removed: window_pages.len(),
                });
            }
            return Ok(IndexBalance::Rebuild);
        }
        let (nc, nright) = index_parent_after(pcells, children, start, end, &[], &[], 0);
        for wp in window_pages.iter() {
            pager.free_page(*wp).await?;
        }
        write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
        return Ok(IndexBalance::Done {
            children_removed: window_pages.len(),
        });
    }
    let n_new = index_min_pages(all.as_slice(), usable, false).min(window_pages.len());
    let (pages, dividers) = partition_index(all.as_slice(), n_new, usable, false);
    let n_new = pages.len();
    let Some(new_pages) = take_pages(window_pages, n_new) else {
        return Ok(IndexBalance::Grow);
    };
    let (nc, nright) = index_parent_after(
        pcells,
        children,
        start,
        end,
        new_pages.as_slice(),
        dividers.as_slice(),
        0,
    );
    // The lifted dividers may be larger than the ones pulled down:
    // a parent that no longer fits needs the growing balance.
    if index_page_need(nc.as_slice(), 0, true) > usable {
        return Ok(IndexBalance::Grow);
    }
    for (g, wp) in pages.iter().zip(new_pages.iter()) {
        write_index_page(pager, *wp, g.as_slice(), 0, false, false).await?;
    }
    for wp in window_pages.iter().skip(n_new) {
        pager.free_page(*wp).await?;
    }
    write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
    Ok(IndexBalance::Done {
        children_removed: window_pages.len() - n_new,
    })
}

/// The first `n` of a window's pages, or None when a partition
/// needs more pages than the window has (the delete balance never
/// allocates; the caller switches to the growing insert balance).
#[cfg(feature = "indexes")]
fn take_pages(window_pages: &[u32], n: usize) -> Option<AVec<u32>> {
    if n > window_pages.len() {
        return None;
    }
    let mut v = AVec::new();
    for wp in window_pages.iter().take(n) {
        v.push(*wp);
    }
    Some(v)
}

#[cfg(feature = "indexes")]
#[allow(clippy::too_many_arguments)]
async fn balance_index_interior_window<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    parent: u32,
    pcells: &[IndexCell],
    children: &[u32],
    start: usize,
    end: usize,
    window_pages: &[u32],
) -> Result<IndexBalance, S::Error> {
    let usable = pager.usable() as usize;
    // Gather: window interior pages' entries in order, each
    // page's rightmost pointer becoming the child of the
    // following parent divider item; the last page's rightmost
    // is the sequence's trailing child.
    let mut all: AVec<IndexCell> = AVec::new();
    let mut trailing = 0u32;
    for (wi, wp) in window_pages.iter().enumerate() {
        let idx = pager.load(*wp).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), *wp, usable)?;
        for c in cells.iter() {
            all.push(c.clone());
        }
        if wi + 1 < window_pages.len() {
            let mut d = pcells[start + wi].clone();
            patch_child(&mut d, rightmost);
            all.push(d);
        } else {
            trailing = rightmost;
        }
    }
    if all.is_empty() {
        if start == 0 && end == children.len() {
            if parent == root {
                for wp in window_pages.iter() {
                    pager.free_page(*wp).await?;
                }
                let empty: &[IndexCell] = &[];
                write_index_page(pager, root, empty, 0, false, false).await?;
                return Ok(IndexBalance::Done {
                    children_removed: window_pages.len(),
                });
            }
            return Ok(IndexBalance::Rebuild);
        }
        let (nc, nright) = index_parent_after(pcells, children, start, end, &[], &[], 4);
        for wp in window_pages.iter() {
            pager.free_page(*wp).await?;
        }
        write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
        return Ok(IndexBalance::Done {
            children_removed: window_pages.len(),
        });
    }
    let n_new = index_min_pages(all.as_slice(), usable, true).min(window_pages.len());
    let (pages, dividers) = partition_index(all.as_slice(), n_new, usable, true);
    let n_new = pages.len();
    let Some(new_pages) = take_pages(window_pages, n_new) else {
        return Ok(IndexBalance::Grow);
    };
    let (nc, nright) = index_parent_after(
        pcells,
        children,
        start,
        end,
        new_pages.as_slice(),
        dividers.as_slice(),
        4,
    );
    if index_page_need(nc.as_slice(), 0, true) > usable {
        return Ok(IndexBalance::Grow);
    }
    for (i, (g, wp)) in pages.iter().zip(new_pages.iter()).enumerate() {
        let right = match dividers.as_slice().get(i) {
            Some(d) if i + 1 < n_new => d.child,
            _ => trailing,
        };
        write_index_page(pager, *wp, g.as_slice(), right, true, false).await?;
    }
    for wp in window_pages.iter().skip(n_new) {
        pager.free_page(*wp).await?;
    }
    write_index_page(pager, parent, nc.as_slice(), nright, true, false).await?;
    Ok(IndexBalance::Done {
        children_removed: window_pages.len() - n_new,
    })
}

/// Collapse an index root that has been reduced to a single
/// child: copy the child's image into the root page and free
/// the child, repeating while the root stays a one-child
/// interior page.
#[cfg(feature = "indexes")]
async fn collapse_index_root<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    loop {
        let idx = pager.load(root).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), root)?;
        if h.kind != KIND_INTERIOR_INDEX {
            return Ok(());
        }
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), root, usable)?;
        if !cells.is_empty() {
            return Ok(());
        }
        let child = rightmost;
        let cidx = pager.load(child).await?;
        let csnap = copy_bytes(pager.frame_data(cidx));
        let ch = head::<S::Error>(csnap.as_slice(), child)?;
        match ch.kind {
            KIND_LEAF_INDEX => {
                let ccells = index_leaf_cells::<S::Error>(csnap.as_slice(), child, usable)?;
                write_index_page(pager, root, ccells.as_slice(), 0, false, false).await?;
            }
            KIND_INTERIOR_INDEX => {
                let (ccells, cright) =
                    index_interior_cells::<S::Error>(csnap.as_slice(), child, usable)?;
                write_index_page(pager, root, ccells.as_slice(), cright, true, false).await?;
            }
            _ => return Err(Error::Unsupported),
        }
        pager.free_page(child).await?;
    }
}

/// Rebuild the index rooted at `root` without the entry `skip`
/// (the index_delete fallback): collect the surviving entries,
/// free every page but the root, reset the root to an empty
/// leaf, and re-insert. The root page number never changes.
#[cfg(feature = "indexes")]
async fn index_rebuild_without<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    desc: &[KeyOrder],
    skip: &IndexEntry,
) -> Result<(), S::Error> {
    use core::cmp::Ordering;
    let mut keep: AVec<AVec<u8>> = AVec::new();
    {
        let nf = entry_fields(pager, root, desc);
        let mut cur = IndexCursor::with_fields(root, desc, nf);
        while let Some(e) = cur.next_entry(pager).await? {
            if compare_entries(
                e.values.as_slice(),
                skip.values.as_slice(),
                desc,
                pager.text_enc(),
            ) == Ordering::Equal
            {
                continue;
            }
            keep.push(encode_entry_payload(&e));
        }
    }
    let pages = collect_index_pages(pager, root).await?;
    for p in pages.iter() {
        if *p != root {
            pager.free_page(*p).await?;
        }
    }
    let empty: &[IndexCell] = &[];
    write_index_page(pager, root, empty, 0, false, false).await?;
    for p in keep.iter() {
        index_insert_core(pager, root, p.as_slice(), desc).await?;
    }
    Ok(())
}

/// How a leaf-level index removal ended, for the driver's
/// rebuild bookkeeping.
#[cfg(feature = "indexes")]
enum IndexRemoval {
    /// Removed; the tree is consistent.
    Done,
    /// A balance reported the degenerate window BEFORE any
    /// page was written: the entry is still in the tree.
    RebuildBeforeWrite,
    /// A balance cascade reported the degenerate window AFTER
    /// the entry was removed: the tree no longer holds it.
    RebuildAfterRemoval,
}

/// Delete the entry whose full key is `payload` (key columns +
/// rowid) from the index rooted at `root`. Returns whether an
/// entry was present.
///
/// The descent mirrors `btree_delete`: it records the path,
/// removes the entry from its leaf, and balances underfull
/// windows upward (redistribution / merge / cascade / root
/// collapse — the same rules as the table path). An entry
/// resident in an interior page is replaced by its predecessor
/// (the greatest entry of its left subtree); the predecessor is
/// removed from its leaf first, and the replacement is then
/// written wherever the entry lives by then (re-seek — a
/// balance may have moved it). The whole-tree rebuild survives
/// only as the degenerate-window fallback.
#[cfg(feature = "without_rowid")]
pub async fn index_delete<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    payload: &[u8],
    desc: &[KeyOrder],
) -> Result<bool, S::Error> {
    // A WITHOUT ROWID table's trees (v0.16; see `index_insert`).
    match pager.wr.view_of(root) {
        crate::wr::WrView::Pk(_) => return Ok(true),
        crate::wr::WrView::Sec(i) => {
            let (raw, orders) = pager.wr.sec_payload::<S::Error>(i, payload)?;
            return index_delete_core(pager, root, raw.as_slice(), orders.as_slice()).await;
        }
        _ => {}
    }
    index_delete_core(pager, root, payload, desc).await
}

/// Without WITHOUT ROWID tables there is nothing to translate.
#[cfg(all(feature = "indexes", not(feature = "without_rowid")))]
pub use self::index_delete_core as index_delete;

/// `index_delete` without the WITHOUT ROWID translation.
#[cfg(feature = "indexes")]
pub async fn index_delete_core<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    payload: &[u8],
    desc: &[KeyOrder],
) -> Result<bool, S::Error> {
    use core::cmp::Ordering;
    let usable = pager.usable() as usize;
    let nfields = entry_fields(pager, root, desc);
    let target = decode_index_entry::<S::Error>(payload, nfields)?;
    // ---- descent, recording (page, child index) pairs ----
    let mut path: AVec<(u32, usize)> = AVec::new();
    let mut page = root;
    // An interior-resident target leaves a pending replacement:
    // (predecessor entry, its leaf-form cell with a fresh
    // overflow chain already written).
    let mut pending: Option<(IndexEntry, IndexCell)> = None;
    let mut leaf_target = target.clone();
    let (leaf_page, leaf_cells, leaf_pos) = loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_INDEX {
                return Err(Error::Unsupported);
            }
            let cells = index_leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            let mut hit = None;
            for (i, c) in cells.iter().enumerate() {
                let p = index_cell_payload(pager, c).await?;
                let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
                if compare_entries(
                    e.values.as_slice(),
                    leaf_target.values.as_slice(),
                    desc,
                    pager.text_enc(),
                ) == Ordering::Equal
                {
                    hit = Some(i);
                    break;
                }
            }
            let Some(pos) = hit else {
                if let Some((_, cell)) = &pending
                    && cell.overflow != 0
                {
                    free_overflow_chain(pager, cell.overflow).await?;
                }
                return Ok(false);
            };
            break (page, cells, pos);
        }
        if h.kind != KIND_INTERIOR_INDEX {
            return Err(Error::Unsupported);
        }
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
        let mut descend = None;
        let mut child_idx = cells.len();
        let mut hit = None;
        for (i, c) in cells.iter().enumerate() {
            let p = index_cell_payload(pager, c).await?;
            let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
            match compare_entries(
                leaf_target.values.as_slice(),
                e.values.as_slice(),
                desc,
                pager.text_enc(),
            ) {
                Ordering::Equal => {
                    hit = Some(i);
                    break;
                }
                Ordering::Less => {
                    descend = Some(c.child);
                    child_idx = i;
                    break;
                }
                Ordering::Greater => {}
            }
        }
        if let Some(pos) = hit {
            if pending.is_some() {
                // The leaf target (a predecessor) is itself
                // interior-resident — impossible in a valid
                // tree (the predecessor is the maximum of the
                // subtree, so it lives in its rightmost leaf).
                return Err(Error::CorruptPage { page });
            }
            // Interior-resident target: replace with the
            // predecessor, then delete the predecessor from
            // its leaf. The replacement is applied by re-seek
            // after the leaf removal (see below).
            let child = cells.as_slice()[pos].child;
            let pred = {
                let mut cur = IndexCursor::with_fields(child, desc, nfields);
                let mut last: Option<IndexEntry> = None;
                while let Some(e) = cur.next_entry(pager).await? {
                    last = Some(e);
                }
                last
            };
            let Some(pred) = pred else {
                // The left subtree holds no entries at all
                // (defensive; a valid tree never has one):
                // drop the cell and free the subtree's pages.
                let sub = collect_index_pages(pager, child).await?;
                let mut kept: AVec<IndexCell> = AVec::new();
                for (j, c) in cells.iter().enumerate() {
                    if j != pos {
                        kept.push(c.clone());
                    }
                }
                write_index_page(pager, page, kept.as_slice(), rightmost, true, false).await?;
                for p in sub.iter() {
                    pager.free_page(*p).await?;
                }
                return Ok(true);
            };
            let pred_payload = encode_entry_payload(&pred);
            #[cfg(feature = "utf16")]
            let pred_payload =
                match stored_record::<S::Error>(pager.text_enc(), pred_payload.as_slice())? {
                    Some(c) => c,
                    None => pred_payload,
                };
            let local_len = index_local(usable, pred_payload.len());
            let ovf = write_overflow(pager, pred_payload.as_slice(), local_len).await?;
            let mut cell_bytes = AVec::new();
            encode_varint(pred_payload.len() as u64, &mut cell_bytes);
            for b in &pred_payload.as_slice()[..local_len] {
                cell_bytes.push(*b);
            }
            if ovf != 0 {
                for b in ovf.to_be_bytes() {
                    cell_bytes.push(b);
                }
            }
            let payload_off = cell_bytes.len() - local_len - if ovf != 0 { 4 } else { 0 };
            pad_cell(&mut cell_bytes, &[]);
            let pred_cell = IndexCell {
                child: 0,
                payload_off,
                bytes: cell_bytes,
                payload_len: pred_payload.len(),
                local_len,
                overflow: ovf,
            };
            pending = Some((pred.clone(), pred_cell));
            leaf_target = pred;
            path.push((page, pos));
            page = child;
            continue;
        }
        path.push((page, child_idx));
        page = descend.unwrap_or(rightmost);
    };
    // ---- remove the entry from its leaf ----
    let removed = leaf_cells.as_slice()[leaf_pos].clone();
    let mut kept: AVec<IndexCell> = AVec::new();
    for (i, c) in leaf_cells.iter().enumerate() {
        if i != leaf_pos {
            kept.push(c.clone());
        }
    }
    let removal = index_remove_from_leaf(pager, root, leaf_page, kept, path.as_slice()).await?;
    match removal {
        IndexRemoval::Done => {}
        IndexRemoval::RebuildBeforeWrite | IndexRemoval::RebuildAfterRemoval => {
            // Fall back to a whole-index rebuild. Before-write:
            // the tree still holds both entries, rebuild without
            // the target. After-removal: the leaf entry is gone,
            // so the rebuild must re-add it (it survives as the
            // pending replacement's source entry — for a plain
            // leaf delete there is no pending entry, and the
            // removed entry is simply the target itself, which
            // the rebuild skips anyway; the removed entry is
            // re-added only when a replacement is pending).
            if let Some((_, cell)) = &pending
                && cell.overflow != 0
            {
                free_overflow_chain(pager, cell.overflow).await?;
            }
            match (removal, &pending) {
                (IndexRemoval::RebuildAfterRemoval, Some((pred, _))) => {
                    index_rebuild_without_extra(pager, root, desc, &target, Some(pred)).await?;
                }
                _ => {
                    index_rebuild_without(pager, root, desc, &target).await?;
                }
            }
            return Ok(true);
        }
    }
    if removed.overflow != 0 {
        free_overflow_chain(pager, removed.overflow).await?;
    }
    // ---- apply a pending predecessor replacement by re-seek --
    if let Some((pred_entry, pred_cell)) = pending {
        index_apply_replacement(pager, root, &target, &pred_entry, &pred_cell, desc).await?;
    }
    Ok(true)
}

/// Remove `kept`'s page-mate entry: write `kept` as the new
/// content of `leaf_page` (a non-root leaf whose previous
/// content was `kept` plus the removed entry), balancing
/// upward along `path` when the leaf underflows. The removed
/// entry's overflow chain is the caller's to free, and only on
/// `Done`.
#[cfg(feature = "indexes")]
async fn index_remove_from_leaf<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    leaf_page: u32,
    kept: AVec<IndexCell>,
    path: &[(u32, usize)],
) -> Result<IndexRemoval, S::Error> {
    let usable = pager.usable() as usize;
    if leaf_page == root {
        write_index_page(pager, leaf_page, kept.as_slice(), 0, false, false).await?;
        return Ok(IndexRemoval::Done);
    }
    let free = usable - index_page_need(kept.as_slice(), 0, false);
    if !page_underfull(free, usable) {
        write_index_page(pager, leaf_page, kept.as_slice(), 0, false, false).await?;
        return Ok(IndexRemoval::Done);
    }
    let Some(&(parent, trigger)) = path.last() else {
        return Err(Error::CorruptPage { page: leaf_page });
    };
    let outcome = balance_index_window(
        pager,
        root,
        parent,
        trigger,
        Some((leaf_page, kept.clone())),
    )
    .await?;
    let mut removed_children = match outcome {
        IndexBalance::Rebuild => return Ok(IndexRemoval::RebuildBeforeWrite),
        IndexBalance::Grow => {
            let up = ins_balance_index_leaf_window(pager, parent, trigger, leaf_page, kept).await?;
            cascade_index_up(pager, root, path, up).await?;
            collapse_index_root(pager, root).await?;
            return Ok(IndexRemoval::Done);
        }
        IndexBalance::Done { children_removed } => children_removed,
    };
    // Cascade, mirroring the table delete: balance each
    // ancestor that lost children and is itself underfull;
    // collapse a one-child root.
    let mut cur = path.len() - 1;
    loop {
        let cur_page = path[cur].0;
        if cur_page == root {
            collapse_index_root(pager, root).await?;
            break;
        }
        if removed_children == 0 {
            break;
        }
        let idx = pager.load(cur_page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), cur_page)?;
        if h.kind != KIND_INTERIOR_INDEX {
            break;
        }
        let (cells, _) = index_interior_cells::<S::Error>(snap.as_slice(), cur_page, usable)?;
        let free = usable - index_page_need(cells.as_slice(), 0, true);
        if !page_underfull(free, usable) {
            break;
        }
        if cur == 0 {
            break;
        }
        let (gp, gtrigger) = path[cur - 1];
        let outcome = balance_index_window(pager, root, gp, gtrigger, None).await?;
        match outcome {
            IndexBalance::Rebuild => return Ok(IndexRemoval::RebuildAfterRemoval),
            IndexBalance::Grow => {
                // No preset: page 0 is never a window page, so every
                // window page is read from its image.
                let up = ins_balance_index_interior_window(pager, gp, gtrigger, 0, AVec::new(), 0)
                    .await?;
                cascade_index_up(pager, root, path, up).await?;
                collapse_index_root(pager, root).await?;
                return Ok(IndexRemoval::Done);
            }
            IndexBalance::Done { children_removed } => {
                removed_children = children_removed;
            }
        }
        cur -= 1;
    }
    Ok(IndexRemoval::Done)
}

/// Write the predecessor replacement: find `target` again
/// (a balance may have moved it between pages or between an
/// interior page and a leaf) and replace its cell with
/// `pred_cell` (leaf form; re-celled onto the found cell's
/// child when the found cell is interior). Falls back to a
/// rebuild when the replacement does not fit the page it now
/// lives on.
#[cfg(feature = "indexes")]
async fn index_apply_replacement<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    target: &IndexEntry,
    pred_entry: &IndexEntry,
    pred_cell: &IndexCell,
    desc: &[KeyOrder],
) -> Result<(), S::Error> {
    use core::cmp::Ordering;
    let usable = pager.usable() as usize;
    let nfields = entry_fields(pager, root, desc);
    let mut page = root;
    loop {
        let idx = pager.load(page).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let h = head::<S::Error>(snap.as_slice(), page)?;
        if h.is_leaf() {
            if h.kind != KIND_LEAF_INDEX {
                return Err(Error::Unsupported);
            }
            let cells = index_leaf_cells::<S::Error>(snap.as_slice(), page, usable)?;
            for (i, c) in cells.iter().enumerate() {
                let p = index_cell_payload(pager, c).await?;
                let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
                if compare_entries(
                    e.values.as_slice(),
                    target.values.as_slice(),
                    desc,
                    pager.text_enc(),
                ) == Ordering::Equal
                {
                    let mut nc: AVec<IndexCell> = AVec::new();
                    for (j, cc) in cells.iter().enumerate() {
                        nc.push(if j == i {
                            pred_cell.clone()
                        } else {
                            cc.clone()
                        });
                    }
                    if index_page_need(nc.as_slice(), 0, false) > usable {
                        if pred_cell.overflow != 0 {
                            free_overflow_chain(pager, pred_cell.overflow).await?;
                        }
                        // The predecessor entry is already out
                        // of its leaf; re-add it via the rebuild.
                        index_rebuild_without_extra(pager, root, desc, target, Some(pred_entry))
                            .await?;
                        return Ok(());
                    }
                    let old_ovf = cells.as_slice()[i].overflow;
                    write_index_page(pager, page, nc.as_slice(), 0, false, false).await?;
                    if old_ovf != 0 {
                        free_overflow_chain(pager, old_ovf).await?;
                    }
                    return Ok(());
                }
            }
            return Err(Error::CorruptPage { page });
        }
        if h.kind != KIND_INTERIOR_INDEX {
            return Err(Error::Unsupported);
        }
        let (cells, rightmost) = index_interior_cells::<S::Error>(snap.as_slice(), page, usable)?;
        let mut descend = None;
        let mut hit = None;
        for (i, c) in cells.iter().enumerate() {
            let p = index_cell_payload(pager, c).await?;
            let e = decode_index_entry::<S::Error>(p.as_slice(), nfields)?;
            match compare_entries(
                target.values.as_slice(),
                e.values.as_slice(),
                desc,
                pager.text_enc(),
            ) {
                Ordering::Equal => {
                    hit = Some(i);
                    break;
                }
                Ordering::Less => {
                    descend = Some(c.child);
                    break;
                }
                Ordering::Greater => {}
            }
        }
        let Some(pos) = hit else {
            page = descend.unwrap_or(rightmost);
            continue;
        };
        let found = cells.as_slice()[pos].clone();
        let new_cell = recell(Some(found.child), pred_cell, 0);
        let mut nc: AVec<IndexCell> = AVec::new();
        for (j, c) in cells.iter().enumerate() {
            nc.push(if j == pos {
                new_cell.clone()
            } else {
                c.clone()
            });
        }
        if index_page_need(nc.as_slice(), 0, true) > usable {
            if pred_cell.overflow != 0 {
                free_overflow_chain(pager, pred_cell.overflow).await?;
            }
            index_rebuild_without_extra(pager, root, desc, target, Some(pred_entry)).await?;
            return Ok(());
        }
        write_index_page(pager, page, nc.as_slice(), rightmost, true, false).await?;
        if found.overflow != 0 {
            free_overflow_chain(pager, found.overflow).await?;
        }
        return Ok(());
    }
}

/// Rebuild without `skip`, re-adding `extra` (an entry the
/// tree currently lacks because a balance removed it before
/// the rebuild fallback fired).
#[cfg(feature = "indexes")]
async fn index_rebuild_without_extra<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    desc: &[KeyOrder],
    skip: &IndexEntry,
    extra: Option<&IndexEntry>,
) -> Result<(), S::Error> {
    use core::cmp::Ordering;
    let mut keep: AVec<AVec<u8>> = AVec::new();
    {
        let nf = entry_fields(pager, root, desc);
        let mut cur = IndexCursor::with_fields(root, desc, nf);
        while let Some(e) = cur.next_entry(pager).await? {
            if compare_entries(
                e.values.as_slice(),
                skip.values.as_slice(),
                desc,
                pager.text_enc(),
            ) == Ordering::Equal
            {
                continue;
            }
            keep.push(encode_entry_payload(&e));
        }
    }
    if let Some(x) = extra
        && compare_entries(
            x.values.as_slice(),
            skip.values.as_slice(),
            desc,
            pager.text_enc(),
        ) != Ordering::Equal
    {
        keep.push(encode_entry_payload(x));
    }
    let pages = collect_index_pages(pager, root).await?;
    for p in pages.iter() {
        if *p != root {
            pager.free_page(*p).await?;
        }
    }
    let empty: &[IndexCell] = &[];
    write_index_page(pager, root, empty, 0, false, false).await?;
    for p in keep.iter() {
        index_insert_core(pager, root, p.as_slice(), desc).await?;
    }
    Ok(())
}
