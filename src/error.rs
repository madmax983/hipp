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
    /// `DROP INDEX` named an index that does not exist (v0.3);
    /// v0.16 (`v16`): an `INDEXED BY` naming no index of its table
    /// (or naming one on a view or CTE) — the pin's "no such
    /// index". Name one of the table's indexes, or drop the clause.
    UnknownIndex,
    /// `INDEXED BY` names a partial index the statement's WHERE
    /// does not imply (v0.16, `expr_index`; the pin's "no query
    /// solution"). Add the index's WHERE condition to the query, or
    /// name another index.
    #[cfg(feature = "expr_index")]
    NoQuerySolution,
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
    /// generated columns"; also in an index expression or a partial
    /// index's WHERE, v0.16 `expr_index`). Compute the value on
    /// INSERT instead.
    ///
    /// v0.16 (`datetime`): also a date and time function that would
    /// read the clock (`'now'`, no time value) or the time zone
    /// (`'localtime'`, `'utc'`) inside a CHECK constraint, an index
    /// expression or a generated column (the pin's
    /// "non-deterministic use of date() in a CHECK constraint").
    /// Pass the instant as a value instead.
    #[cfg(feature = "v16")]
    NonDeterministic,
    /// `'localtime'` / `'utc'` asked the time-zone hook
    /// (`Config::tz_offset`) for an instant it could not convert
    /// (v0.16, `datetime`; the pin's "local time unavailable").
    /// Supply a hook that knows the zone, or none for UTC.
    #[cfg(feature = "datetime")]
    LocalTimeUnavailable,
    /// A subquery inside an index expression or a partial index's
    /// WHERE (v0.16, `expr_index`; the pin's "subqueries prohibited
    /// in index expressions / partial index WHERE clauses"). Index
    /// a column of this table, or an expression over its columns.
    #[cfg(feature = "expr_index")]
    SubqueryInIndex,
    /// A qualified column (`t.a`) inside an index expression (v0.16,
    /// `expr_index`; the pin's "the \".\" operator prohibited in
    /// index expressions"). Name the column bare.
    #[cfg(feature = "expr_index")]
    QualifiedIndexColumn,
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
    /// A window function where the pin forbids one (v0.16,
    /// `window`; its "misuse of window function f()" / "misuse
    /// of aliased window function x"): outside a result column
    /// or an ORDER BY term of a SELECT (in WHERE, GROUP BY,
    /// HAVING, ON, a write statement, a CHECK or an index), or
    /// inside the arguments, PARTITION BY, ORDER BY or FILTER of
    /// an aggregate or another window function; and a built-in
    /// window function (`row_number()`, ...) called without
    /// OVER. Compute it in a FROM subquery and filter outside.
    #[cfg(feature = "window")]
    MisusedWindow,
    /// OVER on a function that is not a window function (v0.16,
    /// `window`; the pin's "f() may not be used as a window
    /// function"): a scalar function, or min / max with several
    /// arguments. Use an aggregate or a built-in window function.
    #[cfg(feature = "window")]
    NotWindowFunction,
    /// DISTINCT in a window function call (v0.16, `window`; the
    /// pin's "DISTINCT is not supported for window functions").
    /// Deduplicate in a subquery first.
    #[cfg(feature = "window")]
    WindowDistinct,
    /// FILTER on a built-in window function, or an aggregate
    /// ORDER BY inside a window aggregate's parentheses (v0.16,
    /// `window`; the pin's "FILTER clause may only be used with
    /// aggregate window functions" / "ORDER BY may not be used
    /// with non-aggregate f()"). Order through the window's own
    /// ORDER BY.
    #[cfg(feature = "window")]
    WindowClauseMisused,
    /// `OVER name` / `OVER (name ...)` / `AS (name ...)` naming no
    /// window of the select's WINDOW clause (v0.16, `window`; the
    /// pin's "no such window"). Define it in the WINDOW clause.
    #[cfg(feature = "window")]
    NoSuchWindow,
    /// A window that inherits from a named window but restates
    /// its PARTITION BY, a second ORDER BY, or inherits from one
    /// with an explicit frame (v0.16, `window`; the pin's "cannot
    /// override ... of window"). Inherit only what the base
    /// leaves out.
    #[cfg(feature = "window")]
    WindowOverride,
    /// A RANGE frame with an `<expr> PRECEDING / FOLLOWING` bound
    /// over other than exactly one ORDER BY term (v0.16, `window`;
    /// the pin's "RANGE with offset PRECEDING/FOLLOWING requires
    /// one ORDER BY expression"). Order by one expression, or use
    /// ROWS / GROUPS.
    #[cfg(feature = "window")]
    RangeOffsetOrder,
    /// A frame's starting offset that is not a non-negative
    /// integer (ROWS / GROUPS) or number (RANGE) — NULL, text, a
    /// blob, a fraction, a negative value or a non-constant
    /// expression (v0.16, `window`; the pin's "frame starting
    /// offset must be a non-negative integer / number"). Write a
    /// non-negative literal.
    #[cfg(feature = "window")]
    FrameStartOffset,
    /// The same for a frame's ending offset (v0.16, `window`).
    #[cfg(feature = "window")]
    FrameEndOffset,
    /// `nth_value(x, N)` with an N that is not a positive integer
    /// (v0.16, `window`; the pin's "second argument to nth_value
    /// must be a positive integer"). Pass N >= 1.
    #[cfg(feature = "window")]
    NthValueArgument,
    /// `ntile(N)` with an N that is not a positive integer for a
    /// partition's first row (v0.16, `window`; the pin's
    /// "argument of ntile must be a positive integer"). Pass N >=
    /// 1.
    #[cfg(feature = "window")]
    NtileArgument,
    /// A window call in the result columns or ORDER BY of a
    /// recursive CTE's last recursive SELECT (v0.16, `window`; the
    /// pin's "cannot use window functions in recursive queries").
    /// Apply the window over the CTE's result instead.
    #[cfg(feature = "window")]
    RecursiveWindow,
    /// A FOREIGN KEY whose column lists disagree (v0.16 fix, the
    /// pin's "foreign key on x should reference only one column of
    /// table p" / "number of columns in foreign key does not match
    /// the number of columns in the referenced table"): a column's
    /// REFERENCES names at most one parent column, a table-level
    /// key as many as it lists. Make the lists the same length.
    ForeignKeyColumns,
    /// A write left a foreign key unsatisfied while `PRAGMA
    /// foreign_keys` is ON (v0.16, `foreign_keys`; the pin's
    /// "FOREIGN KEY constraint failed"): a child row names a parent
    /// key no parent row has, or a parent row still referred to went
    /// — at the statement's end for an immediate key, at COMMIT (the
    /// transaction staying open) for a deferred one; or a RESTRICT
    /// action refused the change. Insert the parent first, delete
    /// the children first, or defer the key.
    #[cfg(feature = "foreign_keys")]
    ForeignKeyViolation,
    /// A foreign key whose parent columns are not the parent's
    /// PRIMARY KEY or a UNIQUE index with their collations (v0.16,
    /// `foreign_keys`; the pin's "foreign key mismatch - "c"
    /// referencing "p""), found when a write needs the key. Give
    /// the parent such a key, or reference the one it has.
    #[cfg(feature = "foreign_keys")]
    ForeignKeyMismatch,
    /// `ALTER TABLE ADD COLUMN` of a REFERENCES column with a
    /// non-NULL default to a table with rows while `PRAGMA
    /// foreign_keys` is ON (v0.16 fix; the pin's "Cannot add a
    /// REFERENCES column with non-NULL default value"). Add it with
    /// no default, before the table has rows, or with the pragma off.
    #[cfg(feature = "v16")]
    ReferencesDefault,
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
    /// `PRAGMA synchronous = ...` inside an explicit transaction
    /// (v0.16, `v16`; the pin's "Safety level may not be changed
    /// inside a transaction", raised when the statement is
    /// prepared). COMMIT or ROLLBACK first.
    #[cfg(feature = "v16")]
    SafetyLevelInTransaction,
    /// `PRAGMA journal_mode = WAL` (or from WAL to another mode)
    /// inside an explicit transaction that has not written yet
    /// (v0.16, `wal`; the pin's "cannot change into / out of wal
    /// mode from within a transaction"). COMMIT or ROLLBACK first.
    /// (Once the transaction has written, the pin — and hipp —
    /// leave the mode alone and report the current one.)
    #[cfg(feature = "wal")]
    WalModeInTransaction,
    /// `PRAGMA wal_checkpoint` inside a transaction that has read or
    /// written the database (v0.16, `wal`; the pin's "database table
    /// is locked"). COMMIT or ROLLBACK first.
    #[cfg(feature = "wal")]
    CheckpointLocked,
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
    /// `AUTOINCREMENT` on a WITHOUT ROWID table (v0.16,
    /// `without_rowid`; the pin's "AUTOINCREMENT not allowed on
    /// WITHOUT ROWID tables"): such a table has no rowid to count.
    /// Drop the keyword, or the WITHOUT ROWID option.
    #[cfg(feature = "without_rowid")]
    AutoincrementWithoutRowid,
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
    /// A write to a table (or view) that has triggers, in a build
    /// without the `triggers` feature (v0.16, every profile): the
    /// triggers would not fire, so the write is refused rather
    /// than silently skipping them. Build with feature `triggers`,
    /// or drop the triggers with an engine that has them. Also a
    /// trigger whose stored text hipp cannot parse (with
    /// `triggers`).
    TableHasTriggers,
    /// `CREATE TRIGGER` of a name the schema already has (v0.16,
    /// `triggers`; the pin's "trigger t already exists"). Pick
    /// another name, or write `IF NOT EXISTS`.
    #[cfg(feature = "triggers")]
    DuplicateTrigger,
    /// `DROP TRIGGER` of a name no trigger has (v0.16, `triggers`;
    /// "no such trigger"). Write `IF EXISTS` to accept that.
    #[cfg(feature = "triggers")]
    UnknownTrigger,
    /// A BEFORE / AFTER trigger on a view (v0.16, `triggers`;
    /// "cannot create BEFORE trigger on view"). Use INSTEAD OF.
    #[cfg(feature = "triggers")]
    TriggerOnView,
    /// An INSTEAD OF trigger on a table (v0.16, `triggers`;
    /// "cannot create INSTEAD OF trigger on table"). Use BEFORE or
    /// AFTER.
    #[cfg(feature = "triggers")]
    InsteadOfTable,
    /// A schema-qualified table name on an INSERT / UPDATE /
    /// DELETE inside a trigger body (v0.16, `triggers`; "qualified
    /// table names are not allowed on INSERT, UPDATE, and DELETE
    /// statements within triggers"). Write the bare name.
    #[cfg(feature = "triggers")]
    TriggerQualifiedTarget,
    /// `INDEXED BY` / `NOT INDEXED` on an UPDATE or DELETE inside a
    /// trigger body (v0.16 review fix, `triggers`; the pin's "the
    /// INDEXED BY clause is not allowed on UPDATE or DELETE
    /// statements within triggers"). Drop the clause.
    #[cfg(feature = "triggers")]
    TriggerIndexedTarget,
    /// A `table.*` item in a RETURNING list (v0.16 review fix,
    /// ungated; the pin's "RETURNING may not use \"TABLE.*\"
    /// wildcards"). Write `*` or name the columns.
    #[cfg(feature = "v07")]
    ReturningTableStar,
    /// RETURNING inside a trigger body (v0.16, `triggers`;
    /// "cannot use RETURNING in a trigger"). Drop the clause.
    #[cfg(feature = "triggers")]
    TriggerReturning,
    /// A trigger of the main schema that names a TEMP object, or a
    /// TEMP trigger with a qualified name (v0.16, `triggers`; the
    /// pin's "trigger ... cannot reference objects in database
    /// temp" / "temporary trigger may not have qualified name").
    /// Create a TEMP trigger, or drop the qualifier.
    #[cfg(feature = "triggers")]
    TriggerTempReference,
    /// `RAISE()` outside a trigger program (v0.16, `triggers`;
    /// "RAISE() may only be used within a trigger-program").
    #[cfg(feature = "triggers")]
    RaiseOutsideTrigger,
    /// A trigger program ran `RAISE(ROLLBACK, msg)` (v0.16,
    /// `triggers`): the statement failed and the transaction was
    /// rolled back. The message is `Connection::raise_message`.
    #[cfg(feature = "triggers")]
    RaiseRollback,
    /// A trigger program ran `RAISE(ABORT, msg)` (v0.16,
    /// `triggers`): the statement failed and was rolled back.
    #[cfg(feature = "triggers")]
    RaiseAbort,
    /// A trigger program ran `RAISE(FAIL, msg)` (v0.16,
    /// `triggers`): the statement failed, keeping its changes so
    /// far.
    #[cfg(feature = "triggers")]
    RaiseFail,
    /// `RAISE(IGNORE)` reached the statement that fired the
    /// trigger (v0.16, `triggers`). Never returned to a caller: the
    /// statement skips the row and goes on.
    #[cfg(feature = "triggers")]
    RaiseIgnore,
    /// Trigger programs nested deeper than
    /// `Config::max_trigger_depth` (v0.16, `triggers`; the pin's
    /// "triggers nested too deep" past 1000 levels).
    /// Bound the recursion with a WHEN clause, or raise the limit
    /// on a thread with the stack for it. (Foreign-key action
    /// programs count too, `foreign_keys`.)
    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
    TriggerDepth,
    /// An upsert into a view (v0.16, `triggers`; "cannot UPSERT a
    /// view"). Drop the ON CONFLICT clause.
    #[cfg(feature = "triggers")]
    UpsertView,
    /// A JSON argument that is neither JSON text (RFC 8259 or
    /// JSON5) nor JSONB, or JSONB whose contents do not decode
    /// (v0.16, `json`; the pin's "malformed JSON"). Pass valid
    /// JSON, or check it first with `json_valid()`.
    #[cfg(feature = "json")]
    JsonMalformed,
    /// A JSON path that does not start with `$` or does not parse
    /// (v0.16, `json`; the pin's "bad JSON path: ..."). Write
    /// `$`, then `.key`, `."quoted key"`, `[N]` or `[#-N]` steps.
    #[cfg(feature = "json")]
    JsonBadPath,
    /// A JSON path walked more than 1000 steps deep (v0.16,
    /// `json`; the pin's "JSON path too deep"). Use a shorter path.
    #[cfg(feature = "json")]
    JsonPathTooDeep,
    /// JSON rendered or merged more than 1000 levels deep (v0.16,
    /// `json`; the pin's "JSON nested too deep"). Nest less deeply.
    #[cfg(feature = "json")]
    JsonNestedTooDeep,
    /// A BLOB that is not JSONB given as a JSON value (v0.16,
    /// `json`; the pin's "JSON cannot hold BLOB values"). Encode it
    /// first, e.g. with `hex()`.
    #[cfg(feature = "json")]
    JsonBlobValue,
    /// `json_object()` / `jsonb_object()` with an odd number of
    /// arguments (v0.16, `json`; the pin's "json_object() requires
    /// an even number of arguments"). Pass label / value pairs.
    #[cfg(feature = "json")]
    JsonObjectOddArgs,
    /// A `json_object()` label that is not TEXT (v0.16, `json`;
    /// the pin's "json_object() labels must be TEXT"). Cast the
    /// label to TEXT.
    #[cfg(feature = "json")]
    JsonObjectLabel,
    /// `json_insert()` / `json_replace()` / `json_set()` /
    /// `json_array_insert()` (or a `jsonb_` form) with an even
    /// number of arguments (v0.16, `json`; the pin's "json_set()
    /// needs an odd number of arguments"). Pass the JSON, then
    /// path / value pairs.
    #[cfg(feature = "json")]
    JsonOddArgs,
    /// `json_valid(X, FLAGS)` with FLAGS outside 1..=15 (v0.16,
    /// `json`; the pin's "FLAGS parameter to json_valid() must be
    /// between 1 and 15").
    #[cfg(feature = "json")]
    JsonValidFlags,
    /// `json_array_insert()` at a path that names an object member
    /// or the root (v0.16, `json`; the pin's "not an array
    /// element"). End the path with an array index.
    #[cfg(feature = "json")]
    JsonNotArrayElement,
    /// `ATTACH ... AS name` where `name` is `main`, `temp` or an
    /// attached database's (any case; v0.16, `attach`; the pin's
    /// "database X is already in use"). Choose another name, or
    /// DETACH that database first.
    #[cfg(feature = "attach")]
    DatabaseInUse,
    /// An eleventh attached database (v0.16, `attach`; the pin's
    /// "too many attached databases - max 10"). DETACH one first.
    #[cfg(feature = "attach")]
    TooManyAttached,
    /// The storage for an ATTACH could not be opened (v0.16,
    /// `attach`; the pin's "unable to open database"): no such
    /// directory, no permission, or storage that reaches no files
    /// (`MemStorage` without `Connection::set_attach_hook`). Fix the
    /// name, or supply a hook.
    #[cfg(feature = "attach")]
    CannotOpenDatabase,
    /// `DETACH main` / `DETACH temp` (v0.16, `attach`; the pin's
    /// "cannot detach database X"). Only attached databases detach.
    #[cfg(feature = "attach")]
    CannotDetach,
    /// `DETACH` of a name no database has (v0.16, `attach`; the
    /// pin's "no such database: X"). Check `PRAGMA database_list`.
    #[cfg(feature = "attach")]
    NoSuchDatabase,
    /// `DETACH` of a database the open transaction has read or
    /// written (v0.16, `attach`; the pin's "database X is locked").
    /// COMMIT or ROLLBACK first.
    #[cfg(feature = "attach")]
    DatabaseLocked,
    /// ATTACH of a database whose text encoding differs from the
    /// main database's (v0.16, `attach`; the pin's "attached
    /// databases must use the same text encoding as main
    /// database"). Convert one of them (VACUUM INTO a database made
    /// with the other's `PRAGMA encoding`).
    #[cfg(feature = "attach")]
    AttachEncoding,
    /// A view or trigger of one database naming an object of
    /// another with a schema qualifier (v0.16, `attach`; the pin's
    /// "view v cannot reference objects in database X", "trigger t
    /// cannot reference objects in database X"). Make it a TEMP view
    /// / trigger, or keep it in one database.
    #[cfg(feature = "attach")]
    CrossDatabaseReference,
    /// `CREATE VIRTUAL TABLE ... USING m` or a statement reading or
    /// writing a virtual table whose module hipp does not have
    /// (v0.16, `vtab`; the pin's "no such module: m"). The rest of
    /// the file works; use an engine with the module for that table.
    #[cfg(feature = "vtab")]
    NoSuchModule,
    /// A shadow table of a virtual table (`t_node`, ...) as the
    /// target of INSERT / UPDATE / DELETE / ALTER / CREATE TRIGGER
    /// (v0.16, `vtab`; the pin shell's defensive "table t_node may
    /// not be modified"), or of CREATE INDEX (which the pin allows,
    /// but hipp's module writes would not maintain the index).
    /// Write the virtual table instead.
    #[cfg(feature = "vtab")]
    ShadowReadOnly,
    /// DROP TABLE of a shadow table (v0.16, `vtab`; "table t_node
    /// may not be dropped"). DROP the virtual table instead.
    #[cfg(feature = "vtab")]
    ShadowNoDrop,
    /// A write to a virtual table whose module has no xUpdate
    /// (v0.16, `vtab`: `generate_series`, `dbstat`; the pin's "table
    /// X may not be modified").
    #[cfg(feature = "vtab")]
    VtabReadOnly,
    /// CREATE INDEX or CREATE TRIGGER on a virtual table (v0.16,
    /// `vtab`; "virtual tables may not be indexed", "cannot create
    /// triggers on virtual tables").
    #[cfg(feature = "vtab")]
    VtabNotIndexable,
    /// ALTER TABLE ADD / DROP / RENAME COLUMN of a virtual table
    /// (v0.16, `vtab`; "virtual tables may not be altered"), or
    /// RENAME TO of one hipp cannot rename (not an R*Tree, or while
    /// the schema has views or triggers whose text would change).
    #[cfg(feature = "vtab")]
    VtabNotAlterable,
    /// The virtual table's own storage is inconsistent (v0.16,
    /// `vtab`; the pin's SQLITE_CORRUPT_VTAB, "database disk image
    /// is malformed"): an R*Tree node missing, of the wrong size, or
    /// not linked as its `%_parent` / `%_rowid` rows say. Rebuild the
    /// table; `rtreecheck()` names what is wrong.
    #[cfg(feature = "vtab")]
    VtabCorrupt,
    /// `generate_series()` with no usable START (v0.16, `vtab`; the
    /// pin's "first argument to "generate_series()" missing or
    /// unusable"). Pass a start, or constrain `value`.
    #[cfg(feature = "vtab")]
    SeriesFirstArgument,
    /// `rtree(...)` with fewer than one dimension (v0.16, `rtree`;
    /// "Too few columns for an rtree table").
    #[cfg(feature = "rtree")]
    RtreeTooFewColumns,
    /// More than five dimensions or 100 auxiliary columns (v0.16,
    /// `rtree`; "Too many columns for an rtree table").
    #[cfg(feature = "rtree")]
    RtreeTooManyColumns,
    /// An odd number of coordinate columns (v0.16, `rtree`; "Wrong
    /// number of columns for an rtree table").
    #[cfg(feature = "rtree")]
    RtreeWrongColumns,
    /// A coordinate column after an auxiliary one (v0.16, `rtree`;
    /// "Auxiliary rtree columns must be last").
    #[cfg(feature = "rtree")]
    RtreeAuxNotLast,
    /// A row whose lower bound exceeds its upper in some dimension
    /// (v0.16, `rtree`; "rtree constraint failed: t.(x0<=x1)"),
    /// whatever the conflict clause (OR IGNORE skips the row).
    /// Order each pair low, high.
    #[cfg(feature = "rtree")]
    RtreeConstraint,
    /// `rtreedepth()` of something not a node blob (v0.16, `rtree`;
    /// "Invalid argument to rtreedepth()").
    #[cfg(feature = "rtree")]
    RtreeDepthArgument,
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
    /// A constraint-level `ON CONFLICT FAIL | IGNORE | REPLACE`
    /// in a build without the conflict-resolving write path
    /// (v0.16; below `v07`): hipp cannot resolve the violation as
    /// the pin would, so it refuses the table instead of ignoring
    /// the clause. Build with feature `v07` (or `full`), or drop
    /// the clause (ABORT and ROLLBACK need no feature).
    #[cfg(not(feature = "v07"))]
    RequiresConflictResolution,
    /// Two PRIMARY KEY / UNIQUE constraints on the same columns
    /// with different `ON CONFLICT` clauses (v0.16, `v07`; the
    /// pin's "conflicting ON CONFLICT clauses specified"). Give
    /// them one clause, or drop the repeat.
    #[cfg(feature = "v07")]
    ConflictingOnConflict,
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
    /// A window frame whose start comes after its end in the
    /// pin's bound order — `CURRENT ROW` to `<expr> PRECEDING`,
    /// or `<expr> FOLLOWING` to `PRECEDING` / `CURRENT ROW`
    /// (v0.16, `window`; the pin's "unsupported frame
    /// specification"). Put the earlier bound first.
    #[cfg(feature = "window")]
    UnsupportedFrame,
    /// `WITHOUT <name>` with a name other than ROWID (v0.16,
    /// `without_rowid`; the pin's "unknown table option"). The
    /// table options are STRICT and WITHOUT ROWID.
    #[cfg(feature = "without_rowid")]
    UnknownTableOption,
    /// A WITHOUT ROWID table with no PRIMARY KEY (v0.16,
    /// `without_rowid`; the pin's "PRIMARY KEY missing on table
    /// t"). Declare one: its columns key the table's B-tree.
    #[cfg(feature = "without_rowid")]
    PrimaryKeyMissing,
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
    /// Header read/write version 2 (WAL) — SPEC §3.1 — or, v0.16, a
    /// non-empty `-wal` log beside the database: a build without the
    /// `wal` leaf cannot read the committed pages a log may hold.
    /// Build with `wal`, or checkpoint the database with SQLite.
    WalUnsupported,
    /// Reserved-bytes-per-page != 0 on a write path.
    ReservedBytesUnsupported,
    /// A write to an auto-vacuum database (header offset 52
    /// non-zero; v0.16 review fix): hipp keeps no pointer-map pages,
    /// so it reads such a file but never writes it. Convert it
    /// first with `PRAGMA auto_vacuum = NONE; VACUUM` (hipp or
    /// SQLite), or write it with SQLite.
    AutoVacuumUnsupported,
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
    /// Another connection holds the database in WAL mode (v0.16,
    /// `wal`): a live SQLite connection, which shares its wal-index
    /// through the `-shm` hipp cannot take part in, or another hipp
    /// connection (this process's too), whose heap wal-index this one
    /// would not see. Close the other connection first
    /// (`Storage::claim_wal` reports it).
    #[cfg(feature = "wal")]
    DatabaseBusy,
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
            Error::AutoVacuumUnsupported => Error::AutoVacuumUnsupported,
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
            #[cfg(feature = "wal")]
            Error::DatabaseBusy => Error::DatabaseBusy,
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
            Error::AutoVacuumUnsupported => Error::AutoVacuumUnsupported,
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
            #[cfg(feature = "wal")]
            Error::DatabaseBusy => Error::DatabaseBusy,
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
