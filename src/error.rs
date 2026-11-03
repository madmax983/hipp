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
    /// `SELECT *` with no FROM clause (v0.16 review fix; the pin's
    /// "no tables specified"). Name a table, or list the columns.
    NoTablesSpecified,
    /// A result set of more than 2000 columns once `*` is expanded
    /// (v0.16; the pin's "too many columns in result set"), or an
    /// ALTER TABLE ADD COLUMN past 2000 ("too many columns on t").
    /// Name fewer columns.
    TooManyColumns,
    /// A FROM clause joining more than 64 tables (v0.16; the pin's
    /// "at most 64 tables in a join"). Split the query.
    TooManyTables,
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
    /// `abs()` of the INTEGER -9223372036854775808, which has no
    /// INTEGER result (v0.16; the pin's "integer overflow" — hipp
    /// used to answer a REAL). Cast the argument to REAL first.
    IntegerOverflow,
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
    /// A bound parameter inside a schema object's definition — a
    /// view, a CHECK constraint, a generated column or a DEFAULT
    /// (v0.16, `params`; the pin's "parameters are not allowed in
    /// views" / "parameters prohibited in CHECK constraints" /
    /// "default value ... is not constant"). The definition is
    /// stored as text and re-read without the bindings: write the
    /// value as a literal.
    #[cfg(feature = "params")]
    ParameterNotAllowed,
    /// A row value `(a, b, ...)` (or a multi-column scalar
    /// subquery) where only a single value is allowed, or two
    /// row values of different sizes compared (v0.16, `v16`;
    /// the pin's "row value misused" and "IN(...) element has N
    /// terms - expected M"). Compare row values of equal size
    /// with `=`, `<>`, `<`, `<=`, `>`, `>=`, `IS [NOT]`,
    /// `BETWEEN`, `IN`, or assign them with `UPDATE ... SET
    /// (a, b) = ...`.
    #[cfg(feature = "v16")]
    RowValueMisused,
    /// A function name the pin does not define, or one this build
    /// leaves out (v0.16; the pin's "no such function"). Raised
    /// at prepare.
    UnknownFunction,
    /// A known function called with an argument count it does not
    /// take (v0.16; the pin's "wrong number of arguments to
    /// function"). Raised at prepare.
    WrongArgumentCount,
    /// `likelihood(X, P)` whose P is not a REAL literal between
    /// 0.0 and 1.0 (v0.16, `v16`; the pin's "second argument to
    /// likelihood() must be a constant between 0.0 and 1.0").
    /// Write P as a literal such as `0.5`.
    #[cfg(feature = "v16")]
    LikelihoodNotConstant,
    /// A non-deterministic function — `random()`, `randomblob()`,
    /// `changes()`, `total_changes()`, `last_insert_rowid()`,
    /// `sqlite_version()` — in a generated column (v0.16, `v16`;
    /// the pin's "non-deterministic functions prohibited in
    /// generated columns"). Compute the value on INSERT instead.
    #[cfg(feature = "v16")]
    NonDeterministic,
    /// A string or blob result of 1,000,000,000 bytes or more
    /// (v0.16, `v16`; the pin's default `SQLITE_LIMIT_LENGTH`,
    /// "string or blob too big"): `zeroblob` / `randomblob` past
    /// the limit, or a `printf` result reaching it.
    #[cfg(feature = "v16")]
    TooBig,
    /// A DISTINCT aggregate called with other than exactly one
    /// argument (v0.16, ungated; the pin's "DISTINCT aggregates
    /// must have exactly one argument" — `group_concat(DISTINCT
    /// x, ',')` used to run). Drop DISTINCT, or the separator.
    DistinctAggregateArgs,
    /// `ORDER BY` inside the parentheses of a function that is
    /// not an aggregate (v0.16, `v16`; the pin's "ORDER BY may
    /// not be used with non-aggregate"). Order the query instead.
    #[cfg(feature = "v16")]
    OrderByNonAggregate,
    /// `FILTER (WHERE ...)` after a function that is not an
    /// aggregate (v0.16, `v16`; the pin's "FILTER may not be used
    /// with non-aggregate"). Use a CASE expression instead.
    #[cfg(feature = "v16")]
    FilterNonAggregate,
    /// The P argument of `percentile(Y, P)` (0 ..= 100) or
    /// `percentile_cont` / `percentile_disc` (0 ..= 1) is not a
    /// number in range — NULL, non-numeric text, a blob, or out
    /// of bounds (v0.16, `v16`; the pin's "the fraction argument
    /// to ...() is not between 0.0 and ..."). Raised for each row
    /// the aggregate steps, NULL Y included. Pass a constant in
    /// range.
    #[cfg(feature = "v16")]
    PercentileFraction,
    /// The P argument of a percentile aggregate differs (by more
    /// than 0.001 of the fraction) between rows of one group
    /// (v0.16, `v16`; the pin's "the fraction argument to ...() is
    /// not the same for all input rows"). Pass a constant P.
    #[cfg(feature = "v16")]
    PercentileFractionVaries,
    /// A non-NULL Y of `median()` / `percentile*()` that is not an
    /// INTEGER or REAL — TEXT (even numeric text) or a BLOB
    /// (v0.16, `v16`; the pin's "input to ...() is not numeric").
    /// CAST the column to REAL.
    #[cfg(feature = "v16")]
    PercentileNotNumeric,
    /// An infinite Y of `median()` / `percentile*()` (v0.16,
    /// `v16`; the pin's "Inf input to ...()"). Filter infinities
    /// out first.
    #[cfg(feature = "v16")]
    PercentileInfinite,
    /// A FOREIGN KEY whose column lists disagree (v0.16 fix, the
    /// pin's "foreign key on x should reference only one column of
    /// table p" / "number of columns in foreign key does not match
    /// the number of columns in the referenced table"): a column's
    /// REFERENCES names at most one parent column, a table-level
    /// key as many as it lists. Make the lists the same length.
    ForeignKeyColumns,
    /// CREATE TABLE / INDEX / VIEW of a name starting with
    /// `sqlite_` (any case; the pin's "object name reserved for
    /// internal use"). Choose another name.
    ReservedName,
    /// A schema qualifier that names no database (v0.16, `v16`;
    /// the pin's "unknown database"). hipp has `main` only.
    #[cfg(feature = "v16")]
    UnknownDatabase,
    /// `PRAGMA encoding` named no encoding the pin knows (v0.16,
    /// `v16`; "unsupported encoding"). Use `UTF-8`.
    #[cfg(feature = "v16")]
    UnknownEncoding,
    /// INSERT / UPDATE / DELETE / DROP / ALTER / CREATE INDEX
    /// against `sqlite_schema` (v0.16, `v16`; the pin's "table
    /// sqlite_master may not be modified"). Change the schema
    /// with CREATE / DROP statements instead.
    #[cfg(feature = "v16")]
    SchemaTableReadOnly,
    /// A view whose definition reads itself, directly or through
    /// other views (v0.16, `v16`; the pin's "view v is circularly
    /// defined", raised when the view is used). Redefine one of
    /// the views without the cycle.
    #[cfg(feature = "v16")]
    CircularView,
    /// A common table expression that reads itself where it may
    /// not (v0.16, `v16`; the pin's "circular reference"): a CTE
    /// that is not a UNION / UNION ALL compound whose last SELECTs
    /// name it in their FROM clause, a reference from its setup
    /// SELECTs, or a cycle between CTEs. Make the CTE recursive
    /// in that shape, or break the cycle.
    #[cfg(feature = "v16")]
    CircularReference,
    /// A recursive SELECT that reads its recursive table more than
    /// once — twice in its FROM clause, or again inside a
    /// subquery (v0.16, `v16`; the pin's "multiple references to
    /// recursive table" / "multiple recursive references"). Read
    /// the recursive table once per recursive SELECT.
    #[cfg(feature = "v16")]
    RecursiveReferences,
    /// A recursive SELECT with an aggregate or GROUP BY (v0.16,
    /// `v16`; the pin's "recursive aggregate queries not
    /// supported"). Aggregate over the CTE's result instead.
    #[cfg(feature = "v16")]
    RecursiveAggregate,
    /// One WITH clause names two CTEs alike (v0.16, `v16`; the
    /// pin's "duplicate WITH table name"). Rename one.
    #[cfg(feature = "v16")]
    DuplicateCteName,
    /// Views and common table expressions nested deeper than
    /// `Config::max_nesting` allows, or a statement whose CTE
    /// references copy 1000 SELECTs or more (v0.16, `v16`; the
    /// pin's "VIEWs and/or subqueries nested too deep"). Define
    /// the views or CTEs less deeply, or raise the limit (with
    /// stack to match).
    #[cfg(feature = "v16")]
    NestedTooDeep,
    /// `RELEASE` / `ROLLBACK TO` a name no open savepoint has
    /// (v0.16, `v16`; the pin's "no such savepoint"). Open it with
    /// `SAVEPOINT name` first; savepoints end with the transaction.
    #[cfg(feature = "v16")]
    NoSuchSavepoint,
    /// `VACUUM` inside a transaction (v0.16, `v16`; the pin's
    /// "cannot VACUUM from within a transaction"). COMMIT or
    /// ROLLBACK first.
    #[cfg(feature = "v16")]
    VacuumInTransaction,
    /// `VACUUM INTO` a file that already holds a database, or that
    /// the connection's `Config::vacuum_into` hook could not write
    /// (v0.16, `v16`; the pin's "output file already exists"), or a
    /// name expression whose value is not text (the pin's
    /// "non-text filename"). Name a new or empty file, as text.
    #[cfg(feature = "v16")]
    VacuumTarget,
    /// `REINDEX x` where `x` names no collation, table or index
    /// (v0.16, `v16`; the pin's "unable to identify the object to
    /// be reindexed"). Name an existing one.
    #[cfg(feature = "v16")]
    ReindexTarget,
    /// A schema change the pin refuses on one of its own tables
    /// (v0.16): `sqlite_*` tables may not be indexed or altered,
    /// and only `sqlite_stat*` may be dropped ("table sqlite_sequence
    /// may not be dropped"). Leave the system tables to the engine.
    #[cfg(any(feature = "indexes", feature = "v04"))]
    SystemTable,
    /// `AUTOINCREMENT` on a column that is not the table's INTEGER
    /// PRIMARY KEY (v0.16, `v16`; "AUTOINCREMENT is only allowed on
    /// an INTEGER PRIMARY KEY"). Declare the column `INTEGER
    /// PRIMARY KEY` (not DESC), or drop the keyword.
    #[cfg(feature = "v16")]
    AutoincrementNotIntegerPk,
    /// `ALTER TABLE ... RENAME COLUMN` to `rowid`, `oid` or
    /// `_rowid_` (v0.16, `v16`; the pin's "cannot rename to rowid
    /// alias"). Pick another name.
    #[cfg(feature = "v16")]
    RowidAliasName,
    /// `ALTER TABLE ... DROP COLUMN` of a PRIMARY KEY or UNIQUE
    /// column, or of a table's only column (v0.16, `v16`; the
    /// pin's "cannot drop PRIMARY KEY column", "cannot drop UNIQUE
    /// column", "no other columns exist"). Rebuild the table
    /// instead (CREATE TABLE ... AS / INSERT ... SELECT).
    #[cfg(feature = "v16")]
    CannotDropColumn,
    /// An AUTOINCREMENT table has handed out rowid
    /// 9223372036854775807, or its largest rowid is that value
    /// (v0.16, `v16`; the pin's "database or disk is full"): no
    /// larger rowid exists and AUTOINCREMENT never reuses one.
    /// Insert with an explicit smaller rowid, or reset the table's
    /// `sqlite_sequence` row. Also a table holding every positive
    /// rowid, with no rowid left for a new row.
    #[cfg(feature = "v16")]
    DatabaseFull,
}

/// Sub-kind for `Error::Parse` — no strings, just the class of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseKind {
    UnexpectedToken,
    UnexpectedEof,
    InvalidLiteral,
    UnsupportedStatement,
    UnsupportedClause,
    /// The statement nests deeper than the parser's stack limit
    /// (the pin's "Recursion limit": 2500 grammar symbols, about
    /// 2490 nested parentheses; `Config::max_parser_depth`), or a
    /// CREATE TABLE item nests parentheses past it. Flatten the
    /// nesting, or raise the limit with the thread's stack.
    RecursionLimit,
    TrailingInput,
    /// An expression tree deeper than the expression depth limit
    /// (v0.16; the pin's "Expression tree is too large (maximum
    /// depth 1000)", `SQLITE_LIMIT_EXPR_DEPTH`;
    /// `Config::max_expr_depth`), counted as the pin counts it —
    /// including the sum of the heights of the expressions a
    /// nested subquery sits in. Split the expression, or raise the
    /// limit with the thread's stack.
    ExprTooDeep,
    /// A statement longer than 1,000,000,000 bytes (v0.16; the
    /// pin's `SQLITE_LIMIT_SQL_LENGTH`, "statement too long").
    StatementTooLong,
    /// More columns than the pin's `SQLITE_MAX_COLUMN` (2000) in a
    /// CREATE TABLE ("too many columns on t"), an index ("too many
    /// columns in index"), a result list or VALUES row ("too many
    /// columns in result set") or an UPDATE SET list ("too many
    /// columns in set list") (v0.16). Split the table or the
    /// statement.
    TooManyColumns,
    /// An ORDER BY or GROUP BY of more than 2000 terms (v0.16; the
    /// pin's "too many terms in ORDER BY clause").
    TooManyTerms,
    /// A function call with more than 1000 arguments (v0.16; the
    /// pin's "too many arguments on function f").
    TooManyArguments,
    /// A compound SELECT of more than 500 SELECTs (v0.16; the pin's
    /// "too many terms in compound SELECT").
    TooManyCompoundTerms,
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
    /// `?0`, or `?NNN` past 32766 (v0.16, `params`; the pin's
    /// "variable number must be between ?1 and ?32766"). Number
    /// parameters from ?1 up to ?32766.
    #[cfg(feature = "params")]
    VariableNumber,
    /// A statement whose parameters need an index past 32766
    /// (v0.16, `params`; the pin's "too many SQL variables").
    /// Split the statement, or reuse named / numbered
    /// parameters.
    #[cfg(feature = "params")]
    TooManyVariables,
    /// A hexadecimal literal with more than 16 significant
    /// digits, or `-` applied to `0x8000000000000000` (v0.16;
    /// the pin's "hex literal too big"). Write a shorter
    /// literal, or a decimal one.
    HexLiteralTooBig,
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
    /// A `bind_*` call names an index outside 1 ..=
    /// `parameter_count()` (v0.16, `params`; the pin's
    /// `SQLITE_RANGE`). Bind by an index the statement has, or
    /// look one up with `parameter_index`.
    #[cfg(feature = "params")]
    BindIndexOutOfRange { index: usize },
    /// A `bind_*` call on a statement that has been stepped and
    /// not reset (v0.16, `params`; the pin's `SQLITE_MISUSE`).
    /// Call `Statement::reset` first. (`clear_bindings` is allowed
    /// at any time and never returns it.)
    #[cfg(feature = "params")]
    ResetRequired,
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
            #[cfg(feature = "params")]
            Error::BindIndexOutOfRange { index } => Error::BindIndexOutOfRange { index },
            #[cfg(feature = "params")]
            Error::ResetRequired => Error::ResetRequired,
            Error::Storage(e) => Error::Storage(e.into()),
        }
    }
}

impl<E> Error<E> {
    /// Re-wrap this error for another storage error type, mapping
    /// a `Storage` error through `f` (v0.16, `v16`: VACUUM builds
    /// its image on an in-memory pager, whose errors surface under
    /// the connection's storage type).
    #[cfg(feature = "v16")]
    pub fn map_storage<F>(self, f: impl FnOnce(E) -> Error<F>) -> Error<F> {
        match self {
            Error::Storage(e) => f(e),
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
            #[cfg(feature = "params")]
            Error::BindIndexOutOfRange { index } => Error::BindIndexOutOfRange { index },
            #[cfg(feature = "params")]
            Error::ResetRequired => Error::ResetRequired,
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
