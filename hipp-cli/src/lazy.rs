//! A database file that stays empty until its first real write (v0.17,
//! `shell` unit): the pin's shell leaves a new database file at 0
//! bytes until something is written to it (a `SELECT`, `.tables`,
//! `BEGIN; COMMIT;` or `PRAGMA page_size = N` write nothing), while
//! hipp's pager writes its header page when it opens an empty file.
//!
//! `LazyFile` wraps the shell's `FileStorage`: while the file is
//! empty, page 1 as the pager writes it at open is kept in memory and
//! read from there; it reaches the file, with whatever else has been
//! written since, at the first write that leaves the image something
//! other than a fresh empty database (`pager::initial_page1` for its
//! page size). Everything else — sidecars, locks, the file's name,
//! attached databases — is the wrapped storage's. The library is not
//! changed: this is the shell's storage.

use hipp::storage::{FileError, FileSidecar, Sidecar};
use hipp::{AttachTarget, FileStorage, Storage};

pub struct LazyFile {
    fs: FileStorage,
    /// The image of a database not yet written to the file.
    pending: Option<Vec<u8>>,
    /// The file was empty when opened (so the pager's first write may
    /// be held back).
    fresh: bool,
}

impl LazyFile {
    pub fn new(fs: FileStorage) -> LazyFile {
        let fresh = hipp::storage::block_on(fs.len()).is_ok_and(|n| n == 0);
        LazyFile {
            fs,
            pending: None,
            fresh,
        }
    }

    /// Whether `img` starts with a fresh page 1 for its page size,
    /// exactly as the pager makes it (`PRAGMA page_size` on an empty
    /// database rewrites it, then trims the file to it).
    fn page1_fresh(img: &[u8]) -> bool {
        let Some(ps) = img.get(16..18) else {
            return false;
        };
        let ps = match u32::from(u16::from_be_bytes([ps[0], ps[1]])) {
            1 => 65536,
            n => n,
        } as usize;
        img.get(..ps) == Some(hipp::pager::initial_page1(ps as u32).as_slice())
    }

    /// Whether `img` is a fresh empty database: that page alone.
    fn untouched(img: &[u8]) -> bool {
        LazyFile::page1_fresh(img)
            && img.get(16..18).is_some_and(|p| {
                let ps = match u16::from_be_bytes([p[0], p[1]]) {
                    1 => 65536,
                    n => usize::from(n),
                };
                img.len() == ps
            })
    }

    /// Write the held image to the file; later writes go straight
    /// through.
    async fn flush(&mut self) -> Result<(), FileError> {
        if let Some(img) = self.pending.take() {
            self.fs.write_at(0, &img).await?;
        }
        Ok(())
    }
}

impl Drop for LazyFile {
    /// Whatever was written and not yet synced reaches the file.
    fn drop(&mut self) {
        if self
            .pending
            .as_deref()
            .is_some_and(|i| !LazyFile::untouched(i))
        {
            let _ = hipp::storage::block_on(self.flush());
        }
    }
}

impl Storage for LazyFile {
    type Error = FileError;
    type Sidecar = FileSidecar;

    async fn len(&self) -> Result<u64, FileError> {
        match &self.pending {
            Some(img) => Ok(img.len() as u64),
            None => self.fs.len().await,
        }
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
        let Some(img) = &self.pending else {
            return self.fs.read_at(offset, buf).await;
        };
        let start = usize::try_from(offset).map_err(|_| FileError)?;
        let end = start.checked_add(buf.len()).ok_or(FileError)?;
        let src = img.get(start..end).ok_or(FileError)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FileError> {
        if self.pending.is_none() && self.fresh && offset == 0 {
            self.fresh = false;
            if self.fs.len().await? == 0 {
                self.pending = Some(Vec::new());
            }
        }
        self.fresh = false;
        let Some(img) = &mut self.pending else {
            return self.fs.write_at(offset, buf).await;
        };
        let start = usize::try_from(offset).map_err(|_| FileError)?;
        let end = start.checked_add(buf.len()).ok_or(FileError)?;
        if img.len() < end {
            img.resize(end, 0);
        }
        img[start..end].copy_from_slice(buf);
        if !LazyFile::page1_fresh(img) {
            self.flush().await?;
        }
        Ok(())
    }

    async fn set_len(&mut self, len: u64) -> Result<(), FileError> {
        self.fresh = false;
        let Some(img) = &mut self.pending else {
            return self.fs.set_len(len).await;
        };
        // (Checked at the next sync: `PRAGMA page_size` trims the held
        // page before it rewrites it.)
        img.resize(usize::try_from(len).map_err(|_| FileError)?, 0);
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), FileError> {
        match &self.pending {
            Some(img) if LazyFile::untouched(img) => return Ok(()),
            Some(_) => self.flush().await?,
            None => {}
        }
        self.fs.sync().await
    }

    async fn sidecar(&mut self, kind: Sidecar) -> Result<FileSidecar, FileError> {
        self.fs.sidecar(kind).await
    }

    fn file_name(&self) -> &[u8] {
        self.fs.file_name()
    }

    fn log_beside(&self) -> bool {
        self.fs.log_beside()
    }

    fn claim_wal(&mut self) -> bool {
        self.fs.claim_wal()
    }

    fn release_wal(&mut self) {
        self.fs.release_wal()
    }

    fn is_memory(&self) -> bool {
        self.fs.is_memory()
    }

    fn open_attached(&self, target: AttachTarget<'_>) -> Option<LazyFile> {
        self.fs.open_attached(target).map(LazyFile::new)
    }

    async fn open_named(&mut self, name: &[u8]) -> Result<Option<FileSidecar>, FileError> {
        self.fs.open_named(name).await
    }
}
