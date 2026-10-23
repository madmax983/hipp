//! Remedy-named, string-free error types (SPEC §5.8).
//!
//! Every failure in the core is a `Result` with one of these variants.
//! The generic parameter `E` is the caller's `Storage` error type; the
//! engine wraps it (`Error::Storage`), never replaces it.

/// Which arena scope ran out (SPEC §7.5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Statement,
    Transaction,
    Catalog,
}

/// Sub-kind for `Error::Schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    UnknownTable,
    UnknownColumn,
    DuplicateTable,
    DuplicateRowid,
    ColumnCountMismatch,
    DatatypeMismatch,
    MissingRowidValue,
    /// A `UNIQUE` index / constraint would be violated (v0.3).
    UniqueViolation,
    /// `DROP INDEX` named an index that does not exist (v0.3).
    UnknownIndex,
    /// `DROP INDEX` named an index owned by a UNIQUE / PRIMARY KEY
    /// constraint, which SQLite refuses to drop (v0.3).
    DropAutoIndex,
    /// `CREATE INDEX` named an index that already exists (v0.3).
    DuplicateIndex,
    /// An unqualified column name matches more than one FROM
    /// source (v0.4).
    AmbiguousColumn,
    /// `ALTER TABLE ... ADD COLUMN` named a column the table
    /// already has (v0.4).
    DuplicateColumn,
    /// `ALTER TABLE ... ADD COLUMN` with a definition SQLite
    /// forbids adding: PRIMARY KEY / UNIQUE, a generated column,
    /// NOT NULL without a non-NULL constant default, or a
    /// non-constant default (v0.4).
    CannotAddColumn,
    /// `DROP VIEW` named a view that does not exist (v0.4).
    UnknownView,
    /// `CREATE VIEW` named an object that already exists (v0.4).
    DuplicateView,
}

/// Sub-kind for `Error::Parse` — no strings, just the class of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseKind {
    UnexpectedToken,
    UnexpectedEof,
    InvalidLiteral,
    UnsupportedStatement,
    UnsupportedClause,
    RecursionLimit,
    TrailingInput,
}

/// The core error type. Generic over the storage error `E`.
#[derive(Debug)]
pub enum Error<E> {
    /// First 16 bytes are not `SQLite format 3\0`.
    BadMagic,
    /// File shorter than the 100-byte database header.
    TruncatedHeader,
    /// Header page-size field outside 512..=65536 powers of two.
    UnsupportedPageSize,
    /// Text encoding 2/3 (UTF-16) — SPEC §3.7.
    UnsupportedEncoding,
    /// Header read/write version 2 (WAL) — SPEC §3.1.
    WalUnsupported,
    /// Reserved-bytes-per-page != 0 on a write path.
    ReservedBytesUnsupported,
    /// A page's bytes violate the format.
    CorruptPage { page: u32 },
    /// A record's bytes violate the format.
    CorruptRecord,
    /// Truncated or overlong varint.
    BadVarint,
    /// Serial type 10/11 (reserved) encountered.
    BadSerialType,
    /// Overflow chain loops back on itself.
    OverflowChainCycle,
    /// Page number beyond the database size.
    PageOutOfRange { page: u32 },
    /// A feature a foreign file uses that v0.1 does not implement.
    Unsupported,
    /// Schema-layer failure (unknown/duplicate names, count mismatches).
    Schema { kind: SchemaKind },
    /// SQL outside the v0.1 grammar; `offset` is a byte offset into the SQL.
    Parse { offset: usize, kind: ParseKind },
    /// All cache frames pinned/in use (SPEC §5.2).
    CacheExhausted,
    /// A second driver for an in-flight transaction stage (SPEC §5.1.2).
    OperationInFlight,
    /// Transaction-control statement in the wrong state (v0.2):
    /// `BEGIN` while an explicit transaction is already open
    /// (`nested: true`), or `COMMIT`/`ROLLBACK` with none open.
    TransactionState { nested: bool },
    /// An arena hit its configured maximum (SPEC §7.5.4).
    ArenaExhausted { scope: Scope, needed: usize },
    /// The rollback journal is malformed beyond safe recovery.
    CorruptJournal,
    /// The caller's storage device error.
    Storage(E),
}

impl<E> Error<E> {
    /// Re-wrap this error for a different storage error type. Only valid
    /// for errors that are not `Storage` — internal helpers use
    /// `Error<()>` and never construct the `Storage` variant, so the
    /// `Storage` arm maps the unit value through `into`, which is
    /// unreachable in practice but must still type-check.
    pub fn generalize<F>(self) -> Error<F>
    where
        E: Into<F>,
    {
        match self {
            Error::BadMagic => Error::BadMagic,
            Error::TruncatedHeader => Error::TruncatedHeader,
            Error::UnsupportedPageSize => Error::UnsupportedPageSize,
            Error::UnsupportedEncoding => Error::UnsupportedEncoding,
            Error::WalUnsupported => Error::WalUnsupported,
            Error::ReservedBytesUnsupported => Error::ReservedBytesUnsupported,
            Error::CorruptPage { page } => Error::CorruptPage { page },
            Error::CorruptRecord => Error::CorruptRecord,
            Error::BadVarint => Error::BadVarint,
            Error::BadSerialType => Error::BadSerialType,
            Error::OverflowChainCycle => Error::OverflowChainCycle,
            Error::PageOutOfRange { page } => Error::PageOutOfRange { page },
            Error::Unsupported => Error::Unsupported,
            Error::Schema { kind } => Error::Schema { kind },
            Error::Parse { offset, kind } => Error::Parse { offset, kind },
            Error::CacheExhausted => Error::CacheExhausted,
            Error::OperationInFlight => Error::OperationInFlight,
            Error::TransactionState { nested } => Error::TransactionState { nested },
            Error::ArenaExhausted { scope, needed } => Error::ArenaExhausted { scope, needed },
            Error::CorruptJournal => Error::CorruptJournal,
            Error::Storage(e) => Error::Storage(e.into()),
        }
    }
}

/// Arena exhaustion in isolation (arena.rs returns this; it converts
/// into `Error::ArenaExhausted` for any storage error type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaError {
    pub scope: Scope,
    pub needed: usize,
}

impl<E> From<ArenaError> for Error<E> {
    fn from(a: ArenaError) -> Self {
        Error::ArenaExhausted {
            scope: a.scope,
            needed: a.needed,
        }
    }
}

pub type Result<T, E> = core::result::Result<T, Error<E>>;
