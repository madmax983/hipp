//! The write-ahead log (v0.16, `wal`): the pin's `-wal` file format
//! and hipp's in-heap wal-index.
//!
//! File format (the pin's, verified against its files): a 32-byte
//! header — magic `0x377f0682` (checksums over little-endian words)
//! or `0x377f0683` (big-endian), format version 3007000, page size,
//! checkpoint sequence, two salts, and a checksum of the first 24
//! bytes — then frames of a 24-byte header (page number, the
//! database size in pages for a commit frame or 0, the two salts,
//! a cumulative checksum) and one page image. Each frame's checksum
//! continues the previous one (the header's for frame 1) over the
//! frame header's first 8 bytes and the page. A reader takes frames
//! up to the first whose salts or checksum do not match (or whose
//! page number is 0), and of those, the ones up to the last commit
//! frame.
//!
//! hipp is single-process, as the pin is in `locking_mode=EXCLUSIVE`:
//! the wal-index (page → newest committed frame) lives in heap
//! memory, rebuilt from the file at open; no `-shm` is written.
//! Writers append after the last committed frame, or — once every
//! frame has been checkpointed — restart the log from frame 1 under
//! a new header (checkpoint sequence + 1, first salt + 1, a fresh
//! second salt), exactly as the pin restarts it, so stale frames
//! never validate.

use crate::arena::AVec;
use crate::error::{Error, Result};
use crate::storage::SidecarFile;

/// The header magic for little-endian checksums (what hipp writes,
/// as the pin does on little-endian hosts); `| 1` is big-endian.
pub const WAL_MAGIC: u32 = 0x377f_0682;
/// The WAL format version (the pin's `WAL_MAX_VERSION`).
pub const WAL_VERSION: u32 = 3_007_000;
/// The WAL header's size in bytes.
pub const WAL_HDR: u64 = 32;
/// A frame header's size in bytes.
pub const FRAME_HDR: u64 = 24;
/// The pin's default `PRAGMA wal_autocheckpoint` (pages).
pub const DEFAULT_AUTOCHECKPOINT: u32 = 1000;

/// Continue the WAL checksum `s` over `data` (a multiple of 8 bytes;
/// any tail is ignored) reading 32-bit words big- or little-endian.
pub fn checksum(big_endian: bool, data: &[u8], s: [u32; 2]) -> [u32; 2] {
    let [mut s0, mut s1] = s;
    for c in data.chunks_exact(8) {
        let Ok(w) = <[u8; 8]>::try_from(c) else {
            break;
        };
        let (a, b) = if big_endian {
            (
                u32::from_be_bytes([w[0], w[1], w[2], w[3]]),
                u32::from_be_bytes([w[4], w[5], w[6], w[7]]),
            )
        } else {
            (
                u32::from_le_bytes([w[0], w[1], w[2], w[3]]),
                u32::from_le_bytes([w[4], w[5], w[6], w[7]]),
            )
        };
        s0 = s0.wrapping_add(a).wrapping_add(s1);
        s1 = s1.wrapping_add(b).wrapping_add(s0);
    }
    [s0, s1]
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    match b.get(off..off + 4) {
        Some(&[a, b2, c, d]) => u32::from_be_bytes([a, b2, c, d]),
        _ => 0,
    }
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    if let Some(d) = b.get_mut(off..off + 4) {
        d.copy_from_slice(&v.to_be_bytes());
    }
}

/// A page → frame map, sorted by page (the wal-index).
#[derive(Default)]
pub struct FrameIndex {
    e: AVec<(u32, u32)>,
}

impl FrameIndex {
    /// The frame holding `page`, if any.
    pub fn get(&self, page: u32) -> Option<u32> {
        let s = self.e.as_slice();
        s.binary_search_by_key(&page, |x| x.0)
            .ok()
            .and_then(|i| s.get(i))
            .map(|x| x.1)
    }
    /// Map `page` to `frame` (newer frames replace older ones).
    pub fn set(&mut self, page: u32, frame: u32) {
        match self.e.as_slice().binary_search_by_key(&page, |x| x.0) {
            Ok(i) => {
                if let Some(x) = self.e.as_mut_slice().get_mut(i) {
                    x.1 = frame;
                }
            }
            Err(i) => self.e.insert(i, (page, frame)),
        }
    }
    pub fn clear(&mut self) {
        self.e.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.e.is_empty()
    }
    /// The (page, frame) pairs in page order.
    pub fn entries(&self) -> &[(u32, u32)] {
        self.e.as_slice()
    }
}

/// The open write-ahead log of one database: its file, its current
/// header, the committed frames' index and the frames the open
/// write transaction has appended so far.
pub struct Wal<F> {
    pub file: F,
    page_size: u32,
    big_endian: bool,
    salt: [u32; 2],
    seq: u32,
    /// The file holds a valid header for the current generation.
    hdr_ok: bool,
    /// The last committed frame (0: none).
    pub mx_frame: u32,
    /// The checksum chain at `mx_frame` (the header's at 0).
    mx_ck: [u32; 2],
    /// The database size in pages at `mx_frame`.
    pub n_page: u32,
    /// Frames already copied into the database file.
    pub backfill: u32,
    committed: FrameIndex,
    /// The open transaction's frames (`tx_end` > `mx_frame`).
    pending: FrameIndex,
    tx_started: bool,
    tx_end: u32,
    tx_ck: [u32; 2],
    /// The first of the open transaction's frames overwritten in
    /// place since it was written (the pin's `iReCksum`): the
    /// checksums from there on are recomputed before the commit
    /// frame counts.
    re_ck: Option<u32>,
    /// The log restarted in this generation and no commit has
    /// truncated it to `journal_size_limit` yet (the pin's
    /// `truncateOnCommit`).
    truncate_on_commit: bool,
}

impl<F: SidecarFile> Wal<F> {
    /// The byte offset of frame `frame` (1-based).
    fn frame_off(&self, frame: u32) -> u64 {
        WAL_HDR + u64::from(frame.saturating_sub(1)) * (FRAME_HDR + u64::from(self.page_size))
    }

    /// Open the log in `file` for a database of `page_size`-byte
    /// pages and rebuild the wal-index from it, as the pin's
    /// recovery does: a header with a bad magic, version, page size
    /// or checksum makes the whole log empty; frames are read up to
    /// the first invalid one, and those after the last commit frame
    /// are ignored. `page_size` is used when the header does not
    /// hold a valid one.
    pub async fn recover(file: F, page_size: u32) -> Result<Wal<F>, F::Error> {
        let mut w = Wal {
            file,
            page_size,
            big_endian: false,
            salt: [0, 0],
            seq: 0,
            hdr_ok: false,
            mx_frame: 0,
            mx_ck: [0, 0],
            n_page: 0,
            backfill: 0,
            committed: FrameIndex::default(),
            pending: FrameIndex::default(),
            tx_started: false,
            tx_end: 0,
            tx_ck: [0, 0],
            re_ck: None,
            truncate_on_commit: false,
        };
        let len = w.file.len().await.map_err(Error::Storage)?;
        if len < WAL_HDR {
            return Ok(w);
        }
        let mut hdr = [0u8; WAL_HDR as usize];
        w.file.read_at(0, &mut hdr).await.map_err(Error::Storage)?;
        let magic = get_u32(&hdr, 0);
        let ps = get_u32(&hdr, 8);
        let be = magic & 1 == 1;
        let ck = checksum(be, hdr.get(..24).unwrap_or_default(), [0, 0]);
        if magic & !1 != WAL_MAGIC
            || get_u32(&hdr, 4) != WAL_VERSION
            || !(512..=65536).contains(&ps)
            || !ps.is_power_of_two()
            || ck != [get_u32(&hdr, 24), get_u32(&hdr, 28)]
        {
            return Ok(w);
        }
        w.page_size = ps;
        w.big_endian = be;
        w.seq = get_u32(&hdr, 12);
        w.salt = [get_u32(&hdr, 16), get_u32(&hdr, 20)];
        w.hdr_ok = true;
        w.mx_ck = ck;
        let mut run = ck;
        let mut buf: AVec<u8> = AVec::with_capacity(ps as usize);
        buf.extend_from_slice(&alloc::vec![0u8; ps as usize]);
        let mut fh = [0u8; FRAME_HDR as usize];
        let mut frame = 1u32;
        let mut pend = FrameIndex::default();
        loop {
            let off = w.frame_off(frame);
            let Some(end) = off.checked_add(FRAME_HDR + u64::from(ps)) else {
                break;
            };
            if end > len {
                break;
            }
            w.file.read_at(off, &mut fh).await.map_err(Error::Storage)?;
            w.file
                .read_at(off + FRAME_HDR, buf.as_mut_slice())
                .await
                .map_err(Error::Storage)?;
            let page = get_u32(&fh, 0);
            if page == 0 || [get_u32(&fh, 8), get_u32(&fh, 12)] != w.salt {
                break;
            }
            let c = checksum(be, fh.get(..8).unwrap_or_default(), run);
            let c = checksum(be, buf.as_slice(), c);
            if c != [get_u32(&fh, 16), get_u32(&fh, 20)] {
                break;
            }
            run = c;
            pend.set(page, frame);
            let commit = get_u32(&fh, 4);
            if commit != 0 {
                for &(p, f) in pend.entries() {
                    w.committed.set(p, f);
                }
                pend.clear();
                w.mx_frame = frame;
                w.mx_ck = c;
                w.n_page = commit;
            }
            let Some(next) = frame.checked_add(1) else {
                break;
            };
            frame = next;
        }
        w.tx_end = w.mx_frame;
        w.tx_ck = w.mx_ck;
        Ok(w)
    }

    /// The page size the log's frames hold.
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Take the database's page size for a log holding no committed
    /// frames (its next header will say it).
    pub fn set_page_size(&mut self, page_size: u32) {
        if self.mx_frame == 0 {
            self.page_size = page_size;
        }
    }

    /// The frame `page` should be read from: the open transaction's
    /// newest, else the newest committed one (else the database
    /// file holds it).
    pub fn lookup(&self, page: u32) -> Option<u32> {
        self.pending.get(page).or_else(|| self.committed.get(page))
    }

    /// Read frame `frame`'s page image into `buf`.
    pub async fn read_page(&self, frame: u32, buf: &mut [u8]) -> Result<(), F::Error> {
        self.file
            .read_at(self.frame_off(frame) + FRAME_HDR, buf)
            .await
            .map_err(Error::Storage)
    }

    /// Prepare the open transaction's first frame: append after the
    /// last committed frame, or — when every frame has been
    /// checkpointed (or the log is empty) — restart the log under a
    /// new header, written now (and synced when `sync`). `rand`
    /// supplies the fresh salt bits.
    async fn start(&mut self, rand: u64, sync: bool) -> Result<(), F::Error> {
        if self.tx_started {
            return Ok(());
        }
        if self.mx_frame > 0 && self.backfill < self.mx_frame {
            self.tx_started = true;
            return Ok(());
        }
        if self.hdr_ok || self.mx_frame > 0 {
            self.seq = self.seq.wrapping_add(1);
            self.salt = [self.salt[0].wrapping_add(1), rand as u32];
        } else {
            self.seq = 0;
            self.salt = [(rand >> 32) as u32, rand as u32];
        }
        self.big_endian = false;
        self.committed.clear();
        self.mx_frame = 0;
        self.backfill = 0;
        self.n_page = 0;
        let mut hdr = [0u8; WAL_HDR as usize];
        put_u32(&mut hdr, 0, WAL_MAGIC);
        put_u32(&mut hdr, 4, WAL_VERSION);
        put_u32(&mut hdr, 8, self.page_size);
        put_u32(&mut hdr, 12, self.seq);
        put_u32(&mut hdr, 16, self.salt[0]);
        put_u32(&mut hdr, 20, self.salt[1]);
        let ck = checksum(false, hdr.get(..24).unwrap_or_default(), [0, 0]);
        put_u32(&mut hdr, 24, ck[0]);
        put_u32(&mut hdr, 28, ck[1]);
        // From here the old frames are void: a crash leaves the
        // (fully checkpointed) database file alone in charge.
        self.hdr_ok = false;
        self.file.write_at(0, &hdr).await.map_err(Error::Storage)?;
        if sync {
            self.file.sync().await.map_err(Error::Storage)?;
        }
        self.hdr_ok = true;
        self.mx_ck = ck;
        self.tx_ck = ck;
        self.tx_end = 0;
        self.tx_started = true;
        self.truncate_on_commit = true;
        Ok(())
    }

    /// Append `data` as a frame for `page` to the open transaction;
    /// `commit` is the database size in pages for its commit frame,
    /// 0 for any other. A page the transaction already wrote a frame
    /// for is overwritten in place instead (unless this is the commit
    /// frame), as the pin does; the commit frame then first rewrites
    /// the checksums from the earliest such frame on, so until the
    /// commit frame's own header is written a reader stops before it.
    pub async fn append(
        &mut self,
        page: u32,
        data: &[u8],
        commit: u32,
        rand: u64,
        sync: bool,
    ) -> Result<(), F::Error> {
        if data.len() != self.page_size as usize {
            return Err(Error::CorruptPage { page });
        }
        self.start(rand, sync).await?;
        if commit == 0
            && let Some(f) = self.pending.get(page).filter(|f| *f > self.mx_frame)
        {
            self.file
                .write_at(self.frame_off(f) + FRAME_HDR, data)
                .await
                .map_err(Error::Storage)?;
            self.re_ck = Some(self.re_ck.map_or(f, |r| r.min(f)));
            return Ok(());
        }
        let frame = self.tx_end.checked_add(1).ok_or(Error::Unsupported)?;
        let mut fh = [0u8; FRAME_HDR as usize];
        put_u32(&mut fh, 0, page);
        put_u32(&mut fh, 4, commit);
        put_u32(&mut fh, 8, self.salt[0]);
        put_u32(&mut fh, 12, self.salt[1]);
        let off = self.frame_off(frame);
        // The page first, then its header: a torn frame never
        // validates.
        self.file
            .write_at(off + FRAME_HDR, data)
            .await
            .map_err(Error::Storage)?;
        self.tx_end = frame;
        self.pending.set(page, frame);
        let from = match self.re_ck {
            Some(r) if commit != 0 => r,
            _ => frame,
        };
        self.seal(from, &mut fh, data).await?;
        if commit != 0 {
            self.re_ck = None;
        }
        Ok(())
    }

    /// Write the headers of frames `from ..= tx_end` with the
    /// checksum chain recomputed from frame `from - 1` (`last` is the
    /// newest frame's header, its page `data`; the frames between
    /// are read back).
    async fn seal(&mut self, from: u32, last: &mut [u8; 24], data: &[u8]) -> Result<(), F::Error> {
        let be = self.big_endian;
        let mut c = if from <= self.mx_frame.saturating_add(1) {
            self.mx_ck
        } else {
            let mut prev = [0u8; FRAME_HDR as usize];
            self.file
                .read_at(self.frame_off(from - 1), &mut prev)
                .await
                .map_err(Error::Storage)?;
            [get_u32(&prev, 16), get_u32(&prev, 20)]
        };
        if from < self.tx_end {
            let mut fh = [0u8; FRAME_HDR as usize];
            let mut buf: AVec<u8> = AVec::with_capacity(self.page_size as usize);
            buf.extend_from_slice(&alloc::vec![0u8; self.page_size as usize]);
            for f in from..self.tx_end {
                let off = self.frame_off(f);
                self.file
                    .read_at(off, &mut fh)
                    .await
                    .map_err(Error::Storage)?;
                self.file
                    .read_at(off + FRAME_HDR, buf.as_mut_slice())
                    .await
                    .map_err(Error::Storage)?;
                c = checksum(be, fh.get(..8).unwrap_or_default(), c);
                c = checksum(be, buf.as_slice(), c);
                put_u32(&mut fh, 16, c[0]);
                put_u32(&mut fh, 20, c[1]);
                self.file.write_at(off, &fh).await.map_err(Error::Storage)?;
            }
        } else {
            c = self.tx_ck;
        }
        c = checksum(be, last.get(..8).unwrap_or_default(), c);
        c = checksum(be, data, c);
        put_u32(last, 16, c[0]);
        put_u32(last, 20, c[1]);
        self.file
            .write_at(self.frame_off(self.tx_end), last)
            .await
            .map_err(Error::Storage)?;
        self.tx_ck = c;
        Ok(())
    }

    /// Whether the open transaction has appended any frame.
    pub fn tx_frames(&self) -> bool {
        self.tx_end > self.mx_frame
    }

    /// The open transaction's last frame was its commit frame:
    /// publish its frames (the commit point has passed), then — on
    /// the first commit after a restart — truncate the file to
    /// `size_limit` bytes (or to the frames' end, if larger) when
    /// the limit is set, as the pin's `journal_size_limit` does
    /// (best effort: a file left longer holds only void frames).
    pub async fn committed(&mut self, n_page: u32, size_limit: i64) {
        for &(p, f) in self.pending.entries() {
            self.committed.set(p, f);
        }
        self.pending.clear();
        self.mx_frame = self.tx_end;
        self.mx_ck = self.tx_ck;
        self.n_page = n_page;
        self.tx_started = false;
        if self.truncate_on_commit && size_limit >= 0 {
            self.truncate_on_commit = false;
            let end = self.frame_off(self.mx_frame.saturating_add(1));
            let want = end.max(size_limit as u64);
            if self.file.len().await.is_ok_and(|len| len > want) {
                let _ = self.file.set_len(want).await;
            }
        }
    }

    /// Sync the log file.
    pub async fn sync(&mut self) -> Result<(), F::Error> {
        self.file.sync().await.map_err(Error::Storage)
    }

    /// Forget the open transaction's frames (a rollback). When
    /// `invalidate` is set its commit frame may have reached the
    /// file (a commit that failed after writing it): that frame's
    /// salts are overwritten, best effort, so a later recovery does
    /// not take the rolled-back transaction as committed.
    pub async fn rollback(&mut self, invalidate: bool) {
        if invalidate && self.tx_end > self.mx_frame {
            let off = self.frame_off(self.tx_end) + 8;
            let _ = self.file.write_at(off, &[0u8; 8]).await;
        }
        self.pending.clear();
        self.tx_end = self.mx_frame;
        self.tx_ck = self.mx_ck;
        self.tx_started = false;
        self.re_ck = None;
    }

    /// The committed index, page order (the checkpoint's copy list).
    pub fn committed_frames(&self) -> &[(u32, u32)] {
        self.committed.entries()
    }

    /// After a complete `TRUNCATE` checkpoint: the file is empty and
    /// the next frame starts a new generation.
    pub fn truncated(&mut self) {
        self.committed.clear();
        self.mx_frame = 0;
        self.backfill = 0;
        self.n_page = 0;
        self.tx_end = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{MemStorage, Sidecar, Storage, block_on};

    /// The checksum over a few words, both byte orders, against the
    /// format's definition worked by hand.
    #[test]
    fn checksum_words() {
        let d = [1u8, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0];
        // s0 = 1, s1 = 2 + 1 = 3; s0 = 1 + 3 + 3 = 7, s1 = 3 + 4 + 7 = 14.
        assert_eq!(checksum(false, &d, [0, 0]), [7, 14]);
        let be: [u8; 8] = [0, 0, 0, 5, 0, 0, 0, 6];
        assert_eq!(checksum(true, &be, [1, 2]), [8, 16]);
        assert_eq!(checksum(false, &be, [0, 0]), [0x0500_0000, 0x0b00_0000]);
    }

    #[test]
    fn frame_index_keeps_the_newest() {
        let mut ix = FrameIndex::default();
        for (p, f) in [(5, 1), (2, 2), (5, 3), (9, 4), (2, 5)] {
            ix.set(p, f);
        }
        assert_eq!(ix.entries(), &[(2, 5), (5, 3), (9, 4)]);
        assert_eq!(ix.get(5), Some(3));
        assert_eq!(ix.get(7), None);
    }

    /// Frames written, committed, recovered from the file; a torn
    /// commit frame and frames after the last commit are not.
    #[test]
    fn append_commit_recover() {
        block_on(async {
            let mut mem = MemStorage::new();
            let sc = mem.sidecar(Sidecar::Wal).await.expect("wal");
            let mut w = Wal::recover(sc, 512).await.expect("empty");
            assert_eq!(w.mx_frame, 0);
            let page = |b: u8| alloc::vec![b; 512];
            w.append(1, &page(1), 0, 7, true).await.expect("f1");
            w.append(2, &page(2), 2, 7, true).await.expect("f2");
            w.committed(2, -1).await;
            w.append(2, &page(3), 0, 7, true).await.expect("f3");
            // Overwritten in place: still three frames.
            w.append(2, &page(4), 0, 7, true).await.expect("f3'");
            assert_eq!(w.tx_end, 3);
            let log = mem.wal_snapshot().expect("log");
            let sc = mem.sidecar(Sidecar::Wal).await.expect("wal");
            let r = Wal::recover(sc, 512).await.expect("recover");
            assert_eq!((r.mx_frame, r.n_page), (2, 2), "uncommitted frame ignored");
            assert_eq!(r.lookup(2), Some(2));
            w.append(3, &page(5), 3, 7, true).await.expect("commit");
            w.committed(3, -1).await;
            let sc = mem.sidecar(Sidecar::Wal).await.expect("wal");
            let r = Wal::recover(sc, 512).await.expect("recover");
            assert_eq!((r.mx_frame, r.n_page), (4, 3));
            let mut buf = alloc::vec![0u8; 512];
            r.read_page(r.lookup(2).expect("page 2"), &mut buf)
                .await
                .expect("read");
            assert_eq!(buf, page(4), "the in-place rewrite, checksums resealed");
            // A torn commit frame: the transaction is not there.
            let full = mem.wal_snapshot().expect("log");
            mem.set_wal(Some(full[..full.len() - 10].to_vec()));
            let sc = mem.sidecar(Sidecar::Wal).await.expect("wal");
            let r = Wal::recover(sc, 512).await.expect("recover");
            assert_eq!(r.mx_frame, 2);
            assert!(log.len() < full.len());
        });
    }
}
