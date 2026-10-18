//! Caller-provided async storage — hipp's own trait, patterned on
//! Horton's caller-owned device idea but self-contained (SPEC §5.1,
//! D2 final form): runtime-agnostic async primitives built only on
//! `core`'s Future/Poll/Waker machinery. hipp ships no executor; the
//! caller brings tokio, embassy, smol, or a trivial `block_on`.
//!
//! Implementor contract (SPEC §5.1.1): futures must tolerate
//! cancellation at any await point — a write may or may not have
//! landed; the journal protocol exists to absorb exactly that
//! uncertainty. `sync` is a durability barrier, not a hint: it must
//! not resolve `Ok` unless previously written bytes survive the
//! target's stated crash model (RISKS R2).

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Which sidecar file is being opened. v0.1 needs exactly one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sidecar {
    RollbackJournal,
}

/// A sidecar file (the rollback journal in v0.1): the same byte
/// shape as the main file plus `delete` and an existence record,
/// because the journal protocol ends by deleting the journal and
/// recovery must tell a pre-existing journal from a freshly created
/// empty one.
// `async fn` in trait is deliberate (SPEC §5.1.1): RPITIT, no
// boxing, no `async-trait` dependency; futures need not be Send.
#[allow(async_fn_in_trait)]
pub trait SidecarFile {
    type Error;
    /// Whether this sidecar already existed (with content or not)
    /// when it was opened — as opposed to being created by the open.
    fn existed(&self) -> bool;
    async fn len(&self) -> Result<u64, Self::Error>;
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error>;
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    async fn sync(&mut self) -> Result<(), Self::Error>;
    /// Delete the sidecar. Deleting an already-deleted sidecar is Ok
    /// (commit stages are idempotent, SPEC §5.3).
    async fn delete(&mut self) -> Result<(), Self::Error>;
}

/// Byte-addressed async storage for one database file plus its
/// sidecars. Page size is a pager-level runtime value; this trait
/// knows nothing about pages.
#[allow(async_fn_in_trait)]
pub trait Storage {
    type Error;
    type Sidecar: SidecarFile<Error = Self::Error>;

    /// Length of the main database file in bytes.
    async fn len(&self) -> Result<u64, Self::Error>;
    /// Read exactly `buf.len()` bytes at `offset`.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error>;
    /// Grow/truncate the main file. Growth is how new pages arrive.
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    /// Durability barrier for the main file.
    async fn sync(&mut self) -> Result<(), Self::Error>;
    /// Open (creating if absent) one of this database's sidecars.
    async fn sidecar(&mut self, kind: Sidecar) -> Result<Self::Sidecar, Self::Error>;
}

/// Optional advisory locking surface (SPEC §5.1): defined now so the
/// pager's shape never needs retrofitting; v0.1 implementations may
/// be no-ops on single-device targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockLevel {
    Shared,
    Reserved,
    Exclusive,
}

#[allow(async_fn_in_trait)]
pub trait AdvisoryLock {
    type Error;
    async fn acquire(&mut self, level: LockLevel) -> Result<(), Self::Error>;
    async fn release(&mut self) -> Result<(), Self::Error>;
}

// ------------------------------------------------------------------
// MemStorage — RAM implementation (alloc-only, ships in the core).
// The test workhorse and crash-injector host (SPEC §5.1).
// ------------------------------------------------------------------

#[derive(Debug)]
pub struct MemError;

struct MemInner {
    data: Vec<u8>,
    journal: Option<Vec<u8>>,
}

/// In-memory `Storage`. Cloning shares the same underlying bytes —
/// that is deliberate: a test can keep a handle to the "device" while
/// the engine owns another, and crash wrappers snapshot through it.
#[derive(Clone)]
pub struct MemStorage {
    inner: Rc<RefCell<MemInner>>,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage {
            inner: Rc::new(RefCell::new(MemInner {
                data: Vec::new(),
                journal: None,
            })),
        }
    }

    /// Wrap existing bytes (e.g. a fixture file's contents).
    pub fn from_bytes(data: Vec<u8>) -> MemStorage {
        MemStorage {
            inner: Rc::new(RefCell::new(MemInner {
                data,
                journal: None,
            })),
        }
    }

    /// Snapshot the main file's current persisted bytes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.borrow().data.clone()
    }

    /// Snapshot the journal's persisted bytes, if a journal exists.
    pub fn journal_snapshot(&self) -> Option<Vec<u8>> {
        self.inner.borrow().journal.clone()
    }

    /// Replace the persisted main bytes (test/crash tooling).
    pub fn set_snapshot(&self, data: Vec<u8>) {
        self.inner.borrow_mut().data = data;
    }

    /// Replace/clear the persisted journal (test/crash tooling).
    pub fn set_journal(&self, journal: Option<Vec<u8>>) {
        self.inner.borrow_mut().journal = journal;
    }

    pub fn journal_exists(&self) -> bool {
        self.inner.borrow().journal.is_some()
    }
}

impl Default for MemStorage {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MemSidecar {
    inner: Rc<RefCell<MemInner>>,
    existed: bool,
}

fn read_exact(data: &[u8], offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
    let off = offset as usize;
    let end = off.checked_add(buf.len()).ok_or(MemError)?;
    if end > data.len() {
        return Err(MemError);
    }
    buf.copy_from_slice(&data[off..end]);
    Ok(())
}

fn write_exact(data: &mut Vec<u8>, offset: u64, buf: &[u8]) -> Result<(), MemError> {
    let off = offset as usize;
    let end = off.checked_add(buf.len()).ok_or(MemError)?;
    if end > data.len() {
        data.resize(end, 0);
    }
    data[off..end].copy_from_slice(buf);
    Ok(())
}

impl SidecarFile for MemSidecar {
    type Error = MemError;

    fn existed(&self) -> bool {
        self.existed
    }
    async fn len(&self) -> Result<u64, MemError> {
        Ok(self
            .inner
            .borrow()
            .journal
            .as_ref()
            .map_or(0, |j| j.len() as u64))
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let inner = self.inner.borrow();
        match &inner.journal {
            Some(j) => read_exact(j, offset, buf),
            None => {
                if buf.is_empty() {
                    Ok(())
                } else {
                    Err(MemError)
                }
            }
        }
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), MemError> {
        let mut inner = self.inner.borrow_mut();
        if inner.journal.is_none() {
            inner.journal = Some(Vec::new());
        }
        write_exact(inner.journal.as_mut().expect("just set"), offset, buf)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), MemError> {
        let mut inner = self.inner.borrow_mut();
        if inner.journal.is_none() {
            inner.journal = Some(Vec::new());
        }
        inner
            .journal
            .as_mut()
            .expect("just set")
            .resize(len as usize, 0);
        Ok(())
    }
    async fn sync(&mut self) -> Result<(), MemError> {
        Ok(())
    }
    async fn delete(&mut self) -> Result<(), MemError> {
        self.inner.borrow_mut().journal = None;
        Ok(())
    }
}

impl Storage for MemStorage {
    type Error = MemError;
    type Sidecar = MemSidecar;

    async fn len(&self) -> Result<u64, MemError> {
        Ok(self.inner.borrow().data.len() as u64)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        read_exact(&self.inner.borrow().data, offset, buf)
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), MemError> {
        write_exact(&mut self.inner.borrow_mut().data, offset, buf)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), MemError> {
        self.inner.borrow_mut().data.resize(len as usize, 0);
        Ok(())
    }
    async fn sync(&mut self) -> Result<(), MemError> {
        Ok(())
    }
    async fn sidecar(&mut self, _kind: Sidecar) -> Result<MemSidecar, MemError> {
        let existed = self.inner.borrow().journal.is_some();
        Ok(MemSidecar {
            inner: Rc::clone(&self.inner),
            existed,
        })
    }
}

// ------------------------------------------------------------------
// FileStorage — hosted files over std::fs (feature = "std").
// Futures complete immediately over blocking calls (SPEC §10).
// ------------------------------------------------------------------

#[cfg(feature = "std")]
mod file_storage {
    use super::{Sidecar, SidecarFile, Storage};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    #[derive(Debug)]
    pub struct FileError;

    impl From<std::io::Error> for FileError {
        fn from(_: std::io::Error) -> Self {
            FileError
        }
    }

    fn read_exact(f: &File, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
        let mut f = f;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)?;
        Ok(())
    }
    fn write_exact(f: &mut File, offset: u64, buf: &[u8]) -> Result<(), FileError> {
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(buf)?;
        Ok(())
    }

    pub struct FileSidecar {
        path: PathBuf,
        file: Option<File>,
        existed: bool,
    }

    impl FileSidecar {
        fn ensure(&mut self) -> Result<&mut File, FileError> {
            if self.file.is_none() {
                self.file = Some(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&self.path)?,
                );
            }
            Ok(self.file.as_mut().expect("just set"))
        }
    }

    impl SidecarFile for FileSidecar {
        type Error = FileError;
        fn existed(&self) -> bool {
            self.existed
        }
        async fn len(&self) -> Result<u64, FileError> {
            match std::fs::metadata(&self.path) {
                Ok(m) => Ok(m.len()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(e) => Err(e.into()),
            }
        }
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
            match &self.file {
                Some(f) => read_exact(f, offset, buf),
                None => {
                    // Not yet created on disk.
                    if buf.is_empty() {
                        Ok(())
                    } else {
                        Err(FileError)
                    }
                }
            }
        }
        async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FileError> {
            let f = self.ensure()?;
            write_exact(f, offset, buf)
        }
        async fn set_len(&mut self, len: u64) -> Result<(), FileError> {
            let f = self.ensure()?;
            f.set_len(len)?;
            Ok(())
        }
        async fn sync(&mut self) -> Result<(), FileError> {
            if let Some(f) = &self.file {
                f.sync_all()?;
            }
            Ok(())
        }
        async fn delete(&mut self) -> Result<(), FileError> {
            self.file = None;
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }

    /// Hosted storage over a real file. The journal sidecar is the
    /// sibling path `<db>-journal`, exactly as SQLite names it.
    pub struct FileStorage {
        file: File,
        path: PathBuf,
    }

    impl FileStorage {
        pub fn open(path: impl Into<PathBuf>) -> Result<FileStorage, FileError> {
            let path = path.into();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            Ok(FileStorage { file, path })
        }

        fn journal_path(&self) -> PathBuf {
            let mut s = self.path.clone().into_os_string();
            s.push("-journal");
            PathBuf::from(s)
        }
    }

    impl Storage for FileStorage {
        type Error = FileError;
        type Sidecar = FileSidecar;

        async fn len(&self) -> Result<u64, FileError> {
            Ok(self.file.metadata()?.len())
        }
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
            read_exact(&self.file, offset, buf)
        }
        async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FileError> {
            write_exact(&mut self.file, offset, buf)
        }
        async fn set_len(&mut self, len: u64) -> Result<(), FileError> {
            self.file.set_len(len)?;
            Ok(())
        }
        async fn sync(&mut self) -> Result<(), FileError> {
            self.file.sync_all()?;
            Ok(())
        }
        async fn sidecar(&mut self, _kind: Sidecar) -> Result<FileSidecar, FileError> {
            let path = self.journal_path();
            let existed = path.exists();
            let file = if existed {
                Some(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(false)
                        .open(&path)?,
                )
            } else {
                None
            };
            Ok(FileSidecar {
                path,
                file,
                existed,
            })
        }
    }
}

#[cfg(feature = "std")]
pub use file_storage::{FileError, FileSidecar, FileStorage};

/// A trivial executor for tests and examples: poll with a no-op
/// waker until Ready. For always-Ready devices this is exactly the
/// right driver (SPEC §5.1.1); wrappers that return `Pending` a
/// bounded number of times also complete under it.
pub fn block_on<F: core::future::Future>(fut: F) -> F::Output {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    fn drop_w(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, drop_w);
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => core::hint::spin_loop(),
        }
    }
}
