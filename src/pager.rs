//! Pager: page cache, database header, rollback-journal protocol,
//! and the resumable commit stage machine (SPEC §5.2, §5.3).
//!
//! All page access goes through the pager; nothing above this layer
//! touches `Storage`. Dirty-page images live in the fixed page pool
//! (frames), never in an arena (SPEC §7.5.5).

use alloc::vec::Vec;

use crate::arena::Arena;
use crate::error::{Error, Result};
use crate::storage::{Sidecar, SidecarFile, Storage};

pub const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
pub const SECTOR_SIZE: u32 = 512;
/// SQLITE_VERSION_NUMBER written into header offset 96 (D1 pin).
pub const PIN_VERSION_NUMBER: u32 = 3_054_001;

pub fn be_u16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
pub fn be_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
pub fn put_be_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
pub fn put_be_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// The journal record checksum (SQFMT §3, verified against the
/// format documentation): start from the nonce, sample single bytes
/// at `page_size-200`, stepping down by 200 while the index is >= 0.
pub fn journal_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut x = nonce;
    let mut i: i64 = page.len() as i64 - 200;
    while i >= 0 {
        x = x.wrapping_add(u32::from(page[i as usize]));
        i -= 200;
    }
    x
}

/// Parsed database header fields the engine tracks (SQFMT §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbHeader {
    pub page_size: u32,
    pub change_counter: u32,
    pub page_count: u32,
    pub freelist_trunk: u32,
    pub freelist_count: u32,
    pub schema_cookie: u32,
}

fn parse_page_size_field<E>(raw: u16) -> Result<u32, E> {
    if raw == 1 {
        return Ok(65536);
    }
    let v = u32::from(raw);
    if (512..=32768).contains(&v) && v.is_power_of_two() {
        Ok(v)
    } else {
        Err(Error::UnsupportedPageSize)
    }
}

/// Validate + parse the 100-byte header probe (SPEC §5.2's order).
pub fn parse_db_header<E>(probe: &[u8], file_len: u64) -> Result<DbHeader, E> {
    if probe.len() < 100 {
        return Err(Error::TruncatedHeader);
    }
    if &probe[0..16] != b"SQLite format 3\0" {
        return Err(Error::BadMagic);
    }
    let page_size = parse_page_size_field::<E>(be_u16(probe, 16))?;
    // Write version at 18, read version at 19; 2 means WAL.
    if probe[18] == 2 || probe[19] == 2 {
        return Err(Error::WalUnsupported);
    }
    if probe[21] != 64 || probe[22] != 32 || probe[23] != 32 {
        return Err(Error::CorruptPage { page: 1 });
    }
    let encoding = be_u32(probe, 56);
    if encoding == 2 || encoding == 3 {
        return Err(Error::UnsupportedEncoding);
    }
    if encoding != 1 {
        return Err(Error::CorruptPage { page: 1 });
    }
    let change_counter = be_u32(probe, 24);
    let in_header_size = be_u32(probe, 28);
    let version_valid_for = be_u32(probe, 92);
    let page_count = if in_header_size != 0 && change_counter == version_valid_for {
        in_header_size
    } else {
        // Trust the file's actual length instead (SQFMT §1.3.7).
        if !file_len.is_multiple_of(u64::from(page_size)) {
            return Err(Error::CorruptPage { page: 1 });
        }
        (file_len / u64::from(page_size)) as u32
    };
    Ok(DbHeader {
        page_size,
        change_counter,
        page_count,
        freelist_trunk: be_u32(probe, 32),
        freelist_count: be_u32(probe, 36),
        schema_cookie: be_u32(probe, 40),
    })
}

/// Build a fresh page-1 image: database header + empty
/// `sqlite_schema` leaf root (SPEC §9.1.1).
pub fn initial_page1(page_size: u32) -> Vec<u8> {
    let mut p = alloc::vec![0u8; page_size as usize];
    p[0..16].copy_from_slice(b"SQLite format 3\0");
    put_be_u16(
        &mut p,
        16,
        if page_size == 65536 {
            1
        } else {
            page_size as u16
        },
    );
    p[18] = 1; // legacy (rollback) write version
    p[19] = 1; // legacy read version
    p[20] = 0; // reserved bytes
    p[21] = 64;
    p[22] = 32;
    p[23] = 32;
    put_be_u32(&mut p, 24, 1); // change counter
    put_be_u32(&mut p, 28, 1); // size in pages
    put_be_u32(&mut p, 40, 0); // schema cookie
    put_be_u32(&mut p, 44, 4); // schema format 4
    put_be_u32(&mut p, 56, 1); // UTF-8
    put_be_u32(&mut p, 92, 1); // version-valid-for = change counter
    put_be_u32(&mut p, 96, PIN_VERSION_NUMBER);
    // B-tree leaf header at offset 100.
    p[100] = 0x0D;
    put_be_u16(&mut p, 103, 0); // cell count
    put_be_u16(
        &mut p,
        105,
        if page_size == 65536 {
            0
        } else {
            page_size as u16
        },
    ); // cell content start
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStage {
    /// No transaction is open.
    Idle,
    /// Transaction is open; pages are being dirtied, journal records
    /// are being appended. Commit has not begun.
    Writing,
    /// Commit: journal must be synced before any data page moves.
    SyncJournal,
    /// Commit: writing dirty page `next` of the dirty list.
    WritePages { next: usize },
    /// Commit: main file sync.
    SyncData,
    /// Commit: deleting the journal — the commit point.
    DeleteJournal,
}

struct Frame {
    page: u32,
    data: Vec<u8>,
    dirty: bool,
    clock_ref: bool,
}

pub struct Pager<S: Storage> {
    storage: S,
    header: DbHeader,
    reserved_bytes: u8,
    frames: Vec<Frame>,
    cache_cap: usize,
    sidecar: Option<S::Sidecar>,
    stage: CommitStage,
    orig_page_count: u32,
    dirty: Vec<u32>,
    journaled: Vec<u32>,
    journal_len: u64,
    nonce: u32,
    nonce_counter: u64,
    nonce_seed: u64,
    pub txn_arena: Arena,
}

impl<S: Storage> Pager<S> {
    /// Usable bytes per page (page size minus reserved bytes).
    pub fn usable(&self) -> u32 {
        self.page_size() - u32::from(self.reserved_bytes)
    }

    pub fn page_size(&self) -> u32 {
        self.header.page_size
    }
    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }
    pub fn schema_cookie(&self) -> u32 {
        self.header.schema_cookie
    }
    pub fn stage(&self) -> CommitStage {
        self.stage
    }
    pub fn in_txn(&self) -> bool {
        self.stage != CommitStage::Idle
    }
    pub fn in_commit(&self) -> bool {
        !matches!(self.stage, CommitStage::Idle | CommitStage::Writing)
    }
    pub fn into_storage(self) -> S {
        self.storage
    }
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Open (or create) the database on `storage`. Handles header
    /// validation and hot-journal recovery (SPEC §5.2/§5.3).
    /// Returns the pager plus whether the file was created new.
    pub async fn open(
        mut storage: S,
        cache_cap: usize,
        new_page_size: u32,
        nonce_seed: u64,
        txn_arena: Arena,
    ) -> Result<(Pager<S>, bool), S::Error> {
        let file_len = storage.len().await.map_err(Error::Storage)?;
        let mut is_new = false;
        if file_len == 0 {
            // Create-new: header + empty schema leaf, then a sync.
            // A crash here leaves a zero/garbage file that fails
            // validation on next open — never a torn database.
            let img = initial_page1(new_page_size);
            storage.write_at(0, &img).await.map_err(Error::Storage)?;
            storage.sync().await.map_err(Error::Storage)?;
            is_new = true;
        }
        let mut probe = [0u8; 100];
        let file_len = storage.len().await.map_err(Error::Storage)?;
        if file_len < 100 {
            return Err(Error::TruncatedHeader);
        }
        storage
            .read_at(0, &mut probe)
            .await
            .map_err(Error::Storage)?;
        let mut header = parse_db_header::<S::Error>(&probe, file_len)?;
        let reserved_bytes = probe[20];

        // Hot-journal recovery: a journal that exists, is well-formed
        // (magic), and holds checksummed records is rolled back
        // before the first read (SPEC §5.3.4).
        let mut sidecar = storage
            .sidecar(Sidecar::RollbackJournal)
            .await
            .map_err(Error::Storage)?;
        let jlen = sidecar.len().await.map_err(Error::Storage)?;
        if sidecar.existed() && jlen > 0 {
            let recovered = replay_journal(&mut storage, &mut sidecar, jlen).await?;
            if recovered {
                // Header fields may have been rolled back too.
                storage
                    .read_at(0, &mut probe)
                    .await
                    .map_err(Error::Storage)?;
                let fl = storage.len().await.map_err(Error::Storage)?;
                header = parse_db_header::<S::Error>(&probe, fl)?;
            }
        }

        let frames = (0..cache_cap)
            .map(|_| Frame {
                page: 0,
                data: alloc::vec![0u8; header.page_size as usize],
                dirty: false,
                clock_ref: true,
            })
            .collect();
        Ok((
            Pager {
                storage,
                header,
                reserved_bytes,
                frames,
                cache_cap,
                sidecar: Some(sidecar),
                stage: CommitStage::Idle,
                orig_page_count: header.page_count,
                dirty: Vec::new(),
                journaled: Vec::new(),
                journal_len: 0,
                nonce: 0,
                nonce_counter: 0,
                nonce_seed,
                txn_arena,
            },
            is_new,
        ))
    }

    fn check_page(&self, page: u32) -> Result<(), S::Error> {
        if page == 0 || page > self.header.page_count {
            return Err(Error::PageOutOfRange { page });
        }
        Ok(())
    }

    fn find_frame(&self, page: u32) -> Option<usize> {
        self.frames.iter().position(|f| f.page == page)
    }

    /// Load `page` into the cache (evicting by CLOCK if needed) and
    /// return its frame index. Frames are addressed by index, never
    /// by pointer, so pins survive everything (SPEC §5.2).
    pub async fn load(&mut self, page: u32) -> Result<usize, S::Error> {
        self.check_page(page)?;
        if let Some(i) = self.find_frame(page) {
            self.frames[i].clock_ref = true;
            return Ok(i);
        }
        // Pick a victim: empty first, then CLOCK over clean frames.
        let victim = {
            let mut chosen = None;
            if let Some(i) = self.frames.iter().position(|f| f.page == 0) {
                chosen = Some(i);
            } else {
                for _ in 0..2 * self.frames.len() {
                    let i = self
                        .frames
                        .iter()
                        .position(|f| !f.dirty)
                        .unwrap_or(usize::MAX);
                    if i == usize::MAX {
                        break;
                    }
                    if self.frames[i].clock_ref {
                        self.frames[i].clock_ref = false;
                    } else {
                        chosen = Some(i);
                        break;
                    }
                }
            }
            chosen
        };
        let vi = match victim {
            Some(i) => i,
            None => return Err(Error::CacheExhausted),
        };
        let ps = self.header.page_size as usize;
        let off = u64::from(page - 1) * ps as u64;
        // Disjoint field borrows (frames vs storage): the read lands
        // directly in the frame — no copy, no allocation (SPEC §5.2).
        self.storage
            .read_at(off, &mut self.frames[vi].data)
            .await
            .map_err(Error::Storage)?;
        self.frames[vi].page = page;
        self.frames[vi].dirty = false;
        self.frames[vi].clock_ref = true;
        Ok(vi)
    }

    /// Frame access after `load` (no I/O).
    pub fn frame_data(&self, idx: usize) -> &[u8] {
        &self.frames[idx].data
    }
    /// Mark an already-loaded frame dirty without journalling —
    /// for pages freshly allocated by `alloc_page` (they have no
    /// on-disk original worth preserving; rollback truncates them).
    pub fn mark_frame_dirty(&mut self, idx: usize) {
        self.frames[idx].dirty = true;
    }

    pub fn frame_data_mut(&mut self, idx: usize) -> &mut [u8] {
        &mut self.frames[idx].data
    }
    pub fn frame_page(&self, idx: usize) -> u32 {
        self.frames[idx].page
    }

    /// Begin the (single, autocommit) write transaction if none is
    /// open: open the journal sidecar and write its header sector.
    pub async fn begin_write(&mut self) -> Result<(), S::Error> {
        if self.reserved_bytes != 0 {
            return Err(Error::ReservedBytesUnsupported);
        }
        match self.stage {
            CommitStage::Idle => {}
            CommitStage::Writing => return Ok(()),
            _ => return Err(Error::OperationInFlight),
        }
        // Charge the transaction arena for per-txn bookkeeping
        // (dirty/journal lists sized by the page pool) — this is the
        // A9 enforcement point for the transaction scope (SPEC
        // §7.5.4): it fails before anything is written anywhere.
        self.txn_arena.charge(self.cache_cap * 4 + 64)?;
        // Fresh nonce per transaction (SPEC §5.3; R2's entropy note:
        // the seed is Config-supplied; targets with real entropy
        // should seed it — a counter mix is the documented floor).
        self.nonce_counter += 1;
        let mut x = self.nonce_seed ^ self.nonce_counter.wrapping_mul(0x9e37_79b1_85eb_ca87);
        x ^= x >> 33;
        self.nonce = (x as u32) | 1;
        self.orig_page_count = self.header.page_count;
        // Journal header sector: magic, page-count = -1 (single
        // segment, "to end of file"), nonce, original size, sector
        // size, page size; zero-padded to one sector.
        let mut hdr = [0u8; SECTOR_SIZE as usize];
        hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
        put_be_u32(&mut hdr, 8, u32::MAX);
        put_be_u32(&mut hdr, 12, self.nonce);
        put_be_u32(&mut hdr, 16, self.orig_page_count);
        put_be_u32(&mut hdr, 20, SECTOR_SIZE);
        put_be_u32(&mut hdr, 24, self.header.page_size);
        let sidecar = self.sidecar.as_mut().expect("sidecar from open");
        sidecar.write_at(0, &hdr).await.map_err(Error::Storage)?;
        self.journal_len = u64::from(SECTOR_SIZE);
        self.stage = CommitStage::Writing;
        Ok(())
    }

    /// Ensure `page`'s original image is in the journal (appending a
    /// record if this transaction has not journaled it yet).
    async fn journal_original(&mut self, page: u32, frame_idx: usize) -> Result<(), S::Error> {
        if self.journaled.contains(&page) {
            return Ok(());
        }
        let ps = self.header.page_size as usize;
        let cksum = journal_checksum(self.nonce, &self.frames[frame_idx].data);
        let mut tail = [0u8; 8];
        put_be_u32(&mut tail, 0, page);
        let sidecar = self.sidecar.as_mut().expect("sidecar from open");
        let off = self.journal_len;
        sidecar
            .write_at(off, &tail[0..4])
            .await
            .map_err(Error::Storage)?;
        // Write the page image directly from the frame via a split
        // borrow dance: copy the frame ref first.
        let img_ptr: &[u8] = &self.frames[frame_idx].data;
        sidecar
            .write_at(off + 4, img_ptr)
            .await
            .map_err(Error::Storage)?;
        put_be_u32(&mut tail, 0, cksum);
        sidecar
            .write_at(off + 4 + ps as u64, &tail[0..4])
            .await
            .map_err(Error::Storage)?;
        self.journal_len = off + 8 + ps as u64;
        self.journaled.push(page);
        Ok(())
    }

    /// Mark `page` dirty for this transaction, journalling its
    /// original image first (SPEC §5.3.1). Returns the frame index.
    pub async fn dirty_page(&mut self, page: u32) -> Result<usize, S::Error> {
        self.begin_write().await?;
        let idx = self.load(page).await?;
        if !self.frames[idx].dirty {
            self.journal_original(page, idx).await?;
            self.frames[idx].dirty = true;
            self.dirty.push(page);
        }
        Ok(idx)
    }

    /// Allocate a page: pop the freelist first (consuming foreign
    /// freelist pages correctly), else grow the file (SPEC §5.4).
    /// The returned frame is zeroed, dirty, and NOT journaled (a new
    /// page has no original; rollback truncates it away).
    pub async fn alloc_page(&mut self) -> Result<(u32, usize), S::Error> {
        self.begin_write().await?;
        if self.header.freelist_count > 0 {
            let trunk = self.header.freelist_trunk;
            if trunk < 2 || trunk > self.header.page_count {
                return Err(Error::CorruptPage { page: trunk });
            }
            let ti = self.load(trunk).await?;
            let (next_trunk, leaf_count) = {
                let d = &self.frames[ti].data;
                (be_u32(d, 0), be_u32(d, 4))
            };
            if leaf_count > 0 {
                let leaf_off = 8 + 4 * (leaf_count as usize - 1);
                let leaf = {
                    let d = &self.frames[ti].data;
                    if leaf_off + 4 > d.len() {
                        return Err(Error::CorruptPage { page: trunk });
                    }
                    be_u32(d, leaf_off)
                };
                if leaf < 2 || leaf > self.header.page_count || leaf == trunk {
                    return Err(Error::CorruptPage { page: leaf });
                }
                // Decrement the trunk's leaf count (a journaled,
                // dirtying write to the trunk page).
                let ti = self.dirty_page(trunk).await?;
                put_be_u32(&mut self.frames[ti].data, 4, leaf_count - 1);
                self.header.freelist_count -= 1;
                return self.fresh_frame(leaf).await;
            }
            // The trunk page itself becomes the allocated page.
            self.header.freelist_trunk = next_trunk;
            self.header.freelist_count -= 1;
            // Journal its trunk image before overwriting.
            let ti = self.dirty_page(trunk).await?;
            for b in self.frames[ti].data.iter_mut() {
                *b = 0;
            }
            return Ok((trunk, ti));
        }
        // Grow: page_count + 1.
        let page = self.header.page_count + 1;
        self.header.page_count = page;
        self.fresh_frame(page).await
    }

    /// Create/get a zeroed dirty frame for a brand-new page (already
    /// counted in `header.page_count`; not journaled).
    async fn fresh_frame(&mut self, page: u32) -> Result<(u32, usize), S::Error> {
        // Find or make a frame slot without reading from the device.
        let idx = if let Some(i) = self.find_frame(page) {
            i
        } else if let Some(i) = self.frames.iter().position(|f| f.page == 0) {
            i
        } else {
            // Evict a clean frame (dirty frames are never evicted —
            // spill is deferred, SPEC §5.2).
            match self.frames.iter().position(|f| !f.dirty) {
                Some(i) => i,
                None => return Err(Error::CacheExhausted),
            }
        };
        for b in self.frames[idx].data.iter_mut() {
            *b = 0;
        }
        self.frames[idx].page = page;
        self.frames[idx].dirty = true;
        self.frames[idx].clock_ref = true;
        if !self.dirty.contains(&page) {
            self.dirty.push(page);
        }
        Ok((page, idx))
    }

    /// Finalise page 1's header fields inside its frame (change
    /// counter incremented, size, freelist, schema cookie,
    /// version-valid-for). Called once at commit start; page 1's
    /// original is journaled first by the usual path.
    async fn finalise_page1(&mut self) -> Result<(), S::Error> {
        let idx = self.dirty_page(1).await?;
        let new_counter = self.header.change_counter + 1;
        self.header.change_counter = new_counter;
        let d = &mut self.frames[idx].data;
        put_be_u32(d, 24, new_counter);
        put_be_u32(d, 28, self.header.page_count);
        put_be_u32(d, 32, self.header.freelist_trunk);
        put_be_u32(d, 36, self.header.freelist_count);
        put_be_u32(d, 40, self.header.schema_cookie);
        put_be_u32(d, 92, new_counter);
        put_be_u32(d, 96, PIN_VERSION_NUMBER);
        Ok(())
    }

    pub fn bump_schema_cookie(&mut self) {
        self.header.schema_cookie = self.header.schema_cookie.wrapping_add(1);
    }

    /// Drive the commit stage machine as far as the device allows.
    /// Every stage is idempotent, and the stage + next index live in
    /// the pager — dropping the driving future anywhere loses
    /// nothing (SPEC §5.1.1/§5.3). Also callable as `resume_txn`.
    pub async fn drive_commit(&mut self) -> Result<(), S::Error> {
        match self.stage {
            CommitStage::Idle => return Ok(()),
            CommitStage::Writing => {
                self.finalise_page1().await?;
                self.stage = CommitStage::SyncJournal;
            }
            _ => {}
        }
        loop {
            match self.stage {
                CommitStage::SyncJournal => {
                    let sidecar = self.sidecar.as_mut().expect("sidecar from open");
                    sidecar.sync().await.map_err(Error::Storage)?;
                    self.stage = CommitStage::WritePages { next: 0 };
                }
                CommitStage::WritePages { next } => {
                    if next >= self.dirty.len() {
                        self.stage = CommitStage::SyncData;
                        continue;
                    }
                    let page = self.dirty[next];
                    let idx = self.find_frame(page).ok_or(Error::CacheExhausted)?;
                    let ps = self.header.page_size as u64;
                    let off = u64::from(page - 1) * ps;
                    // Disjoint field borrows again: the frame image
                    // is written straight from the pool (SPEC §5.2).
                    self.storage
                        .write_at(off, &self.frames[idx].data)
                        .await
                        .map_err(Error::Storage)?;
                    self.stage = CommitStage::WritePages { next: next + 1 };
                }
                CommitStage::SyncData => {
                    let want = u64::from(self.header.page_count) * u64::from(self.header.page_size);
                    let cur = self.storage.len().await.map_err(Error::Storage)?;
                    if cur != want {
                        self.storage.set_len(want).await.map_err(Error::Storage)?;
                    }
                    self.storage.sync().await.map_err(Error::Storage)?;
                    self.stage = CommitStage::DeleteJournal;
                }
                CommitStage::DeleteJournal => {
                    let sidecar = self.sidecar.as_mut().expect("sidecar from open");
                    sidecar.delete().await.map_err(Error::Storage)?;
                    // Commit point passed. Clean up.
                    for f in self.frames.iter_mut() {
                        f.dirty = false;
                    }
                    self.dirty.clear();
                    self.journaled.clear();
                    self.journal_len = 0;
                    self.stage = CommitStage::Idle;
                    self.txn_arena.reset();
                    return Ok(());
                }
                CommitStage::Idle | CommitStage::Writing => unreachable!(),
            }
        }
    }

    /// Roll back the open transaction (SPEC §5.3.3). If no data page
    /// has been written yet this is pure bookkeeping; if commit had
    /// begun, the journal is replayed from the sidecar — the same
    /// code path open-recovery uses.
    pub async fn rollback(&mut self) -> Result<(), S::Error> {
        match self.stage {
            CommitStage::Idle => return Ok(()),
            CommitStage::Writing => {
                let sidecar = self.sidecar.as_mut().expect("sidecar from open");
                sidecar.delete().await.map_err(Error::Storage)?;
            }
            _ => {
                // Commit had begun: replay the journal over whatever
                // data pages landed, truncate, sync, delete.
                let mut sc = self.sidecar.take().expect("sidecar from open");
                let jlen = sc.len().await.map_err(Error::Storage)?;
                if jlen > 0 {
                    replay_journal(&mut self.storage, &mut sc, jlen).await?;
                } else {
                    sc.delete().await.map_err(Error::Storage)?;
                }
                self.sidecar = Some(sc);
                // Restore header counters from the rolled-back page 1.
                let mut probe = [0u8; 100];
                let fl = self.storage.len().await.map_err(Error::Storage)?;
                self.storage
                    .read_at(0, &mut probe)
                    .await
                    .map_err(Error::Storage)?;
                self.header = parse_db_header::<S::Error>(&probe, fl)?;
            }
        }
        for f in self.frames.iter_mut() {
            f.page = 0;
            f.dirty = false;
        }
        self.dirty.clear();
        self.journaled.clear();
        self.journal_len = 0;
        self.stage = CommitStage::Idle;
        self.txn_arena.reset();
        Ok(())
    }

    /// Page pool bytes (for §4.1 accounting by the session layer).
    pub fn pool_bytes(&self) -> usize {
        self.cache_cap * self.header.page_size as usize
    }
}

/// Replay a hot rollback journal over the main file (SPEC §5.3.4),
/// then truncate to the journal's recorded original size, sync, and
/// delete the journal. Returns whether a valid journal was found.
/// Segmented journals (page-count > 0 with repeated headers) are
/// handled; hipp's own journals are single-segment (count = -1).
async fn replay_journal<S: Storage>(
    storage: &mut S,
    sidecar: &mut S::Sidecar,
    jlen: u64,
) -> Result<bool, S::Error> {
    let mut hdr = [0u8; 28];
    if jlen < u64::from(SECTOR_SIZE) {
        // Too short to hold a header sector: not a hot journal.
        sidecar.delete().await.map_err(Error::Storage)?;
        return Ok(false);
    }
    sidecar.read_at(0, &mut hdr).await.map_err(Error::Storage)?;
    if hdr[0..8] != JOURNAL_MAGIC {
        // Not hot (SQLite commits can also be header-zeroing).
        sidecar.delete().await.map_err(Error::Storage)?;
        return Ok(false);
    }
    let mut seg_count = be_u32(&hdr, 8) as i64;
    let mut nonce = be_u32(&hdr, 12);
    let orig_pages = be_u32(&hdr, 16);
    let sector = u64::from(be_u32(&hdr, 20)).max(1);
    let page_size = be_u32(&hdr, 24) as usize;
    if page_size == 0 {
        return Err(Error::CorruptJournal);
    }
    let mut off = sector;
    let rec_len = 8 + page_size as u64;
    loop {
        if seg_count == 0 {
            // Segment exhausted: align to the next sector and read a
            // fresh header (same page/sector sizes, new nonce/count).
            off = off.div_ceil(sector) * sector;
            if off + 28 > jlen {
                break;
            }
            sidecar
                .read_at(off, &mut hdr)
                .await
                .map_err(Error::Storage)?;
            if hdr[0..8] != JOURNAL_MAGIC {
                break;
            }
            seg_count = be_u32(&hdr, 8) as i64;
            nonce = be_u32(&hdr, 12);
            off += sector;
            continue;
        }
        if off + rec_len > jlen {
            break; // incomplete tail record — the rest is stale
        }
        let mut tag = [0u8; 4];
        sidecar
            .read_at(off, &mut tag)
            .await
            .map_err(Error::Storage)?;
        let page = be_u32(&tag, 0);
        let mut img = alloc::vec![0u8; page_size];
        sidecar
            .read_at(off + 4, &mut img)
            .await
            .map_err(Error::Storage)?;
        let mut ck = [0u8; 4];
        sidecar
            .read_at(off + 4 + page_size as u64, &mut ck)
            .await
            .map_err(Error::Storage)?;
        let stored = be_u32(&ck, 0);
        if stored != journal_checksum(nonce, &img) {
            break; // stale/torn record — stop, as SQLite does
        }
        if page == 0 || page > orig_pages {
            break;
        }
        storage
            .write_at(u64::from(page - 1) * page_size as u64, &img)
            .await
            .map_err(Error::Storage)?;
        off += rec_len;
        if seg_count > 0 {
            seg_count -= 1;
        }
    }
    storage
        .set_len(u64::from(orig_pages) * page_size as u64)
        .await
        .map_err(Error::Storage)?;
    storage.sync().await.map_err(Error::Storage)?;
    sidecar.delete().await.map_err(Error::Storage)?;
    Ok(true)
}
