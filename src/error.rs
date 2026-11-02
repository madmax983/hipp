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
    /// A `NOT NULL` column would receive a NULL (v0.10). Raised
    /// for the statement conflict actions that error (ABORT /
    /// FAIL / ROLLBACK and the default), for an OR REPLACE
    /// substitution whose column default is itself NULL, and
    /// unconditionally for an upsert DO UPDATE result row.
    NotNullViolation,
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
    /// A statement that needs a table named a view instead:
    /// `DROP TABLE` / `UPDATE` / `DELETE` on a view (v0.5; the
    /// oracle refuses these even under IF EXISTS).
    IsView,
    /// `DROP VIEW` named a table instead of a view (v0.5; the
    /// oracle refuses this even under IF EXISTS).
    IsTable,
    /// A `USING (col)` / NATURAL join column that is missing from
    /// the joined source or from every source on its left
    /// (v0.16; the pin's "cannot join using column ... not
    /// present in both tables"). Name a column both sides have.
    UsingColumnMissing,
    /// An ON clause names a source joined after it (v0.16): an
    /// outer join's ON (LEFT / RIGHT / FULL) never may, and with
    /// a RIGHT / FULL join anywhere in the FROM clause no ON may
    /// — the pin's "ON clause references tables to its right".
    /// Move the term to WHERE or to a later ON.
    OnReferencesLaterSource,
    /// An ORDER BY / GROUP BY ordinal outside 1..=result width
    /// (v0.16; the pin's "term out of range"). Use a valid
    /// column number or an expression.
    TermOutOfRange,
    /// An aggregate where the pin forbids one (v0.16): in a
    /// GROUP BY term (directly, or through an ordinal / alias
    /// naming an aggregate result column) or in LIMIT / OFFSET.
    MisusedAggregate,
    /// A HAVING clause on a query that is not an aggregate query
    /// — no GROUP BY and no aggregate in the result list (v0.16;
    /// the pin refuses it). Add a GROUP BY or use WHERE.
    HavingWithoutAggregate,
    /// A row failed one of its table's CHECK constraints
    /// (v0.16): the constraint evaluated to false — NULL passes.
    /// Raised under every conflict action except OR IGNORE
    /// (which skips the row); OR REPLACE does not resolve it.
    CheckViolation,
    /// A column DEFAULT that is not constant (v0.16; the pin's
    /// "default value of column is not constant"): it names a
    /// column or contains a subquery. Use a literal or a
    /// constant expression in parentheses.
    NotConstantDefault,
    /// Generated columns that depend on each other in a cycle
    /// (v0.16; the pin's "generated column loop").
    GeneratedColumnLoop,
    /// A generated column the pin refuses to define (v0.16): a
    /// DEFAULT on it, a PRIMARY KEY on it, a qualified (`t.c`)
    /// reference or `rowid` in its expression, or a table with
    /// no non-generated column. Drop the offending clause.
    InvalidGeneratedColumn,
    /// A subquery inside a CHECK constraint or generated column
    /// (v0.16; the pin prohibits both).
    SubqueryInConstraint,
    /// An INSERT column list / UPDATE SET / upsert DO UPDATE
    /// SET names a generated column (v0.16; the pin's "cannot
    /// INSERT/UPDATE generated column"). Write its source
    /// columns instead.
    GeneratedColumnWrite,
    /// A STRICT table column received a value its declared type
    /// cannot hold after the type's conversion (v0.16; the
    /// pin's "cannot store TEXT value in INT column").
    StrictTypeViolation,
    /// A STRICT table column without a type, or with one other
    /// than INT / INTEGER / REAL / TEXT / BLOB / ANY (v0.16;
    /// the pin's "missing / unknown datatype"). Declare one of
    /// the six.
    StrictColumnType,
    /// An UPDATE ... FROM whose FROM clause repeats the target
    /// table under the target's alias (or, both unaliased, bare)
    /// (v0.16; the pin's "target object/alias may not appear in
    /// FROM clause"). Give the FROM clause's copy its own
    /// alias: `FROM t AS t2`.
    UpdateTargetInFrom,
    /// A `COLLATE` clause or operator names a collating
    /// sequence other than BINARY, NOCASE or RTRIM (v0.16; the
    /// pin's "no such collation sequence"). Use one of the
    /// three built-ins.
    UnknownCollation,
    /// A LIKE `ESCAPE` operand (or `like()`'s third argument)
    /// that is not exactly one character (v0.16; the pin's
    /// "ESCAPE expression must be a single character"). Pass a
    /// one-character escape.
    EscapeNotSingleChar,
    /// A LIKE / GLOB pattern longer than 50,000 bytes (v0.16;
    /// the pin's default `SQLITE_LIMIT_LIKE_PATTERN_LENGTH`,
    /// "LIKE or GLOB pattern too complex"). Shorten the pattern.
    PatternTooComplex,
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
    /// A join operator whose keywords do not form a join type
    /// (v0.16): `OUTER JOIN` alone, `INNER OUTER JOIN`, `LEFT
    /// CROSS JOIN`, more than three keywords — the pin's
    /// "unknown join type". Write LEFT / RIGHT / FULL [OUTER],
    /// [INNER | CROSS], optionally after NATURAL.
    UnknownJoinType,
    /// A NATURAL join that also has an ON or USING clause
    /// (v0.16; the pin refuses it). Drop NATURAL or the clause.
    NaturalJoinConstraint,
    /// A CHECK constraint, generated column, or non-literal
    /// DEFAULT in a build without the v0.4 expression evaluator
    /// (the tiny profile, v0.16): hipp cannot enforce or
    /// compute it, so it refuses the table instead of silently
    /// ignoring the clause. Build with feature `v04` (or
    /// `full`), or drop the clause.
    RequiresEvaluator,
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
    /// A statement needs the wall clock (`DEFAULT
    /// CURRENT_TIMESTAMP` and friends, v0.16) but the connection
    /// has none: the core has no clock of its own. Supply one
    /// with `Config::clock`.
    NoClock,
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
            Error::NoClock => Error::NoClock,
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
