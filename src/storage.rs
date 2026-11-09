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

/// Which sidecar file is being opened. v0.1 needs exactly one kind;
/// the `wal` leaf (v0.16) adds the write-ahead log and the pin's
/// shared-memory index file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sidecar {
    /// `<db>-journal`: the rollback journal (and, in WAL mode, the
    /// savepoint scratch area).
    RollbackJournal,
    /// `<db>-wal`: the write-ahead log (v0.16, `wal`).
    #[cfg(feature = "wal")]
    Wal,
    /// `<db>-shm`: the pin's wal-index file (v0.16, `wal`). hipp
    /// never writes it (its wal-index lives in heap memory, as the
    /// pin's does in `locking_mode=EXCLUSIVE`); it only asks whether
    /// one exists and deletes a stale one at close, as the pin's last
    /// connection does.
    #[cfg(feature = "wal")]
    Shm,
}

/// A sidecar file (the rollback journal in v0.1): the same byte
/// shape as the main file plus `delete` and an existence record,
/// because the journal protocol ends by deleting the journal and
/// recovery must tell a pre-existing journal from a freshly created
/// empty one.
// `async fn` in trait is deliberate (SPEC §5.1.1): RPITIT, no
// boxing, no `async-trait` dependency; futures need not be Send.
// `len` is a byte count of a file, not a collection length:
// no `is_empty` belongs in this contract.
#[allow(async_fn_in_trait, clippy::len_without_is_empty)]
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
#[allow(async_fn_in_trait, clippy::len_without_is_empty)]
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
    /// The database's file name as `PRAGMA database_list` shows it
    /// (v0.16): the absolute path of a file, empty (the default)
    /// for storage without one.
    fn file_name(&self) -> &[u8] {
        &[]
    }
    /// Whether a non-empty write-ahead log (`<db>-wal`) sits beside
    /// this database (v0.16). Consulted only by builds without the
    /// `wal` leaf, which then refuse the database
    /// (`Error::WalUnsupported`): the pin opens such a database in
    /// WAL mode whatever its header says, so its committed pages may
    /// be in the log and the main file stale. With the leaf the log
    /// is read through `Sidecar::Wal`. The default, for storage that
    /// keeps no log, is `false`; `FileStorage` looks for the file.
    fn log_beside(&self) -> bool {
        false
    }
    /// Claim the database for this connection's WAL mode (v0.16,
    /// `wal`), called when a database opens in WAL mode and when it
    /// enters it. `false` when another connection holds it: a live
    /// SQLite connection, whose shared-memory wal-index hipp cannot
    /// take part in, or another hipp connection (in this process or
    /// another), whose heap wal-index this one would not see — its
    /// close would checkpoint and delete the log this one appends
    /// to. hipp then refuses with `Error::DatabaseBusy`. The claim
    /// lasts until `release_wal`, or until the storage is dropped.
    /// The default, for storage only one connection can reach,
    /// claims nothing and returns `true`.
    #[cfg(feature = "wal")]
    fn claim_wal(&mut self) -> bool {
        true
    }
    /// Release a `claim_wal` hold (v0.16, `wal`): the database left
    /// WAL mode or closed. The default does nothing.
    #[cfg(feature = "wal")]
    fn release_wal(&mut self) {}
    /// This storage is an in-memory database (v0.16, the pin's
    /// `:memory:`): `PRAGMA journal_mode` reports and keeps `memory`,
    /// so it never enters WAL mode. The default is `false`, a file;
    /// `MemStorage::new` models a file too (the test and
    /// crash-injection workhorse), `MemStorage::memory_db` makes an
    /// in-memory database.
    #[cfg(feature = "sql")]
    fn is_memory(&self) -> bool {
        false
    }
    /// Open the storage of a database `ATTACH` names (v0.16,
    /// `attach`), or `None` when this kind of storage cannot reach
    /// it (the statement then fails with
    /// `SchemaKind::CannotOpenDatabase`, the pin's "unable to open
    /// database"). Consulted when the connection has no
    /// `Connection::set_attach_hook`. The default reaches nothing;
    /// `MemStorage` makes `:memory:` and temporary databases in RAM,
    /// `FileStorage` opens files (relative names against the working
    /// directory, as the pin does) and backs `:memory:` and `''` with
    /// a private temporary file removed when it is dropped.
    #[cfg(feature = "attach")]
    fn open_attached(&self, target: AttachTarget<'_>) -> Option<Self>
    where
        Self: Sized,
    {
        let _ = target;
        None
    }
    /// Open (creating it only when first written) the file named
    /// `name` — a full path as `file_name` gives them, plus a suffix
    /// — as a sidecar-shaped file (v0.16): the pin's super-journal
    /// (`<main>-mjXXXXXX9XX`), which a multi-database commit writes
    /// and hot-journal recovery looks for, and another database's
    /// journal named in one. `Ok(None)` when this storage has no such
    /// namespace (the default): such storage writes no super-journal
    /// (a commit of several databases is then atomic per database
    /// only, as the pin's is for a database without a file name), and
    /// recovery rolls back a journal that names one.
    async fn open_named(&mut self, name: &[u8]) -> Result<Option<Self::Sidecar>, Self::Error> {
        let _ = name;
        Ok(None)
    }
}

/// What an `ATTACH` asks storage for (v0.16, `attach`;
/// `Storage::open_attached`, `Connection::set_attach_hook`).
#[cfg(feature = "attach")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTarget<'a> {
    /// A file, by the name the ATTACH expression gave (its text as
    /// written; not necessarily UTF-8).
    File(&'a [u8]),
    /// `':memory:'`: a private in-memory database (journal mode
    /// `memory`, no file name).
    Memory,
    /// `''` or NULL: a private temporary database, gone once it is
    /// detached (no file name).
    Temporary,
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
    /// The `-wal` sidecar (v0.16, `wal`).
    #[cfg(feature = "wal")]
    wal: Option<Vec<u8>>,
    /// The `-shm` sidecar (v0.16, `wal`): only tests create one.
    #[cfg(feature = "wal")]
    shm: Option<Vec<u8>>,
}

/// In-memory `Storage`. Cloning shares the same underlying bytes —
/// that is deliberate: a test can keep a handle to the "device" while
/// the engine owns another, and crash wrappers snapshot through it.
#[derive(Clone)]
pub struct MemStorage {
    inner: Rc<RefCell<MemInner>>,
    /// An in-memory database rather than a model of a file (v0.16,
    /// `Storage::is_memory`).
    #[cfg(feature = "sql")]
    memory: bool,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage {
            inner: Rc::new(RefCell::new(MemInner {
                data: Vec::new(),
                journal: None,
                #[cfg(feature = "wal")]
                wal: None,
                #[cfg(feature = "wal")]
                shm: None,
            })),
            #[cfg(feature = "sql")]
            memory: false,
        }
    }

    /// Wrap existing bytes (e.g. a fixture file's contents).
    pub fn from_bytes(data: Vec<u8>) -> MemStorage {
        MemStorage {
            inner: Rc::new(RefCell::new(MemInner {
                data,
                journal: None,
                #[cfg(feature = "wal")]
                wal: None,
                #[cfg(feature = "wal")]
                shm: None,
            })),
            #[cfg(feature = "sql")]
            memory: false,
        }
    }

    /// An in-memory database (v0.16): the pin's `:memory:`, whose
    /// journal mode is `memory` (`Storage::is_memory`).
    #[cfg(feature = "sql")]
    pub fn memory_db() -> MemStorage {
        MemStorage {
            memory: true,
            ..MemStorage::new()
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

    /// Snapshot the `-wal` sidecar's bytes, if it exists (v0.16,
    /// `wal`; test/crash tooling).
    #[cfg(feature = "wal")]
    pub fn wal_snapshot(&self) -> Option<Vec<u8>> {
        self.inner.borrow().wal.clone()
    }

    /// Replace/clear the `-wal` sidecar (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn set_wal(&self, wal: Option<Vec<u8>>) {
        self.inner.borrow_mut().wal = wal;
    }

    /// Replace/clear the `-shm` sidecar (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn set_shm(&self, shm: Option<Vec<u8>>) {
        self.inner.borrow_mut().shm = shm;
    }

    /// Whether a `-shm` sidecar exists (v0.16, `wal`).
    #[cfg(feature = "wal")]
    pub fn shm_exists(&self) -> bool {
        self.inner.borrow().shm.is_some()
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
    /// Which sidecar this is (v0.16, `wal`; always the journal
    /// without it).
    #[cfg(feature = "wal")]
    kind: Sidecar,
}

impl MemSidecar {
    /// This sidecar's bytes in `inner`.
    #[inline(always)]
    fn get<'a>(&self, inner: &'a MemInner) -> &'a Option<Vec<u8>> {
        #[cfg(feature = "wal")]
        match self.kind {
            Sidecar::Wal => return &inner.wal,
            Sidecar::Shm => return &inner.shm,
            Sidecar::RollbackJournal => {}
        }
        &inner.journal
    }
    #[inline(always)]
    fn get_mut<'a>(&self, inner: &'a mut MemInner) -> &'a mut Option<Vec<u8>> {
        #[cfg(feature = "wal")]
        match self.kind {
            Sidecar::Wal => return &mut inner.wal,
            Sidecar::Shm => return &mut inner.shm,
            Sidecar::RollbackJournal => {}
        }
        &mut inner.journal
    }
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
            .get(&self.inner.borrow())
            .as_ref()
            .map_or(0, |j| j.len() as u64))
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let inner = self.inner.borrow();
        match self.get(&inner) {
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
        write_exact(
            self.get_mut(&mut inner).get_or_insert_with(Vec::new),
            offset,
            buf,
        )
    }
    async fn set_len(&mut self, len: u64) -> Result<(), MemError> {
        let mut inner = self.inner.borrow_mut();
        self.get_mut(&mut inner)
            .get_or_insert_with(Vec::new)
            .resize(len as usize, 0);
        Ok(())
    }
    async fn sync(&mut self) -> Result<(), MemError> {
        Ok(())
    }
    async fn delete(&mut self) -> Result<(), MemError> {
        *self.get_mut(&mut self.inner.borrow_mut()) = None;
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
    #[cfg(feature = "sql")]
    fn is_memory(&self) -> bool {
        self.memory
    }
    /// `:memory:` and temporary databases live in RAM (a temporary
    /// one models a file, as `MemStorage::new` does); a file name
    /// reaches nothing, there being no file system.
    #[cfg(feature = "attach")]
    fn open_attached(&self, target: AttachTarget<'_>) -> Option<MemStorage> {
        match target {
            AttachTarget::Memory => Some(MemStorage::memory_db()),
            AttachTarget::Temporary => Some(MemStorage::new()),
            AttachTarget::File(_) => None,
        }
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<MemSidecar, MemError> {
        let _ = kind;
        let sc = MemSidecar {
            inner: Rc::clone(&self.inner),
            existed: false,
            #[cfg(feature = "wal")]
            kind,
        };
        let existed = sc.get(&self.inner.borrow()).is_some();
        Ok(MemSidecar { existed, ..sc })
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
        /// This handle created the file, and its directory entry is
        /// not yet synced: the first `sync` syncs the directory too,
        /// as the pin's `unixSync` does for a journal, super-journal
        /// or log it created (v0.16 review fix).
        dir_pending: bool,
        /// A file opened by name (`Storage::open_named`: the
        /// super-journal): created exclusively, and its deletion — a
        /// multi-database commit's commit point — synced in its
        /// directory, as the pin's `unixDelete(.., dirSync=1)`.
        named: bool,
    }

    /// Sync the directory holding `path` (best effort, as the pin's:
    /// a directory that cannot be opened is skipped).
    fn sync_dir(path: &std::path::Path) {
        #[cfg(unix)]
        {
            let dir = match path.parent() {
                Some(d) if !d.as_os_str().is_empty() => d,
                _ => std::path::Path::new("."),
            };
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    impl FileSidecar {
        fn ensure(&mut self) -> Result<&mut File, FileError> {
            if self.file.is_none() {
                let created = !self.path.exists();
                let mut o = OpenOptions::new();
                o.read(true).write(true);
                if created && self.named {
                    o.create_new(true);
                } else {
                    o.create(true).truncate(false);
                }
                self.file = Some(o.open(&self.path)?);
                self.dir_pending = created;
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
                if self.dir_pending {
                    sync_dir(&self.path);
                    self.dir_pending = false;
                }
            }
            Ok(())
        }
        async fn delete(&mut self) -> Result<(), FileError> {
            self.file = None;
            self.dir_pending = false;
            match std::fs::remove_file(&self.path) {
                Ok(()) => {
                    if self.named {
                        sync_dir(&self.path);
                    }
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }

    /// The hosted `VACUUM INTO` writer (v0.16, `sql`; the default
    /// `Config::vacuum_into` with `std`): writes `image` to the
    /// file named `name` (UTF-8, relative to the working
    /// directory) and syncs it. An existing non-empty file is
    /// left alone and reported as `false`, as the pin refuses
    /// one; so is a name that is not UTF-8 or a failed write.
    #[cfg(feature = "sql")]
    pub fn write_new_file(name: &[u8], image: &[u8]) -> bool {
        let Ok(path) = core::str::from_utf8(name) else {
            return false;
        };
        let Ok(mut f) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        else {
            return false;
        };
        if f.metadata().map(|m| m.len() != 0).unwrap_or(true) {
            return false;
        }
        f.write_all(image).is_ok() && f.sync_all().is_ok()
    }

    /// Hosted storage over a real file. The journal sidecar is the
    /// sibling path `<db>-journal`, exactly as SQLite names it.
    pub struct FileStorage {
        file: File,
        path: PathBuf,
        /// The absolute path, as text (v0.16: `PRAGMA
        /// database_list`).
        full: std::vec::Vec<u8>,
        /// A private temporary file standing for a `:memory:` (`Some(true)`)
        /// or temporary (`Some(false)`) database (v0.16, `attach`;
        /// `FileStorage::anonymous`): it has no name to show, and it
        /// and its sidecars are removed when the storage is dropped.
        anon: Option<bool>,
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
            let full = std::fs::canonicalize(&path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            Ok(FileStorage {
                file,
                path,
                full,
                anon: None,
            })
        }

        /// A private database in a fresh file of the system's
        /// temporary directory (v0.16): with `memory`, the pin's
        /// `:memory:` (`Storage::is_memory`: journal mode `memory`),
        /// else the pin's temporary database (`ATTACH '' AS x`).
        /// Either has an empty file name (`PRAGMA database_list`
        /// shows none), and the file and its sidecars are removed
        /// when the storage is dropped.
        pub fn anonymous(memory: bool) -> Result<FileStorage, FileError> {
            let dir = std::env::temp_dir();
            let pid = std::process::id();
            let mut seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
                ^ (u64::from(pid) << 32);
            for _ in 0..64 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let path = dir.join(std::format!("hipp-{pid}-{seed:016x}.db"));
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(file) => {
                        return Ok(FileStorage {
                            file,
                            path,
                            full: std::vec::Vec::new(),
                            anon: Some(memory),
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            Err(FileError)
        }

        fn journal_path(&self) -> PathBuf {
            let mut s = self.path.clone().into_os_string();
            s.push("-journal");
            PathBuf::from(s)
        }

        /// `<db><suffix>` (v0.16: `-wal`, `-shm`).
        fn sibling(&self, suffix: &str) -> PathBuf {
            let mut s = self.path.clone().into_os_string();
            s.push(suffix);
            PathBuf::from(s)
        }
    }

    impl Drop for FileStorage {
        /// A private temporary database (`FileStorage::anonymous`)
        /// leaves nothing behind.
        fn drop(&mut self) {
            if self.anon.is_some() {
                let _ = std::fs::remove_file(&self.path);
                for suffix in ["-journal", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(self.sibling(suffix));
                }
            }
        }
    }

    /// Whether a process other than this one holds a POSIX lock on
    /// any of `paths` (v0.16, `wal`; Linux: read from `/proc/locks`,
    /// whose lines name the locked file as `MAJOR:MINOR:INODE`, the
    /// device numbers in hex). A live SQLite connection holds one on
    /// its database in WAL mode (and on the `-shm`). Anything that
    /// cannot be read counts as unlocked.
    #[cfg(all(feature = "wal", target_os = "linux"))]
    fn posix_locked(paths: &[PathBuf]) -> bool {
        use std::os::unix::fs::MetadataExt;
        let ids: std::vec::Vec<(u64, u64, u64)> = paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| {
                let d = m.dev();
                let major = ((d >> 8) & 0xfff) | ((d >> 32) & !0xfff);
                let minor = (d & 0xff) | ((d >> 12) & !0xff);
                (major, minor, m.ino())
            })
            .collect();
        if ids.is_empty() {
            return false;
        }
        let Ok(text) = std::fs::read_to_string("/proc/locks") else {
            return false;
        };
        let me = u64::from(std::process::id());
        text.lines().any(|line| {
            let f: std::vec::Vec<&str> = line.split_whitespace().collect();
            // "1: POSIX  ADVISORY  READ 966 fe:00:3287306 128 128"
            let (Some(kind), Some(pid), Some(id)) = (f.get(1), f.get(4), f.get(5)) else {
                return false;
            };
            if *kind != "POSIX" || pid.parse::<u64>().ok() == Some(me) {
                return false;
            }
            let mut parts = id.split(':');
            let (Some(a), Some(b), Some(c)) = (parts.next(), parts.next(), parts.next()) else {
                return false;
            };
            let (Ok(a), Ok(b), Ok(c)) = (
                u64::from_str_radix(a, 16),
                u64::from_str_radix(b, 16),
                c.parse::<u64>(),
            ) else {
                return false;
            };
            ids.contains(&(a, b, c))
        })
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
        fn file_name(&self) -> &[u8] {
            &self.full
        }
        #[cfg(feature = "sql")]
        fn is_memory(&self) -> bool {
            self.anon == Some(true)
        }
        /// A file by name, relative to the working directory, as the
        /// pin opens one; `:memory:` and `''` as a private temporary
        /// file (`FileStorage::anonymous`).
        #[cfg(feature = "attach")]
        fn open_attached(&self, target: super::AttachTarget<'_>) -> Option<FileStorage> {
            match target {
                super::AttachTarget::Memory => FileStorage::anonymous(true).ok(),
                super::AttachTarget::Temporary => FileStorage::anonymous(false).ok(),
                super::AttachTarget::File(name) => {
                    #[cfg(unix)]
                    let path = {
                        use std::os::unix::ffi::OsStrExt;
                        PathBuf::from(std::ffi::OsStr::from_bytes(name))
                    };
                    #[cfg(not(unix))]
                    let path = PathBuf::from(core::str::from_utf8(name).ok()?);
                    FileStorage::open(path).ok()
                }
            }
        }
        async fn open_named(&mut self, name: &[u8]) -> Result<Option<FileSidecar>, FileError> {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                PathBuf::from(std::ffi::OsStr::from_bytes(name))
            };
            #[cfg(not(unix))]
            let path = match core::str::from_utf8(name) {
                Ok(p) => PathBuf::from(p),
                Err(_) => return Ok(None),
            };
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
            Ok(Some(FileSidecar {
                path,
                file,
                existed,
                dir_pending: false,
                named: true,
            }))
        }
        fn log_beside(&self) -> bool {
            std::fs::metadata(self.sibling("-wal")).is_ok_and(|m| m.len() > 0)
        }
        /// A live SQLite connection is seen by its POSIX lock (Linux:
        /// `/proc/locks`); hipp connections, this process's included,
        /// by an exclusive `flock` on the database file (Unix), which
        /// SQLite's POSIX locks neither take nor see.
        #[cfg(feature = "wal")]
        fn claim_wal(&mut self) -> bool {
            #[cfg(target_os = "linux")]
            if posix_locked(&[self.path.clone(), self.sibling("-shm")]) {
                return false;
            }
            #[cfg(unix)]
            if let Err(std::fs::TryLockError::WouldBlock) = self.file.try_lock() {
                return false;
            }
            true
        }
        #[cfg(feature = "wal")]
        fn release_wal(&mut self) {
            #[cfg(unix)]
            let _ = self.file.unlock();
        }
        async fn sidecar(&mut self, kind: Sidecar) -> Result<FileSidecar, FileError> {
            let path = match kind {
                Sidecar::RollbackJournal => self.journal_path(),
                #[cfg(feature = "wal")]
                Sidecar::Wal => self.sibling("-wal"),
                #[cfg(feature = "wal")]
                Sidecar::Shm => self.sibling("-shm"),
            };
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
                dir_pending: false,
                named: false,
            })
        }
    }
}

#[cfg(all(feature = "std", feature = "sql"))]
pub use file_storage::write_new_file;
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
