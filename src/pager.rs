//! Pager: page cache, database header, rollback-journal protocol,
//! and the resumable commit stage machine (SPEC §5.2, §5.3).
//!
//! All page access goes through the pager; nothing above this layer
//! touches `Storage`. Dirty-page images live in the fixed page pool
//! (frames), never in an arena (SPEC §7.5.5).

use alloc::vec::Vec;

#[cfg(feature = "v16")]
use crate::arena::AVec;
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
    /// The user version (offset 60, v0.16 `v16`: `PRAGMA
    /// user_version`).
    #[cfg(feature = "v16")]
    pub user_version: u32,
    /// The application id (offset 68, v0.16 `v16`).
    #[cfg(feature = "v16")]
    pub application_id: u32,
    /// The auto-vacuum mode (v0.16 `v16`): 0 none, 1 full, 2
    /// incremental (offsets 52 and 64).
    #[cfg(feature = "v16")]
    pub auto_vacuum: u8,
    /// The text encoding (offset 56; v0.16, `utf16`). A field still
    /// 0 reads as UTF-8, as on the pin.
    #[cfg(feature = "utf16")]
    pub text_enc: crate::record::TextEnc,
    /// The encoding field is still 0 (v0.16, `utf16`): the pin's
    /// file after only header writes, whose encoding `PRAGMA
    /// encoding` may still choose.
    #[cfg(feature = "utf16")]
    pub enc_unset: bool,
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
    // Write version at 18, read version at 19; 2 means WAL (read
    // through the log with the `wal` leaf, v0.16; refused without
    // it, never read stale).
    #[cfg(not(feature = "wal"))]
    if probe[18] == 2 || probe[19] == 2 {
        return Err(Error::WalUnsupported);
    }
    if probe[21] != 64 || probe[22] != 32 || probe[23] != 32 {
        return Err(Error::CorruptPage { page: 1 });
    }
    let encoding = be_u32(probe, 56);
    // v0.16 (`v16`): the pin reads the encoding's low two bits: 0 —
    // not set yet, as in its file after only a header write
    // (`PRAGMA user_version = N` on a new file) — and 1 are UTF-8.
    #[cfg(feature = "v16")]
    let encoding = match encoding & 3 {
        0 => 1,
        e => e,
    };
    #[cfg(feature = "utf16")]
    let text_enc = match encoding {
        2 => crate::record::TextEnc::Utf16le,
        3 => crate::record::TextEnc::Utf16be,
        _ => crate::record::TextEnc::Utf8,
    };
    #[cfg(not(feature = "utf16"))]
    if encoding == 2 || encoding == 3 {
        return Err(Error::UnsupportedEncoding);
    }
    #[cfg(not(feature = "utf16"))]
    if encoding != 1 {
        return Err(Error::CorruptPage { page: 1 });
    }
    #[cfg(feature = "utf16")]
    if !(1..=3).contains(&encoding) {
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
        #[cfg(feature = "v16")]
        user_version: be_u32(probe, 60),
        #[cfg(feature = "v16")]
        application_id: be_u32(probe, 68),
        #[cfg(feature = "v16")]
        auto_vacuum: if be_u32(probe, 52) == 0 {
            0
        } else if be_u32(probe, 64) != 0 {
            2
        } else {
            1
        },
        #[cfg(feature = "utf16")]
        text_enc,
        #[cfg(feature = "utf16")]
        enc_unset: be_u32(probe, 56) == 0,
    })
}

/// The header's offset-56 value for `enc` (v0.16, `utf16`).
#[cfg(feature = "utf16")]
pub fn enc_code(enc: crate::record::TextEnc) -> u32 {
    match enc {
        crate::record::TextEnc::Utf8 => 1,
        crate::record::TextEnc::Utf16le => 2,
        crate::record::TextEnc::Utf16be => 3,
    }
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
    // Schema format 4, UTF-8. With `utf16` (v0.16) both stay 0 as
    // in the pin's new file, until the first schema change sets
    // them (`commit`): a file that only got header writes (`PRAGMA
    // user_version`) may still choose its encoding.
    #[cfg(not(feature = "utf16"))]
    {
        put_be_u32(&mut p, 44, 4);
        put_be_u32(&mut p, 56, 1);
    }
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
    /// WAL-mode commit (v0.16, `wal`): appending dirty page `next`
    /// of the (page-sorted) dirty list as a frame; the last one is
    /// the commit frame — the commit point once it is on the file.
    #[cfg(feature = "wal")]
    WalFrames { next: usize },
    /// WAL-mode commit: syncing the log (`synchronous` FULL or
    /// EXTRA), then publishing the frames.
    #[cfg(feature = "wal")]
    WalSync,
}

/// A `PRAGMA wal_checkpoint` mode (v0.16, `wal`). hipp is the only
/// process on the file, so no checkpoint ever waits on a reader:
/// PASSIVE, FULL and RESTART all copy every committed frame (the
/// next write then restarts the log); TRUNCATE also empties the
/// `-wal` file.
#[cfg(feature = "wal")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    Passive,
    Full,
    Restart,
    Truncate,
}

/// The WAL-related settings of one database (v0.16, `wal`).
#[cfg(feature = "wal")]
#[derive(Debug, Clone, Copy)]
pub struct WalCfg {
    /// The pin's safety level (`PRAGMA synchronous` + 1): 1 OFF (no
    /// syncs), 2 NORMAL (the log synced only by checkpoints), 3 FULL
    /// (the default; the log synced at every commit), 4 and above
    /// EXTRA (as FULL).
    pub sync: u8,
    /// Checkpoint after a commit leaves this many frames in the log
    /// (`PRAGMA wal_autocheckpoint`; 0: never).
    pub autocheckpoint: u32,
    /// `PRAGMA journal_size_limit` in bytes (-1: none).
    pub size_limit: i64,
}

#[cfg(feature = "wal")]
impl Default for WalCfg {
    fn default() -> WalCfg {
        WalCfg {
            sync: 3,
            autocheckpoint: crate::wal::DEFAULT_AUTOCHECKPOINT,
            size_limit: -1,
        }
    }
}

struct Frame {
    page: u32,
    data: Vec<u8>,
    dirty: bool,
    clock_ref: bool,
}

/// One savepoint level (v0.16, `v16`): a named `SAVEPOINT`, or
/// the anonymous statement savepoint a write statement inside a
/// transaction runs under (so a failed statement undoes only
/// itself, as on the pin).
#[cfg(feature = "v16")]
struct SpMark {
    /// The savepoint's name as written (dequoted); `None` for a
    /// statement savepoint.
    name: Option<AVec<u8>>,
    /// The savepoint opened the transaction (`SAVEPOINT` outside
    /// `BEGIN`): releasing it commits.
    starts_txn: bool,
    /// Sub-journal length when the savepoint opened: the records
    /// from here on undo what happened since.
    sub_len: usize,
    /// The header as it stood when the savepoint opened.
    header: DbHeader,
}

/// One sub-journal record (v0.16, `v16`): `page`'s image as it was
/// when it was first changed after the then-newest savepoint, at
/// sidecar offset `off`. A page the transaction had not touched
/// yet points at its rollback-journal record's image; any other
/// was copied into the journal file's savepoint area
/// (`Pager::sp_area`), past the largest rollback journal the
/// transaction can write, so recovery never reaches it (it stops
/// at the zero record in between, as the pin's does).
#[cfg(feature = "v16")]
struct SubRec {
    page: u32,
    off: u64,
}

pub struct Pager<S: Storage> {
    storage: S,
    header: DbHeader,
    reserved_bytes: u8,
    /// The file is an auto-vacuum database (header offset 52
    /// non-zero; v0.16 review fix, ungated): it carries pointer-map
    /// pages hipp does not maintain, so every write is refused
    /// (`Error::AutoVacuumUnsupported`) — before, writes left the
    /// maps stale and the pin then found the file corrupt.
    ptrmaps: bool,
    frames: Vec<Frame>,
    cache_cap: usize,
    sidecar: Option<S::Sidecar>,
    stage: CommitStage,
    /// A dirty frame was spilled to the main file mid-transaction
    /// to free a cache slot (v0.5; see `spill_one`). While set, a
    /// rollback must replay the journal — the cheap
    /// nothing-has-moved path no longer holds.
    spilled: bool,
    /// Header snapshot taken when the write txn opens (v0.2): a
    /// rollback from the `Writing` stage restores schema cookie,
    /// freelist and page count in memory too, not just via the
    /// journal image on disk. (The commit-begun rollback path
    /// re-reads the header from page 1 instead.)
    txn_header: Option<DbHeader>,
    orig_page_count: u32,
    dirty: Vec<u32>,
    journaled: Vec<u32>,
    journal_len: u64,
    nonce: u32,
    nonce_counter: u64,
    nonce_seed: u64,
    pub txn_arena: Arena,
    /// This open created the file (v0.16, `v16`): until its first
    /// commit the database is "empty" in the pin's sense, so
    /// `PRAGMA page_size` may still change.
    #[cfg(feature = "v16")]
    created: bool,
    /// A write transaction has begun since the open (v0.16, `v16`):
    /// the pin's `BTS_PAGESIZE_FIXED`, which a ROLLBACK does not
    /// clear — the page size, encoding and auto-vacuum mode of an
    /// empty database are fixed from then on.
    #[cfg(feature = "v16")]
    layout_fixed: bool,
    /// A schema change has been made since the open (v0.16, `utf16`;
    /// a ROLLBACK does not clear it): as on the pin, the text
    /// encoding is fixed from then on, though a file that only got
    /// header writes may still choose it.
    #[cfg(feature = "utf16")]
    schema_changed: bool,
    /// Open savepoints, oldest first (v0.16, `v16`).
    #[cfg(feature = "v16")]
    sp: AVec<SpMark>,
    /// The savepoint sub-journal (v0.16, `v16`): page images to
    /// restore on `ROLLBACK TO`, in the order they were recorded.
    /// It lives in memory; the rollback journal alone keeps crash
    /// recovery exact (a crash undoes the whole transaction).
    #[cfg(feature = "v16")]
    sub: AVec<SubRec>,
    /// The next free offset of the savepoint area (0: not placed
    /// yet in this transaction; see `SubRec`).
    #[cfg(feature = "v16")]
    sub_off: u64,
    /// The WITHOUT ROWID trees' layouts and the statement's row
    /// handles (v0.16, `without_rowid`; see `crate::wr`).
    #[cfg(feature = "without_rowid")]
    pub wr: crate::wr::WrState,
    /// The write-ahead log, while the database is in WAL mode
    /// (v0.16, `wal`).
    #[cfg(feature = "wal")]
    wal: Option<crate::wal::Wal<S::Sidecar>>,
    /// The WAL settings (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub wal_cfg: WalCfg,
    /// In WAL mode the savepoint area lives in the journal file
    /// (v0.16, `wal`): set once this transaction wrote there, so the
    /// file is deleted when it ends.
    #[cfg(feature = "wal")]
    sp_file: bool,
    /// A page has been read since `clear_touched` (v0.16, `wal`:
    /// the pin refuses a checkpoint inside a transaction that has
    /// read the database).
    #[cfg(feature = "wal")]
    touched: bool,
    /// WAL mode was entered and no page has been read since (v0.16,
    /// `wal`): the pin has not opened its log yet, and its first
    /// checkpoint opens it with a read transaction.
    #[cfg(feature = "wal")]
    wal_fresh: bool,
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
        // Without `wal`, a non-empty log beside the file is refused
        // whatever page 1 says (v0.16 review fix: it was read stale,
        // and written under the log's frames): the pin opens such a
        // database in WAL mode, so its committed pages may be in the
        // log (`Storage::log_beside`).
        #[cfg(not(feature = "wal"))]
        if storage.log_beside() {
            return Err(Error::WalUnsupported);
        }
        // A log beside an empty database belongs to no database: the
        // pin deletes it (v0.16, `wal`).
        #[cfg(feature = "wal")]
        if file_len == 0 {
            let mut w = storage
                .sidecar(Sidecar::Wal)
                .await
                .map_err(Error::Storage)?;
            if w.existed() {
                w.delete().await.map_err(Error::Storage)?;
            }
        }
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
        #[cfg(not(feature = "wal"))]
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
        // WAL mode (v0.16, `wal`): the committed frames of the log
        // replace their pages, page 1's header included.
        #[cfg(feature = "wal")]
        let wal = open_wal(&mut storage, &mut probe, &mut header).await?;
        #[cfg(feature = "wal")]
        let reserved_bytes = probe[20];
        #[cfg(feature = "wal")]
        let in_log = wal.as_ref().is_some_and(|w| w.mx_frame > 0);
        #[cfg(not(feature = "wal"))]
        let in_log = false;
        let ptrmaps = probe[52..56] != [0; 4];
        // A header that claims more pages than the file holds is
        // the pin's "malformed" (v0.16, `v16`: hipp read past the
        // end as empty pages, which integrity_check reported).
        #[cfg(feature = "v16")]
        let fl = storage.len().await.map_err(Error::Storage)?;
        #[cfg(feature = "v16")]
        if !in_log && u64::from(header.page_count) > fl.div_ceil(u64::from(header.page_size)) {
            return Err(Error::CorruptPage { page: 1 });
        }
        let _ = in_log;

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
                ptrmaps,
                frames,
                cache_cap,
                sidecar: Some(sidecar),
                stage: CommitStage::Idle,
                spilled: false,
                txn_header: None,
                orig_page_count: header.page_count,
                dirty: Vec::new(),
                journaled: Vec::new(),
                journal_len: 0,
                nonce: 0,
                nonce_counter: 0,
                nonce_seed,
                txn_arena,
                #[cfg(feature = "v16")]
                created: is_new,
                #[cfg(feature = "v16")]
                layout_fixed: false,
                #[cfg(feature = "utf16")]
                schema_changed: false,
                #[cfg(feature = "v16")]
                sp: AVec::new(),
                #[cfg(feature = "v16")]
                sub: AVec::new(),
                #[cfg(feature = "v16")]
                sub_off: 0,
                #[cfg(feature = "without_rowid")]
                wr: crate::wr::WrState::default(),
                #[cfg(feature = "wal")]
                wal,
                #[cfg(feature = "wal")]
                wal_cfg: WalCfg::default(),
                #[cfg(feature = "wal")]
                sp_file: false,
                #[cfg(feature = "wal")]
                touched: false,
                #[cfg(feature = "wal")]
                wal_fresh: false,
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
        #[cfg(feature = "wal")]
        {
            self.touched = true;
            self.wal_fresh = false;
        }
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
            None => match self.spill_one().await? {
                Some(i) => i,
                None => return Err(Error::CacheExhausted),
            },
        };
        let ps = self.header.page_size as usize;
        let off = u64::from(page - 1) * ps as u64;
        // In WAL mode (v0.16, `wal`) a page with a frame in the log
        // is read from its newest one.
        #[cfg(feature = "wal")]
        let from_log = match self.wal.as_ref() {
            Some(w) => match w.lookup(page) {
                Some(fr) => {
                    w.read_page(fr, &mut self.frames[vi].data).await?;
                    true
                }
                None => false,
            },
            None => false,
        };
        #[cfg(not(feature = "wal"))]
        let from_log = false;
        // Disjoint field borrows (frames vs storage): the read lands
        // directly in the frame — no copy, no allocation (SPEC §5.2).
        if !from_log {
            self.storage
                .read_at(off, &mut self.frames[vi].data)
                .await
                .map_err(Error::Storage)?;
        }
        self.frames[vi].page = page;
        self.frames[vi].dirty = false;
        self.frames[vi].clock_ref = true;
        Ok(vi)
    }

    /// Spill one dirty frame to the main file to free a cache
    /// slot (SPEC §5.2's spill, landed in v0.5): the page's
    /// original is already in the rollback journal, so writing
    /// its current image early is exactly SQLite's protocol —
    /// a crash is recovered from the journal, a rollback replays
    /// it, and commit writes the final image regardless. The
    /// page stays in the dirty list; only the frame is released.
    /// Returns the freed frame index, or None when no frame is
    /// dirty (the cache is genuinely exhausted).
    async fn spill_one(&mut self) -> Result<Option<usize>, S::Error> {
        let Some(i) = self.frames.iter().position(|f| f.dirty) else {
            return Ok(None);
        };
        let page = self.frames[i].page;
        // In WAL mode (v0.16, `wal`) the page goes to the log as a
        // frame of the open transaction (no commit marker), as the
        // pin spills: the database file never changes outside a
        // checkpoint.
        #[cfg(feature = "wal")]
        if self.wal.is_some() {
            let rand = self.rand64();
            let sync = self.wal_cfg.sync >= 3;
            if let Some(w) = self.wal.as_mut() {
                w.append(page, &self.frames[i].data, 0, rand, sync).await?;
            }
            self.frames[i].dirty = false;
            return Ok(Some(i));
        }
        let off = u64::from(page - 1) * u64::from(self.header.page_size);
        self.storage
            .write_at(off, &self.frames[i].data)
            .await
            .map_err(Error::Storage)?;
        self.frames[i].dirty = false;
        self.spilled = true;
        Ok(Some(i))
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
        if self.ptrmaps {
            return Err(Error::AutoVacuumUnsupported);
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
        #[cfg(feature = "v16")]
        {
            self.layout_fixed = true;
        }
        // Fresh nonce per transaction (SPEC §5.3; R2's entropy note:
        // the seed is Config-supplied; targets with real entropy
        // should seed it — a counter mix is the documented floor).
        self.nonce_counter += 1;
        let mut x = self.nonce_seed ^ self.nonce_counter.wrapping_mul(0x9e37_79b1_85eb_ca87);
        x ^= x >> 33;
        self.nonce = (x as u32) | 1;
        self.orig_page_count = self.header.page_count;
        // WAL mode (v0.16, `wal`): no rollback journal — the log's
        // frames past the last commit frame are the transaction.
        #[cfg(feature = "wal")]
        if self.wal.is_some() {
            self.txn_header = Some(self.header);
            self.stage = CommitStage::Writing;
            return Ok(());
        }
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
        self.txn_header = Some(self.header);
        self.stage = CommitStage::Writing;
        Ok(())
    }

    /// Ensure `page`'s original image is in the journal (appending a
    /// record if this transaction has not journaled it yet).
    async fn journal_original(&mut self, page: u32, frame_idx: usize) -> Result<(), S::Error> {
        // A page past the original size has no original: recovery
        // truncates it away. (v0.16 fix, ungated: a page this
        // transaction allocated, spilled and then changed again
        // was journaled, and replay stops at such a record — the
        // records after it were lost to a crash.)
        if self.journaled.contains(&page) || page > self.orig_page_count {
            return Ok(());
        }
        // WAL mode keeps no rollback journal (v0.16, `wal`).
        #[cfg(feature = "wal")]
        if self.wal.is_some() {
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
        #[cfg(feature = "v16")]
        self.sp_note(page, idx).await?;
        if !self.frames[idx].dirty {
            self.journal_original(page, idx).await?;
            self.frames[idx].dirty = true;
            self.dirty.push(page);
        }
        Ok(idx)
    }

    /// Maximum leaf page numbers one freelist trunk page holds
    /// (SQFMT §1.3.8): the array starts at offset 8, 4 bytes each.
    fn trunk_capacity(&self) -> u32 {
        (self.usable() - 8) / 4
    }

    /// Allocate a page: pop the freelist first (consuming foreign
    /// freelist pages correctly), else grow the file (SPEC §5.4).
    /// The returned frame is zeroed, dirty, and NOT journaled (a new
    /// page has no original; rollback truncates it away).
    ///
    /// Freelist hardening (v0.3): every value read out of a trunk
    /// page is untrusted input (RISKS R1/R8) — the leaf count is
    /// bounded by the trunk capacity, a leaf may not name the trunk
    /// itself, page 1, an out-of-range page, or a duplicate of
    /// another leaf in the same trunk (a duplicate would hand one
    /// page out twice and silently corrupt two B-trees), and a
    /// trunk chain link being followed is validated end to end
    /// (range, no cycles, per-trunk counts) before it is trusted.
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
            if leaf_count > self.trunk_capacity() {
                return Err(Error::CorruptPage { page: trunk });
            }
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
                // The leaf must appear exactly once in this trunk's
                // array (see the hardening note above).
                {
                    let d = &self.frames[ti].data;
                    let mut seen = 0usize;
                    for k in 0..leaf_count as usize {
                        if be_u32(d, 8 + 4 * k) == leaf {
                            seen += 1;
                        }
                    }
                    if seen != 1 {
                        return Err(Error::CorruptPage { page: leaf });
                    }
                }
                // Decrement the trunk's leaf count (a journaled,
                // dirtying write to the trunk page).
                let ti = self.dirty_page(trunk).await?;
                put_be_u32(&mut self.frames[ti].data, 4, leaf_count - 1);
                self.header.freelist_count -= 1;
                // A leaf freed earlier in this transaction still
                // holds a page the rolled-back state references:
                // journal it (and note it for savepoints) before
                // reuse. (v0.16 fix, ungated where pages can be
                // freed: a crash after its new image landed tore
                // the restored B-tree.) Leaves past the original
                // size have no original to keep.
                #[cfg(any(feature = "v05", feature = "indexes"))]
                if leaf <= self.orig_page_count || self.dirty.contains(&leaf) {
                    let li = self.dirty_page(leaf).await?;
                    for b in self.frames[li].data.iter_mut() {
                        *b = 0;
                    }
                    return Ok((leaf, li));
                }
                return self.fresh_frame(leaf).await;
            }
            // The trunk page itself becomes the allocated page. The
            // chain link is about to become the header's trunk, so
            // validate the whole remaining chain first (it is the
            // only moment the chain is walked): every link in
            // range, no repeats (a cycle hands pages out forever),
            // every trunk's leaf count within capacity, and the
            // walked page total must not exceed the header's
            // freelist count.
            if next_trunk != 0 {
                if next_trunk < 2 || next_trunk > self.header.page_count || next_trunk == trunk {
                    return Err(Error::CorruptPage { page: next_trunk });
                }
                let mut walked: Vec<u32> = alloc::vec![trunk];
                let mut total = 1u32; // the trunk being consumed
                let mut cur = next_trunk;
                loop {
                    if walked.contains(&cur) {
                        return Err(Error::CorruptPage { page: cur });
                    }
                    walked.push(cur);
                    let ci = self.load(cur).await?;
                    let (nxt, cnt) = {
                        let d = &self.frames[ci].data;
                        (be_u32(d, 0), be_u32(d, 4))
                    };
                    if cnt > self.trunk_capacity() {
                        return Err(Error::CorruptPage { page: cur });
                    }
                    total += 1 + cnt;
                    if total > self.header.freelist_count {
                        return Err(Error::CorruptPage { page: cur });
                    }
                    if nxt == 0 {
                        break;
                    }
                    if nxt < 2 || nxt > self.header.page_count {
                        return Err(Error::CorruptPage { page: nxt });
                    }
                    cur = nxt;
                }
            }
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

    /// Return `page` to the freelist (SQFMT §1.3.8; v0.3 — the
    /// first producer was DROP INDEX; v0.5 adds DELETE / DROP
    /// TABLE). The format's rules, mirrored from observed oracle
    /// behaviour:
    /// - an empty freelist: the page becomes the trunk (next = 0,
    ///   leaf count = 0);
    /// - a trunk with room: the page is appended to its leaf array
    ///   and its own bytes are left untouched — a freelist leaf
    ///   carries no information, and leaving the image intact is
    ///   what makes rollback of the freeing transaction restore
    ///   the page for free (its linkage lives in page 1 + the
    ///   trunk, both journaled);
    /// - a full trunk: the page becomes the new trunk, pointing
    ///   at the old one.
    ///
    /// A page turned into a trunk IS modified (its first 8
    /// bytes), so it goes through `dirty_page` and is journaled
    /// first.
    pub async fn free_page(&mut self, page: u32) -> Result<(), S::Error> {
        if page < 2 || page > self.header.page_count {
            return Err(Error::PageOutOfRange { page });
        }
        self.begin_write().await?;
        if self.header.freelist_count == 0 {
            let idx = self.dirty_page(page).await?;
            let d = &mut self.frames[idx].data;
            put_be_u32(d, 0, 0);
            put_be_u32(d, 4, 0);
            self.header.freelist_trunk = page;
            self.header.freelist_count = 1;
            return Ok(());
        }
        let trunk = self.header.freelist_trunk;
        if trunk < 2 || trunk > self.header.page_count {
            return Err(Error::CorruptPage { page: trunk });
        }
        let ti = self.load(trunk).await?;
        let leaf_count = be_u32(&self.frames[ti].data, 4);
        if leaf_count > self.trunk_capacity() {
            return Err(Error::CorruptPage { page: trunk });
        }
        if leaf_count < self.trunk_capacity() {
            // Append as a leaf of the current trunk.
            let ti = self.dirty_page(trunk).await?;
            put_be_u32(&mut self.frames[ti].data, 8 + 4 * leaf_count as usize, page);
            put_be_u32(&mut self.frames[ti].data, 4, leaf_count + 1);
            self.header.freelist_count += 1;
            return Ok(());
        }
        // Trunk is full: the freed page becomes the new trunk.
        let idx = self.dirty_page(page).await?;
        let d = &mut self.frames[idx].data;
        put_be_u32(d, 0, trunk);
        put_be_u32(d, 4, 0);
        self.header.freelist_trunk = page;
        self.header.freelist_count += 1;
        Ok(())
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
            // Evict a clean frame; if there is none, spill a
            // dirty one (SPEC §5.2 spill, v0.5).
            match self.frames.iter().position(|f| !f.dirty) {
                Some(i) => i,
                None => match self.spill_one().await? {
                    Some(i) => i,
                    None => return Err(Error::CacheExhausted),
                },
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

    /// Find or make a frame slot for `page` without reading from
    /// the device: its own frame, an empty one, a clean one
    /// evicted, else a dirty one spilled (SPEC §5.2 spill, v0.5;
    /// v0.16 `v16`: `sp_rollback`'s restores).
    #[cfg(feature = "v16")]
    async fn frame_slot(&mut self, page: u32) -> Result<usize, S::Error> {
        if let Some(i) = self.find_frame(page) {
            return Ok(i);
        }
        if let Some(i) = self.frames.iter().position(|f| f.page == 0) {
            return Ok(i);
        }
        match self.frames.iter().position(|f| !f.dirty) {
            Some(i) => Ok(i),
            None => match self.spill_one().await? {
                Some(i) => Ok(i),
                None => Err(Error::CacheExhausted),
            },
        }
    }

    /// Finalise page 1's header fields inside its frame (change
    /// counter incremented, size, freelist, schema cookie,
    /// version-valid-for). Called once at commit start; page 1's
    /// original is journaled first by the usual path.
    async fn finalise_page1(&mut self) -> Result<(), S::Error> {
        // In WAL mode (v0.16, `wal`) page 1 is written only when the
        // transaction changed it or a header field, as on the pin
        // (whose change counter moves only then).
        #[cfg(feature = "wal")]
        if self.wal.is_some() && !self.dirty.contains(&1) && self.txn_header == Some(self.header) {
            return Ok(());
        }
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
        // The first schema change on a file whose schema format and
        // encoding are still unset (0) sets them, as the pin's
        // CREATE TABLE / VIEW does (v0.16, `v16`: such a file opens
        // only there).
        #[cfg(feature = "v16")]
        if self
            .txn_header
            .is_some_and(|h| h.schema_cookie != self.header.schema_cookie)
            && be_u32(d, 44) == 0
        {
            put_be_u32(d, 44, 4);
            put_be_u32(d, 56, 1);
        }
        // The encoding `PRAGMA encoding` chose for the empty database
        // lands with its first schema change (v0.16, `utf16`).
        #[cfg(feature = "utf16")]
        if self
            .txn_header
            .is_some_and(|h| h.schema_cookie != self.header.schema_cookie)
        {
            put_be_u32(d, 56, enc_code(self.header.text_enc));
            self.header.enc_unset = false;
        }
        Ok(())
    }

    /// The parsed database header (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub fn header(&self) -> &DbHeader {
        &self.header
    }

    /// The database's text encoding (v0.16; always UTF-8 without
    /// `utf16`).
    #[inline]
    pub fn text_enc(&self) -> crate::record::TextEnc {
        #[cfg(feature = "utf16")]
        return self.header.text_enc;
        #[cfg(not(feature = "utf16"))]
        crate::record::TextEnc::Utf8
    }

    /// Choose the text encoding of an empty database (v0.16,
    /// `utf16`: `PRAGMA encoding = ...` while the encoding is not
    /// fixed); the header field is written with the first schema
    /// change. A TEMP database's pager takes its main database's
    /// encoding through this too.
    #[cfg(feature = "utf16")]
    pub fn set_text_enc(&mut self, enc: crate::record::TextEnc) {
        self.header.text_enc = enc;
        if let Some(h) = self.txn_header.as_mut() {
            h.text_enc = enc;
        }
        for m in self.sp.as_mut_slice().iter_mut() {
            m.header.text_enc = enc;
        }
    }

    /// The file is as this open created it (v0.16, `v16`): no
    /// transaction has committed and none is open — the pin's
    /// empty database, whose page size, encoding and auto-vacuum
    /// mode are not fixed yet.
    #[cfg(feature = "v16")]
    pub fn pristine(&self) -> bool {
        self.created
            && self.header.change_counter == 1
            && self.header.page_count == 1
            && self.stage == CommitStage::Idle
    }

    /// The empty database's layout (page size, encoding,
    /// auto-vacuum mode) may still change (v0.16, `v16`): it is
    /// `pristine` and no write transaction has begun since the open
    /// (`BEGIN IMMEDIATE` counts; see `fix_layout`).
    #[cfg(feature = "v16")]
    pub fn layout_open(&self) -> bool {
        self.pristine() && !self.layout_fixed
    }

    /// Fix the layout as a write transaction's start does (v0.16,
    /// `v16`: `BEGIN IMMEDIATE` / `EXCLUSIVE`, which take the
    /// pin's write lock at once).
    #[cfg(feature = "v16")]
    pub fn fix_layout(&mut self) {
        self.layout_fixed = true;
    }

    /// Write a 32-bit header field of page 1 inside the current
    /// write transaction (v0.16, `v16`: `PRAGMA user_version` /
    /// `application_id`), opening one if needed. Offsets 60 and
    /// 68 also update the parsed header.
    #[cfg(feature = "v16")]
    pub async fn set_header_u32(&mut self, off: usize, v: u32) -> Result<(), S::Error> {
        let idx = self.dirty_page(1).await?;
        let d = &mut self.frames[idx].data;
        if d.len() < off + 4 {
            return Err(Error::CorruptPage { page: 1 });
        }
        put_be_u32(d, off, v);
        match off {
            60 => self.header.user_version = v,
            68 => self.header.application_id = v,
            _ => {}
        }
        Ok(())
    }

    /// Re-create a pristine file with another page size (v0.16,
    /// `v16`: `PRAGMA page_size` before the first write). The
    /// caller checks `pristine` and the size.
    #[cfg(feature = "v16")]
    pub async fn reformat(&mut self, page_size: u32) -> Result<(), S::Error> {
        if !self.layout_open() {
            return Err(Error::Unsupported);
        }
        let img = initial_page1(page_size);
        self.storage
            .set_len(u64::from(page_size))
            .await
            .map_err(Error::Storage)?;
        self.storage
            .write_at(0, &img)
            .await
            .map_err(Error::Storage)?;
        self.storage.sync().await.map_err(Error::Storage)?;
        #[cfg(feature = "utf16")]
        let enc = self.header.text_enc;
        self.header = parse_db_header::<S::Error>(&img, u64::from(page_size))?;
        #[cfg(feature = "utf16")]
        {
            self.header.text_enc = enc;
        }
        self.orig_page_count = self.header.page_count;
        // The new page size is not transactional (the pin keeps it
        // across ROLLBACK TO): an open savepoint returns to the
        // reformatted empty file, not to the old page size.
        for m in self.sp.as_mut_slice().iter_mut() {
            m.header = self.header;
        }
        for f in self.frames.iter_mut() {
            f.page = 0;
            f.dirty = false;
            f.data = alloc::vec![0u8; page_size as usize];
        }
        Ok(())
    }

    pub fn bump_schema_cookie(&mut self) {
        self.header.schema_cookie = self.header.schema_cookie.wrapping_add(1);
        #[cfg(feature = "utf16")]
        {
            self.schema_changed = true;
        }
    }

    /// A schema change has been made since the open, even one rolled
    /// back (v0.16, `utf16`).
    #[cfg(feature = "utf16")]
    pub fn schema_changed(&self) -> bool {
        self.schema_changed
    }

    /// The text encoding may still be chosen (v0.16, `utf16`; the
    /// pin's encoding is not fixed yet): no schema change since the
    /// open, and the file is as created or its encoding field is
    /// still 0 (only header writes so far). The caller also checks
    /// that the database has no schema.
    #[cfg(feature = "utf16")]
    pub fn enc_open(&self) -> bool {
        !self.schema_changed && (self.layout_open() || self.header.enc_unset)
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
                // WAL mode (v0.16, `wal`): frames in page order, as
                // the pin writes them.
                // A page spilled to the log and not changed since is
                // there already; with none left in the cache the
                // commit frame is page 1's (the pin's rule).
                #[cfg(feature = "wal")]
                if let Some(w) = self.wal.as_ref() {
                    let frames = &self.frames;
                    self.dirty
                        .retain(|p| frames.iter().any(|f| f.page == *p && f.dirty));
                    if self.dirty.is_empty() && w.tx_frames() {
                        self.dirty.push(1);
                    }
                    self.dirty.sort_unstable();
                    self.stage = CommitStage::WalFrames { next: 0 };
                }
            }
            _ => {}
        }
        loop {
            match self.stage {
                #[cfg(feature = "wal")]
                CommitStage::WalFrames { next } => {
                    let Some(&page) = self.dirty.get(next) else {
                        self.stage = CommitStage::WalSync;
                        continue;
                    };
                    let idx = self.load(page).await?;
                    let commit = if next + 1 == self.dirty.len() {
                        self.header.page_count
                    } else {
                        0
                    };
                    let rand = self.rand64();
                    let sync = self.wal_cfg.sync >= 3;
                    let Some(w) = self.wal.as_mut() else {
                        return Err(Error::OperationInFlight);
                    };
                    w.append(page, &self.frames[idx].data, commit, rand, sync)
                        .await?;
                    self.stage = CommitStage::WalFrames { next: next + 1 };
                }
                #[cfg(feature = "wal")]
                CommitStage::WalSync => {
                    let (sync, limit) = (self.wal_cfg.sync >= 3, self.wal_cfg.size_limit);
                    let n_page = self.header.page_count;
                    let Some(w) = self.wal.as_mut() else {
                        return Err(Error::OperationInFlight);
                    };
                    if w.tx_frames() {
                        if sync {
                            w.sync().await?;
                        }
                        w.committed(n_page, limit).await;
                    }
                    // Commit point passed. Clean up.
                    self.end_txn().await;
                    self.auto_checkpoint().await;
                    return Ok(());
                }
                CommitStage::SyncJournal => {
                    let sidecar = self.sidecar.as_mut().expect("sidecar from open");
                    // `PRAGMA synchronous = OFF` skips every sync
                    // (v0.16, `wal`).
                    #[cfg(feature = "wal")]
                    let skip = self.wal_cfg.sync < 2;
                    #[cfg(not(feature = "wal"))]
                    let skip = false;
                    if !skip {
                        sidecar.sync().await.map_err(Error::Storage)?;
                    }
                    self.stage = CommitStage::WritePages { next: 0 };
                }
                CommitStage::WritePages { next } => {
                    if next >= self.dirty.len() {
                        self.stage = CommitStage::SyncData;
                        continue;
                    }
                    let page = self.dirty[next];
                    // The page may have been spilled out of the
                    // cache mid-transaction; load restores it —
                    // its on-disk image is current (the spill
                    // wrote it, and nothing else writes the main
                    // file during a transaction).
                    let idx = self.load(page).await?;
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
                    #[cfg(feature = "wal")]
                    let skip = self.wal_cfg.sync < 2;
                    #[cfg(not(feature = "wal"))]
                    let skip = false;
                    if !skip {
                        self.storage.sync().await.map_err(Error::Storage)?;
                    }
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
                    self.spilled = false;
                    self.txn_header = None;
                    self.stage = CommitStage::Idle;
                    self.txn_arena.reset();
                    #[cfg(feature = "v16")]
                    self.sp_clear();
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
        #[cfg(feature = "v16")]
        self.sp_clear();
        match self.stage {
            CommitStage::Idle => return Ok(()),
            // WAL mode (v0.16, `wal`): nothing reached the database
            // file; the transaction's frames are forgotten (a commit
            // frame already written, when the commit failed at its
            // sync, is voided first).
            #[cfg(feature = "wal")]
            stage if self.wal.is_some() => {
                if let Some(w) = self.wal.as_mut() {
                    w.rollback(stage == CommitStage::WalSync).await;
                }
                if let Some(h) = self.txn_header.take() {
                    self.header = h;
                }
                self.drop_sp_file().await;
            }
            CommitStage::Writing if !self.spilled => {
                let sidecar = self.sidecar.as_mut().expect("sidecar from open");
                sidecar.delete().await.map_err(Error::Storage)?;
                // No data page has moved: restore the in-memory
                // header (schema cookie / freelist / page count may
                // have been mutated by the aborted txn).
                if let Some(h) = self.txn_header.take() {
                    self.header = h;
                }
            }
            _ => {
                // Commit had begun: replay the journal over whatever
                // data pages landed, truncate, sync, delete.
                // Borrowed in place (v0.16 fix, ungated: the sidecar
                // was taken out and lost when the replay failed, so
                // the next transaction panicked).
                let sc = self.sidecar.as_mut().expect("sidecar from open");
                let jlen = sc.len().await.map_err(Error::Storage)?;
                if jlen > 0 {
                    replay_journal(&mut self.storage, sc, jlen).await?;
                } else {
                    sc.delete().await.map_err(Error::Storage)?;
                }
                // Restore header counters from the rolled-back page 1.
                let mut probe = [0u8; 100];
                let fl = self.storage.len().await.map_err(Error::Storage)?;
                self.storage
                    .read_at(0, &mut probe)
                    .await
                    .map_err(Error::Storage)?;
                self.header = parse_db_header::<S::Error>(&probe, fl)?;
                self.txn_header = None;
            }
        }
        for f in self.frames.iter_mut() {
            f.page = 0;
            f.dirty = false;
        }
        self.dirty.clear();
        self.journaled.clear();
        self.journal_len = 0;
        self.spilled = false;
        self.stage = CommitStage::Idle;
        self.txn_arena.reset();
        Ok(())
    }

    /// Record `page`'s current image in the sub-journal before it
    /// changes, when a savepoint is open and the page has not been
    /// recorded since the newest one opened (v0.16, `v16`; called
    /// with the page loaded at frame `idx`, inside the write
    /// transaction). A page the transaction has not touched is
    /// about to get its rollback-journal record, which already
    /// holds that image; any other is copied to the savepoint area
    /// of the journal file, so no page image is held in RAM. Pages
    /// past the newest savepoint's size need nothing: rolling back
    /// to it truncates them away.
    #[cfg(feature = "v16")]
    async fn sp_note(&mut self, page: u32, idx: usize) -> Result<(), S::Error> {
        let Some(top) = self.sp.as_slice().last() else {
            return Ok(());
        };
        if page > top.header.page_count {
            return Ok(());
        }
        let since = self.sub.as_slice().get(top.sub_len..).unwrap_or(&[]);
        if since.iter().any(|r| r.page == page) {
            return Ok(());
        }
        // WAL mode (v0.16, `wal`) keeps no rollback journal: every
        // image is copied to the savepoint area, which starts past a
        // zero first sector (a hot-journal check never takes it for
        // a journal).
        #[cfg(feature = "wal")]
        let copy = self.wal.is_some();
        #[cfg(not(feature = "wal"))]
        let copy = false;
        let off = if copy || self.dirty.contains(&page) || page > self.orig_page_count {
            let ps = u64::from(self.header.page_size);
            #[cfg(feature = "wal")]
            if copy && self.sub_off == 0 {
                self.sub_off = u64::from(SECTOR_SIZE);
                self.sp_file = true;
            }
            if self.sub_off == 0 {
                // Past the largest rollback journal this transaction
                // can write (every original page once), plus one
                // zero record where recovery stops.
                self.sub_off = u64::from(SECTOR_SIZE)
                    + (u64::from(self.orig_page_count) + 1).saturating_mul(ps + 8);
            }
            let off = self.sub_off;
            let Some(sc) = self.sidecar.as_mut() else {
                return Err(Error::OperationInFlight);
            };
            sc.write_at(off, &self.frames[idx].data)
                .await
                .map_err(Error::Storage)?;
            // Four zero bytes after each image: the journal file
            // never ends in the journal magic, which the pin would
            // read as a super-journal name.
            sc.write_at(off + ps, &[0u8; 4])
                .await
                .map_err(Error::Storage)?;
            self.sub_off = off + ps + 4;
            off
        } else {
            // `journal_original` appends this page's record next:
            // the 4-byte page number, then the image.
            self.journal_len + 4
        };
        self.sub.push(SubRec { page, off });
        Ok(())
    }

    /// Open a savepoint (v0.16, `v16`): `name` for `SAVEPOINT name`
    /// (`starts_txn` when it opened the transaction), `None` for a
    /// statement savepoint. No I/O: the state to return to is the
    /// current header plus the sub-journal from here on.
    #[cfg(feature = "v16")]
    pub fn sp_open(&mut self, name: Option<&[u8]>, starts_txn: bool) {
        let name = name.map(|n| {
            let mut v = AVec::with_capacity(n.len());
            v.extend_from_slice(n);
            v
        });
        self.sp.push(SpMark {
            name,
            starts_txn,
            sub_len: self.sub.len(),
            header: self.header,
        });
    }

    /// Open savepoints, statement savepoints included (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub fn sp_depth(&self) -> usize {
        self.sp.len()
    }

    /// The level of the newest savepoint named `name` (the pin
    /// compares names ignoring ASCII case), if any (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub fn sp_find(&self, name: &[u8]) -> Option<usize> {
        self.sp.as_slice().iter().rposition(|m| {
            m.name
                .as_ref()
                .is_some_and(|n| n.as_slice().eq_ignore_ascii_case(name))
        })
    }

    /// The open savepoints, oldest first (v0.16, `v16`): each one's
    /// name (`None` for a statement savepoint) and whether it
    /// opened the transaction. The TEMP database replays them.
    #[cfg(feature = "v16")]
    pub fn sp_marks(&self) -> AVec<(Option<AVec<u8>>, bool)> {
        let mut out = AVec::new();
        for m in self.sp.iter() {
            out.push((m.name.clone(), m.starts_txn));
        }
        out
    }

    /// Whether the savepoint at `level` opened the transaction
    /// (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub fn sp_starts_txn(&self, level: usize) -> bool {
        self.sp.as_slice().get(level).is_some_and(|m| m.starts_txn)
    }

    /// Close the savepoint at `level` and every newer one, keeping
    /// their changes (v0.16, `v16`; `RELEASE`). The sub-journal is
    /// kept while an older savepoint may still need it.
    #[cfg(feature = "v16")]
    pub fn sp_release(&mut self, level: usize) {
        self.sp.truncate(level);
        if self.sp.is_empty() {
            self.sub.clear();
            self.sub_off = 0;
        }
    }

    /// Forget every savepoint (v0.16, `v16`): the transaction ended.
    #[cfg(feature = "v16")]
    pub fn sp_clear(&mut self) {
        self.sp.clear();
        self.sub.clear();
        self.sub_off = 0;
    }

    /// Undo everything since the savepoint at `level` opened, which
    /// stays open; newer ones close (v0.16, `v16`; `ROLLBACK TO`).
    /// Each page recorded since then takes its earliest recorded
    /// image (its image when the savepoint opened), pages the
    /// file grew by are dropped, and the header is restored. The
    /// transaction stays open; nothing is written to the file, and
    /// the rollback journal is untouched, so a crash still restores
    /// the transaction's start.
    #[cfg(feature = "v16")]
    pub async fn sp_rollback(&mut self, level: usize) -> Result<(), S::Error> {
        let Some(m) = self.sp.as_slice().get(level) else {
            return Ok(());
        };
        let (start, hdr) = (m.sub_len, m.header);
        let ps = self.header.page_size as usize;
        let mut done: AVec<u32> = AVec::new();
        let mut buf: AVec<u8> = AVec::with_capacity(ps);
        buf.extend_from_slice(&alloc::vec![0u8; ps]);
        let mut i = start;
        while let Some(rec) = self.sub.as_slice().get(i) {
            i += 1;
            let page = rec.page;
            if page > hdr.page_count || done.as_slice().contains(&page) {
                continue;
            }
            done.push(page);
            let Some(sc) = self.sidecar.as_ref() else {
                return Err(Error::OperationInFlight);
            };
            sc.read_at(rec.off, buf.as_mut_slice())
                .await
                .map_err(Error::Storage)?;
            let idx = self.frame_slot(page).await?;
            let f = &mut self.frames[idx];
            f.data.copy_from_slice(buf.as_slice());
            f.page = page;
            f.dirty = true;
            f.clock_ref = true;
            if !self.dirty.contains(&page) {
                self.dirty.push(page);
            }
        }
        for f in self.frames.iter_mut() {
            if f.page > hdr.page_count {
                f.page = 0;
                f.dirty = false;
            }
        }
        self.dirty.retain(|p| *p <= hdr.page_count);
        self.header = hdr;
        self.sub.truncate(start);
        self.sp.truncate(level + 1);
        Ok(())
    }

    /// Replace the whole database with `img`, a complete database
    /// image of `page_size`-byte pages (v0.16, `v16`: VACUUM), as
    /// one crash-safe transaction: every original page is
    /// journaled and the journal synced before the file changes,
    /// so a crash at any point recovers the old file, and deleting
    /// the journal is the commit point, as in `drive_commit`.
    /// Page 1 is written last. The page size may change (`PRAGMA
    /// page_size` before VACUUM); the cache is rebuilt for it. No
    /// transaction may be open.
    ///
    /// The image is a plain (non-auto-vacuum) database, so it may
    /// replace an auto-vacuum file (`PRAGMA auto_vacuum = NONE;
    /// VACUUM`): the write refusal is lifted for it, and kept when
    /// the replacement fails.
    #[cfg(feature = "v16")]
    pub async fn replace_image(&mut self, img: &[u8], page_size: u32) -> Result<(), S::Error> {
        let pm = core::mem::replace(&mut self.ptrmaps, false);
        let r = self.replace_image_inner(img, page_size).await;
        if r.is_err() {
            self.ptrmaps = pm;
        }
        r
    }

    #[cfg(feature = "v16")]
    async fn replace_image_inner(&mut self, img: &[u8], page_size: u32) -> Result<(), S::Error> {
        if self.stage != CommitStage::Idle {
            return Err(Error::OperationInFlight);
        }
        let ps = page_size as usize;
        if ps == 0 || img.len() < ps || !img.len().is_multiple_of(ps) {
            return Err(Error::CorruptPage { page: 1 });
        }
        let new_header = parse_db_header::<S::Error>(img, img.len() as u64)?;
        #[cfg(feature = "wal")]
        if self.wal.is_some() {
            return self.replace_image_wal(img, new_header).await;
        }
        self.begin_write().await?;
        for page in 1..=self.orig_page_count {
            let idx = self.load(page).await?;
            self.journal_original(page, idx).await?;
        }
        let Some(sc) = self.sidecar.as_mut() else {
            return Err(Error::OperationInFlight);
        };
        sc.sync().await.map_err(Error::Storage)?;
        // From here the main file changes: a rollback must replay
        // the journal.
        self.spilled = true;
        if let Err(e) = self.write_image(img, ps).await {
            let _ = self.rollback().await;
            return Err(e);
        }
        let deleted = match self.sidecar.as_mut() {
            Some(sc) => sc.delete().await.map_err(Error::Storage),
            None => Err(Error::OperationInFlight),
        };
        if let Err(e) = deleted {
            let _ = self.rollback().await;
            return Err(e);
        }
        // Commit point passed: the cache is rebuilt for the image.
        self.header = new_header;
        self.orig_page_count = new_header.page_count;
        for f in self.frames.iter_mut() {
            f.page = 0;
            f.dirty = false;
            if f.data.len() != ps {
                f.data = alloc::vec![0u8; ps];
            }
        }
        self.dirty.clear();
        self.journaled.clear();
        self.journal_len = 0;
        self.spilled = false;
        self.txn_header = None;
        self.stage = CommitStage::Idle;
        self.txn_arena.reset();
        self.sp_clear();
        Ok(())
    }

    /// The main-file half of `replace_image`: pages 2.. then page
    /// 1, the length, and a sync.
    #[cfg(feature = "v16")]
    async fn write_image(&mut self, img: &[u8], ps: usize) -> Result<(), S::Error> {
        let mut off = ps;
        while let Some(chunk) = img.get(off..off + ps) {
            self.storage
                .write_at(off as u64, chunk)
                .await
                .map_err(Error::Storage)?;
            off += ps;
        }
        let first = img.get(..ps).ok_or(Error::CorruptPage { page: 1 })?;
        self.storage
            .write_at(0, first)
            .await
            .map_err(Error::Storage)?;
        self.storage
            .set_len(img.len() as u64)
            .await
            .map_err(Error::Storage)?;
        self.storage.sync().await.map_err(Error::Storage)
    }

    /// The next salt bits for the log (v0.16, `wal`): the nonce
    /// stream (the Config-supplied seed mixed with a counter).
    #[cfg(feature = "wal")]
    fn rand64(&mut self) -> u64 {
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        let mut x = self.nonce_seed ^ self.nonce_counter.wrapping_mul(0x9e37_79b1_85eb_ca87);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^ (x >> 33)
    }

    /// The database is in WAL mode (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn wal_mode(&self) -> bool {
        self.wal.is_some()
    }

    /// The log's last committed frame and the frames checkpointed so
    /// far, in WAL mode (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn wal_frames(&self) -> Option<(u32, u32)> {
        self.wal.as_ref().map(|w| (w.mx_frame, w.backfill))
    }

    /// Whether a page was read since `clear_touched` (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn touched(&self) -> bool {
        self.touched
    }

    /// Start watching for page reads (v0.16, `wal`: at `BEGIN`).
    #[cfg(feature = "wal")]
    pub fn clear_touched(&mut self) {
        self.touched = false;
    }

    /// Count a read of the database as the pin's first checkpoint
    /// after entering WAL mode makes one (it opens its log with a
    /// `PRAGMA table_list`; v0.16, `wal`). Returns whether it did.
    #[cfg(feature = "wal")]
    pub fn open_fresh_wal(&mut self) -> bool {
        let fresh = self.wal_fresh && self.wal.is_some();
        if fresh {
            self.touched = true;
            self.wal_fresh = false;
        }
        fresh
    }

    /// Delete the journal file a WAL-mode transaction used for its
    /// savepoint area (v0.16, `wal`; best effort: what is left has a
    /// zero first sector, which no recovery takes for a journal).
    #[cfg(feature = "wal")]
    async fn drop_sp_file(&mut self) {
        if self.sp_file {
            if let Some(sc) = self.sidecar.as_mut() {
                let _ = sc.delete().await;
            }
            self.sp_file = false;
        }
    }

    /// A WAL-mode transaction is over, its frames published or
    /// forgotten (v0.16, `wal`): the cache keeps its (now clean)
    /// pages.
    #[cfg(feature = "wal")]
    async fn end_txn(&mut self) {
        for f in self.frames.iter_mut() {
            f.dirty = false;
        }
        self.dirty.clear();
        self.journaled.clear();
        self.journal_len = 0;
        self.spilled = false;
        self.txn_header = None;
        self.stage = CommitStage::Idle;
        self.txn_arena.reset();
        self.sp_clear();
        self.drop_sp_file().await;
    }

    /// The automatic checkpoint after a commit (v0.16, `wal`): a
    /// PASSIVE one once the log holds `wal_autocheckpoint` frames or
    /// more, as the pin's default WAL hook runs. Its failure does not
    /// undo the commit (the pin ignores it too); the frames stay in
    /// the log for the next checkpoint.
    #[cfg(feature = "wal")]
    async fn auto_checkpoint(&mut self) {
        let n = self.wal_cfg.autocheckpoint;
        if n > 0 && self.wal.as_ref().is_some_and(|w| w.mx_frame >= n) {
            let _ = self.checkpoint(Checkpoint::Passive).await;
        }
    }

    /// Copy the log's committed frames into the database file
    /// (v0.16, `wal`; `PRAGMA wal_checkpoint`): the log is synced,
    /// each page's newest frame not yet copied is written, the file
    /// is cut to the committed size and synced (`synchronous = OFF`
    /// skips the syncs). The next write transaction then restarts
    /// the log; TRUNCATE also empties the `-wal` file now. Returns
    /// the pin's (log, checkpointed) frame counts, or `None` when
    /// the database is not in WAL mode. No transaction may be open.
    #[cfg(feature = "wal")]
    pub async fn checkpoint(&mut self, mode: Checkpoint) -> Result<Option<(u32, u32)>, S::Error> {
        if self.stage != CommitStage::Idle {
            return Err(Error::OperationInFlight);
        }
        let sync = self.wal_cfg.sync >= 2;
        let ps = self.header.page_size as usize;
        let Some(w) = self.wal.as_mut() else {
            return Ok(None);
        };
        if w.mx_frame > w.backfill {
            if sync {
                w.sync().await?;
            }
            let mut buf: AVec<u8> = AVec::with_capacity(ps);
            buf.extend_from_slice(&alloc::vec![0u8; ps]);
            let (from, n) = (w.backfill, w.n_page);
            for &(page, frame) in w.committed_frames() {
                if frame <= from || page > n || page == 0 {
                    continue;
                }
                w.read_page(frame, buf.as_mut_slice()).await?;
                self.storage
                    .write_at(u64::from(page - 1) * ps as u64, buf.as_slice())
                    .await
                    .map_err(Error::Storage)?;
            }
            let want = u64::from(n) * ps as u64;
            if self.storage.len().await.map_err(Error::Storage)? != want {
                self.storage.set_len(want).await.map_err(Error::Storage)?;
            }
            if sync {
                self.storage.sync().await.map_err(Error::Storage)?;
            }
            w.backfill = w.mx_frame;
        }
        if mode == Checkpoint::Truncate && w.backfill == w.mx_frame {
            w.file.set_len(0).await.map_err(Error::Storage)?;
            if sync {
                w.sync().await?;
            }
            w.truncated();
        }
        Ok(Some((w.mx_frame, w.backfill)))
    }

    /// VACUUM's image in WAL mode (v0.16, `wal`): every page appended
    /// to the log as one transaction, whose commit frame says the new
    /// size; the database file changes only at the next checkpoint.
    /// The page size cannot change in WAL mode (the caller keeps it).
    #[cfg(feature = "wal")]
    async fn replace_image_wal(
        &mut self,
        img: &[u8],
        new_header: DbHeader,
    ) -> Result<(), S::Error> {
        let ps = self.header.page_size as usize;
        if new_header.page_size as usize != ps {
            return Err(Error::Unsupported);
        }
        self.begin_write().await?;
        let n = new_header.page_count;
        let r = self.append_image(img, ps, n).await;
        if let Err(e) = r {
            let _ = self.rollback().await;
            return Err(e);
        }
        let limit = self.wal_cfg.size_limit;
        if let Some(w) = self.wal.as_mut() {
            w.committed(n, limit).await;
        }
        self.header = new_header;
        self.orig_page_count = n;
        for f in self.frames.iter_mut() {
            f.page = 0;
        }
        self.end_txn().await;
        self.auto_checkpoint().await;
        Ok(())
    }

    /// `replace_image_wal`'s frames, then the log's sync.
    #[cfg(feature = "wal")]
    async fn append_image(&mut self, img: &[u8], ps: usize, n: u32) -> Result<(), S::Error> {
        let sync = self.wal_cfg.sync >= 3;
        for page in 1..=n {
            let at = (page as usize - 1) * ps;
            let chunk = img.get(at..at + ps).ok_or(Error::CorruptPage { page })?;
            let rand = self.rand64();
            let commit = if page == n { n } else { 0 };
            let Some(w) = self.wal.as_mut() else {
                return Err(Error::OperationInFlight);
            };
            w.append(page, chunk, commit, rand, sync).await?;
        }
        self.stage = CommitStage::WalSync;
        if sync && let Some(w) = self.wal.as_mut() {
            w.sync().await?;
        }
        Ok(())
    }

    /// Write `vals` at page 1's offset `off` (bytes 18 / 19, v0.16
    /// `wal`) as one rollback-journal transaction of its own, unless
    /// they are already there.
    #[cfg(feature = "wal")]
    async fn set_file_version(&mut self, v: u8) -> Result<(), S::Error> {
        let i1 = self.load(1).await?;
        if self.frames[i1].data.get(18..20) == Some(&[v, v][..]) {
            return Ok(());
        }
        let idx = self.dirty_page(1).await?;
        if let Some(d) = self.frames[idx].data.get_mut(18..20) {
            d.copy_from_slice(&[v, v]);
        }
        if let Err(e) = self.drive_commit().await {
            let _ = self.rollback().await;
            return Err(e);
        }
        Ok(())
    }

    /// Enter WAL mode (v0.16, `wal`; `PRAGMA journal_mode = WAL`):
    /// page 1's file-format bytes (18, 19) become 2 in a
    /// rollback-journal transaction, as the pin writes them, then
    /// writes go to the `-wal` log. A log already beside the file
    /// held no frames hipp would read (else it opened in WAL mode):
    /// it is reused from a new generation. No transaction may be
    /// open; a database another process holds is `DatabaseBusy`.
    #[cfg(feature = "wal")]
    pub async fn enter_wal(&mut self) -> Result<(), S::Error> {
        if self.wal.is_some() {
            return Ok(());
        }
        if self.stage != CommitStage::Idle {
            return Err(Error::OperationInFlight);
        }
        if !self.storage.claim_wal() {
            return Err(Error::DatabaseBusy);
        }
        let w = match self.open_new_wal().await {
            Ok(w) => w,
            Err(e) => {
                self.storage.release_wal();
                return Err(e);
            }
        };
        self.wal = Some(w);
        self.wal_fresh = true;
        Ok(())
    }

    /// `enter_wal`'s switch once the database is claimed: bytes 18 /
    /// 19 written, the log opened from a new generation.
    #[cfg(feature = "wal")]
    async fn open_new_wal(&mut self) -> Result<crate::wal::Wal<S::Sidecar>, S::Error> {
        self.set_file_version(2).await?;
        let sc = self
            .storage
            .sidecar(Sidecar::Wal)
            .await
            .map_err(Error::Storage)?;
        let mut w = crate::wal::Wal::recover(sc, self.header.page_size).await?;
        w.truncated();
        w.set_page_size(self.header.page_size);
        Ok(w)
    }

    /// Checkpoint everything, then delete the log and a stale `-shm`
    /// (v0.16, `wal`), as the pin's last connection does at close
    /// and on leaving WAL mode. Returns whether the database was in
    /// WAL mode. On a failure the log stays (and the database stays
    /// in WAL mode), so nothing committed is lost.
    #[cfg(feature = "wal")]
    async fn retire_wal(&mut self) -> Result<bool, S::Error> {
        if self.checkpoint(Checkpoint::Passive).await?.is_none() {
            return Ok(false);
        }
        let Some(mut w) = self.wal.take() else {
            return Ok(false);
        };
        if let Err(e) = w.file.delete().await {
            self.wal = Some(w);
            return Err(Error::Storage(e));
        }
        let mut shm = self
            .storage
            .sidecar(Sidecar::Shm)
            .await
            .map_err(Error::Storage)?;
        if shm.existed() {
            shm.delete().await.map_err(Error::Storage)?;
        }
        self.storage.release_wal();
        Ok(true)
    }

    /// Leave WAL mode (v0.16, `wal`; `PRAGMA journal_mode = DELETE`):
    /// checkpoint, delete the log, then write file-format bytes 1 in
    /// a rollback-journal transaction, the pin's order — a crash in
    /// between leaves a complete database still marked WAL, with no
    /// log. No transaction may be open.
    #[cfg(feature = "wal")]
    pub async fn leave_wal(&mut self) -> Result<(), S::Error> {
        if self.stage != CommitStage::Idle {
            return Err(Error::OperationInFlight);
        }
        if self.retire_wal().await? {
            self.set_file_version(1).await?;
        }
        Ok(())
    }

    /// Close the database (v0.16, `wal`; `Connection::close`): in WAL
    /// mode, checkpoint and delete the log as the pin does. Any open
    /// transaction is rolled back first.
    #[cfg(feature = "wal")]
    pub async fn close_wal(&mut self) -> Result<(), S::Error> {
        if self.stage != CommitStage::Idle {
            self.rollback().await?;
        }
        self.retire_wal().await.map(|_| ())
    }

    /// Page pool bytes (for §4.1 accounting by the session layer).
    pub fn pool_bytes(&self) -> usize {
        self.cache_cap * self.header.page_size as usize
    }
}

/// Open the database's write-ahead log when it is in WAL mode
/// (v0.16, `wal`): page 1 says so (bytes 18 / 19 = 2), or a non-empty
/// `-wal` file sits beside it (the pin then reads it whatever page 1
/// says). The committed frames' page 1 replaces `probe` and `header`,
/// whose page count becomes the last commit's size. A database
/// another process holds is `DatabaseBusy`.
#[cfg(feature = "wal")]
async fn open_wal<S: Storage>(
    storage: &mut S,
    probe: &mut [u8; 100],
    header: &mut DbHeader,
) -> Result<Option<crate::wal::Wal<S::Sidecar>>, S::Error> {
    let sc = storage
        .sidecar(Sidecar::Wal)
        .await
        .map_err(Error::Storage)?;
    if probe[18] != 2 && probe[19] != 2 {
        let len = if sc.existed() {
            sc.len().await.map_err(Error::Storage)?
        } else {
            0
        };
        if len == 0 {
            return Ok(None);
        }
    }
    if !storage.claim_wal() {
        return Err(Error::DatabaseBusy);
    }
    let mut w = crate::wal::Wal::recover(sc, header.page_size).await?;
    if w.mx_frame == 0 {
        w.set_page_size(header.page_size);
        return Ok(Some(w));
    }
    let ps = w.page_size();
    if let Some(f) = w.lookup(1) {
        let mut img: AVec<u8> = AVec::with_capacity(ps as usize);
        img.extend_from_slice(&alloc::vec![0u8; ps as usize]);
        w.read_page(f, img.as_mut_slice()).await?;
        probe.copy_from_slice(img.as_slice().get(..100).ok_or(Error::TruncatedHeader)?);
    }
    *header = parse_db_header::<S::Error>(&probe[..], u64::from(w.n_page) * u64::from(ps))?;
    if header.page_size != ps {
        return Err(Error::CorruptPage { page: 1 });
    }
    header.page_count = w.n_page;
    Ok(Some(w))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAL header (file-format bytes 18 / 19 = 2) is refused cleanly
    /// without the `wal` leaf — never read from the possibly stale
    /// database file — and parsed with it (v0.16).
    #[test]
    fn wal_header() {
        for (w, r) in [(2u8, 2u8), (1, 2), (2, 1)] {
            let mut p = initial_page1(4096);
            p[18] = w;
            p[19] = r;
            let h = parse_db_header::<()>(&p, 4096);
            #[cfg(not(feature = "wal"))]
            assert!(matches!(h, Err(Error::WalUnsupported)), "{w}/{r}");
            #[cfg(feature = "wal")]
            assert!(h.is_ok(), "{w}/{r}");
        }
    }

    /// Without the `wal` leaf a WAL-mode database is refused at open,
    /// whatever its `-wal` holds (v0.16).
    #[cfg(not(feature = "wal"))]
    #[test]
    fn wal_database_refused_at_open() {
        let mut p = initial_page1(4096);
        p[18] = 2;
        p[19] = 2;
        let mem = crate::storage::MemStorage::from_bytes(p);
        let r = crate::storage::block_on(Pager::open(
            mem,
            4,
            4096,
            1,
            Arena::new(crate::error::Scope::Transaction, 1024, 4096),
        ));
        assert!(matches!(r, Err(Error::WalUnsupported)));
    }

    /// A non-empty `-wal` beside a rollback-mode header (v0.16 review
    /// fix): the pin opens such a database in WAL mode and commits
    /// into the log, so without the `wal` leaf it is refused
    /// (`Storage::log_beside`) — it was read stale, and written under
    /// the log. With the leaf a log of no valid frames leaves the
    /// main file as it is, and one beside an empty file is deleted,
    /// as on the pin.
    #[test]
    fn log_beside_rollback_header() {
        use crate::storage::MemStorage;
        fn open<S: Storage>(s: S) -> Result<(), S::Error> {
            crate::storage::block_on(Pager::open(
                s,
                4,
                4096,
                1,
                Arena::new(crate::error::Scope::Transaction, 1024, 4096),
            ))
            .map(|_| ())
        }
        /// MemStorage with a log beside it, as `log_beside` sees one.
        struct Logged(MemStorage);
        impl Storage for Logged {
            type Error = crate::storage::MemError;
            type Sidecar = crate::storage::MemSidecar;
            async fn len(&self) -> core::result::Result<u64, Self::Error> {
                self.0.len().await
            }
            async fn read_at(&self, o: u64, b: &mut [u8]) -> core::result::Result<(), Self::Error> {
                self.0.read_at(o, b).await
            }
            async fn write_at(
                &mut self,
                o: u64,
                b: &[u8],
            ) -> core::result::Result<(), Self::Error> {
                self.0.write_at(o, b).await
            }
            async fn set_len(&mut self, l: u64) -> core::result::Result<(), Self::Error> {
                self.0.set_len(l).await
            }
            async fn sync(&mut self) -> core::result::Result<(), Self::Error> {
                self.0.sync().await
            }
            async fn sidecar(
                &mut self,
                k: Sidecar,
            ) -> core::result::Result<Self::Sidecar, Self::Error> {
                self.0.sidecar(k).await
            }
            fn log_beside(&self) -> bool {
                true
            }
        }
        let r = open(Logged(MemStorage::from_bytes(initial_page1(4096))));
        #[cfg(not(feature = "wal"))]
        assert!(matches!(r, Err(Error::WalUnsupported)));
        #[cfg(feature = "wal")]
        assert!(r.is_ok());
        assert!(open(MemStorage::from_bytes(initial_page1(4096))).is_ok());
        #[cfg(feature = "wal")]
        {
            let mem = MemStorage::from_bytes(initial_page1(4096));
            mem.set_wal(Some(alloc::vec![7u8; 100]));
            assert!(open(mem.clone()).is_ok());
            assert!(mem.wal_snapshot().is_some());
            let mem = MemStorage::new();
            mem.set_wal(Some(alloc::vec![7u8; 100]));
            assert!(open(mem.clone()).is_ok());
            assert!(mem.wal_snapshot().is_none());
        }
    }

    /// A UTF-16 header (offset 56 = 2 / 3) is refused cleanly
    /// without the `utf16` leaf — never read as UTF-8 — and opens
    /// with it (v0.16).
    #[test]
    fn utf16_header() {
        for code in [2u32, 3] {
            let mut p = initial_page1(4096);
            put_be_u32(&mut p, 56, code);
            let r = parse_db_header::<()>(&p, 4096);
            #[cfg(not(feature = "utf16"))]
            assert!(matches!(r, Err(Error::UnsupportedEncoding)));
            #[cfg(feature = "utf16")]
            assert_eq!(
                r.ok().map(|h| enc_code(h.text_enc)),
                Some(code),
                "encoding {code}"
            );
        }
    }
}
