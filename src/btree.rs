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
    Ok(())
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
        }
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

/// Largest rowid in the table rooted at `root`, if any.
pub async fn seek_max_rowid<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
) -> Result<Option<i64>, S::Error> {
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

/// Find one row by rowid.
pub async fn seek_rowid<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    rowid: i64,
) -> Result<Option<AVec<u8>>, S::Error> {
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
        let idx = pager.load(p).await?;
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
    let idx = pager.load(page).await?;
    pager.mark_frame_dirty(idx);
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
    let usable = pager.usable() as usize;
    let page_size = pager.page_size() as usize;
    // ---- descent, remembering (interior page, child index) ----
    let mut path: AVec<(u32, usize)> = AVec::new();
    let mut page = root;
    let (leaf_page, leaf_cells, insert_pos) = loop {
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

    let hdr_off = if leaf_page == 1 { 100 } else { 0 };
    if leaf_need(all.as_slice(), hdr_off) <= usable {
        let idx = pager.dirty_page(leaf_page).await?;
        let base = copy_bytes(pager.frame_data(idx));
        let img = build_leaf_image(leaf_page, page_size, all.as_slice(), base.as_slice());
        set_page_image(pager, leaf_page, img.as_slice()).await?;
        return Ok(());
    }

    // ---- split the leaf ----
    let (left, right) = split_leaf_cells(all);
    let divider = left.as_slice()[left.len() - 1].rowid;
    if path.is_empty() {
        // Leaf is the root: root becomes an interior over two fresh
        // children (its number — and schema references — stay).
        let (lp, _) = pager.alloc_page().await?;
        let (rp, _) = pager.alloc_page().await?;
        write_leaf_page(pager, lp, left.as_slice(), true).await?;
        write_leaf_page(pager, rp, right.as_slice(), true).await?;
        let idx = pager.dirty_page(root).await?;
        let base = copy_bytes(pager.frame_data(idx));
        let pairs = [(lp, divider)];
        let img = build_interior_image(root, page_size, &pairs, rp, base.as_slice());
        set_page_image(pager, root, img.as_slice()).await?;
        return Ok(());
    }
    let (right_page, _) = pager.alloc_page().await?;
    write_leaf_page(pager, right_page, right.as_slice(), true).await?;
    write_leaf_page(pager, leaf_page, left.as_slice(), false).await?;
    propagate(pager, path, leaf_page, right_page, divider).await
}

/// Partition sorted cells into (left, right) halves by byte need.
fn split_leaf_cells(all: AVec<LeafCell>) -> (AVec<LeafCell>, AVec<LeafCell>) {
    let total: usize = all.iter().map(|c| c.bytes.len() + 2).sum();
    let half = total.div_ceil(2);
    let mut left = AVec::new();
    let mut right = AVec::new();
    let mut acc = 0usize;
    for c in all.iter() {
        if !left.is_empty() && acc >= half {
            right.push(c.clone());
        } else {
            left.push(c.clone());
            acc += c.bytes.len() + 2;
        }
    }
    if right.is_empty() && left.len() > 1 {
        let last = left.pop().expect("len>1");
        right.push(last);
    }
    (left, right)
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

/// Splice (left --divider--> right) into the ancestor chain and
/// propagate splits upward. When the chain empties, the page that
/// split was the root: finish with the root-split rewrite.
async fn propagate<S: Storage>(
    pager: &mut Pager<S>,
    mut path: AVec<(u32, usize)>,
    mut left: u32,
    mut right: u32,
    mut divider: i64,
) -> Result<(), S::Error> {
    let page_size = pager.page_size() as usize;
    loop {
        let (parent, ci) = match path.pop() {
            Some(v) => v,
            None => {
                // `left` is the original root page; its left-half
                // image is already in place (written as `left`
                // during the previous iteration's split, or it is
                // the leaf that started this — but a leaf root is
                // handled in btree_insert, so `left` here is an
                // interior page whose halves are (left, right)).
                return finish_root_split(pager, left, right, divider).await;
            }
        };
        let is_root = path.is_empty();
        // Load + parse the parent's current model.
        let idx = pager.load(parent).await?;
        let snap = copy_bytes(pager.frame_data(idx));
        let (pairs, rightmost) = interior_cells::<S::Error>(snap.as_slice(), parent)?;
        // Children/keys splice.
        let mut children: AVec<u32> = AVec::new();
        let mut keys: AVec<i64> = AVec::new();
        for (c, k) in pairs.iter() {
            children.push(*c);
            keys.push(*k);
        }
        children.push(rightmost);
        debug_assert_eq!(children.as_slice()[ci], left, "descent path child mismatch");
        let mut nc: AVec<u32> = AVec::new();
        let mut nk: AVec<i64> = AVec::new();
        for i in 0..children.len() {
            nc.push(children.as_slice()[i]);
            if i == ci {
                nc.push(right);
            }
            if i < keys.len() {
                if i == ci {
                    nk.push(divider);
                }
                nk.push(keys.as_slice()[i]);
            }
        }
        if ci >= keys.len() {
            nk.push(divider);
        }
        let mut npairs: AVec<(u32, i64)> = AVec::new();
        for j in 0..nk.len() {
            npairs.push((nc.as_slice()[j], nk.as_slice()[j]));
        }
        let nrightmost = *nc.as_slice().last().expect("nonempty children");
        let hdr_off = if parent == 1 { 100 } else { 0 };
        if interior_need(npairs.as_slice(), hdr_off) <= page_size {
            let idx = pager.dirty_page(parent).await?;
            let base = copy_bytes(pager.frame_data(idx));
            let img = build_interior_image(
                parent,
                page_size,
                npairs.as_slice(),
                nrightmost,
                base.as_slice(),
            );
            set_page_image(pager, parent, img.as_slice()).await?;
            return Ok(());
        }
        // Split the parent's pairs: NP[0..s] stays, NP[s].key goes
        // up, NP[s+1..] moves right; left's rightmost = NP[s].child.
        let m = npairs.len();
        let s = m / 2;
        let up_key = npairs.as_slice()[s].1;
        let left_rightmost = npairs.as_slice()[s].0;
        let mut lpairs: AVec<(u32, i64)> = AVec::new();
        for j in 0..s {
            lpairs.push(npairs.as_slice()[j]);
        }
        let mut rpairs: AVec<(u32, i64)> = AVec::new();
        for j in s + 1..m {
            rpairs.push(npairs.as_slice()[j]);
        }
        if is_root {
            // Root split by data: two fresh children get the halves,
            // the root page is rewritten as a one-pair interior.
            let (lp, _) = pager.alloc_page().await?;
            let (rp, _) = pager.alloc_page().await?;
            write_interior_page(pager, lp, lpairs.as_slice(), left_rightmost, true).await?;
            write_interior_page(pager, rp, rpairs.as_slice(), nrightmost, true).await?;
            let idx = pager.dirty_page(parent).await?;
            let base = copy_bytes(pager.frame_data(idx));
            let one = [(lp, up_key)];
            let img = build_interior_image(parent, page_size, &one, rp, base.as_slice());
            set_page_image(pager, parent, img.as_slice()).await?;
            return Ok(());
        }
        let (right_page, _) = pager.alloc_page().await?;
        write_interior_page(pager, right_page, rpairs.as_slice(), nrightmost, true).await?;
        write_interior_page(pager, parent, lpairs.as_slice(), left_rightmost, false).await?;
        left = parent;
        right = right_page;
        divider = up_key;
    }
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

/// Finish a split that reached the root through `propagate`'s empty
/// path. In this implementation splits are resolved one level at a
/// time and the root case is handled inside `propagate` via
/// `is_root`, so reaching here means the descent path was
/// inconsistent — a defensive error, never a silent corruption.
async fn finish_root_split<S: Storage>(
    _pager: &mut Pager<S>,
    _left: u32,
    _right: u32,
    _divider: i64,
) -> Result<(), S::Error> {
    Err(Error::Unsupported)
}
