//! AST for the SQL grammar (SPEC §5.6; v0.2 adds multi-row
//! VALUES, `WHERE rowid =`, and transaction control; v0.3 adds
//! index statements and records UNIQUE/PK constraint sets).
//! Text/blob bytes live in the statement arena and are referenced
//! by `ABytes` handles, so the tree holds no self-referential
//! borrows.

use crate::arena::{ABytes, AVec};
#[cfg(any(feature = "v04", feature = "indexes"))]
use crate::record::Coll;
use alloc::boxed::Box;

/// A literal value as written in the SQL text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Literal {
    Null,
    Integer(i64),
    Real(f64),
    Text(ABytes),
    Blob(ABytes),
}

/// One column definition inside `CREATE TABLE`.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: ABytes,
    /// The declared type text exactly as written (may be empty),
    /// stored for affinity derivation and schema round-tripping.
    pub decl: ABytes,
    /// `PRIMARY KEY` appeared in this column's constraints (or the
    /// column was named by a table-level `PRIMARY KEY (col)`).
    pub primary_key: bool,
    /// The column's PRIMARY KEY was written with `DESC` (the
    /// SQLite quirk: such a key is NOT the rowid alias even when
    /// the declared type is exactly INTEGER).
    pub pk_desc: bool,
    /// `UNIQUE` appeared in this column's constraints (v0.3).
    pub unique: bool,
    /// A simple literal `DEFAULT`, when present and parseable as a
    /// literal (v0.1 materialises only literal defaults).
    pub default: Option<Literal>,
    /// `NOT NULL` appeared in this column's constraints (v0.4:
    /// recorded for ALTER TABLE ADD COLUMN validation; v0.10:
    /// carried into the catalog and enforced on every write
    /// path — see `session::resolve_not_null`).
    pub not_null: bool,
    /// A `DEFAULT` was present but is not a plain literal
    /// (e.g. `CURRENT_TIMESTAMP`, a function call) (v0.4).
    pub default_nonconst: bool,
    /// The column is generated — `GENERATED ALWAYS AS (expr)`
    /// or the short `AS (expr)` (v0.4: recorded so ADD COLUMN
    /// could refuse it; v0.16: computed, see `gen_expr`).
    pub generated: bool,
    /// The DEFAULT clause's expression text when it is not a
    /// plain literal (v0.16): `(1+2)`, `-'3'`, `-x'01'`,
    /// `CURRENT_TIMESTAMP`, ... — re-parsed and evaluated by
    /// the planner when a row needs the default. `None` for a
    /// literal default (then `default` holds it) or no DEFAULT.
    #[cfg(feature = "v04")]
    pub default_expr: Option<ABytes>,
    /// The generation expression's text (the inside of `AS
    /// (...)`) of a generated column (v0.16).
    #[cfg(feature = "v04")]
    pub gen_expr: Option<ABytes>,
    /// A generated column written `STORED` (v0.16): computed on
    /// write and kept in the record. Otherwise (`VIRTUAL`, the
    /// default) it is computed on read and absent from the
    /// record, exactly as in the pin's file format.
    #[cfg(feature = "v04")]
    pub stored: bool,
    /// The column's `COLLATE name` (v0.16): its collating
    /// sequence for comparisons, ORDER BY / GROUP BY / DISTINCT
    /// and the indexes on it; `None` (no clause) is BINARY. An
    /// unknown name is refused at parse, as on the pin.
    #[cfg(any(feature = "v04", feature = "indexes"))]
    pub coll: Option<Coll>,
}

/// One UNIQUE or PRIMARY KEY constraint of a `CREATE TABLE`, in
/// statement order (v0.3): the catalog derives one implicit index
/// (`sqlite_autoindex_<table>_<n>`) per set that is not the rowid
/// alias, in exactly this order (verified against the oracle).
#[derive(Debug, Clone)]
pub struct ConstraintSet {
    pub primary_key: bool,
    pub columns: AVec<IndexColumnDef>,
}

/// One indexed column in a `CREATE INDEX` column list (v0.3).
#[derive(Debug, Clone, Copy)]
pub struct IndexColumnDef {
    pub name: ABytes,
    /// `DESC` sort order for this key column.
    pub desc: bool,
    /// An explicit `COLLATE name` on this key column (v0.16);
    /// `None` takes the table column's collation.
    #[cfg(any(feature = "v04", feature = "indexes"))]
    pub coll: Option<Coll>,
}

#[derive(Debug, Clone)]
pub enum SelectList {
    All,
    Columns(AVec<ABytes>),
}

/// Binary operators (v0.4 expression grammar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `IS` / `IS NOT`: NULL-safe equality.
    Is,
    IsNot,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    /// `&`, `|`, `<<`, `>>` (v0.16, `v16`): both operands taken
    /// as 64-bit integers (the pin's `sqlite3VdbeIntValue`), NULL
    /// if either is NULL; a negative shift count shifts the other
    /// way and a count of 64 or more shifts every bit out.
    #[cfg(feature = "v16")]
    BitAnd,
    #[cfg(feature = "v16")]
    BitOr,
    #[cfg(feature = "v16")]
    Shl,
    #[cfg(feature = "v16")]
    Shr,
}

/// A scalar SQL expression (v0.4). Column references are by
/// (optional qualifier, name); resolution to FROM-source slots is
/// a plan-time step (see session/query).
#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Literal),
    Column {
        table: Option<ABytes>,
        name: ABytes,
    },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Unary minus.
    Neg(Box<Expr>),
    /// Unary plus over a non-literal operand (v0.16): the
    /// operand's value with *no* affinity (the pin's `TK_UPLUS`;
    /// `+a = '1'` compares without converting, and a
    /// CREATE TABLE ... AS SELECT column `+a` is named `"+a"`
    /// and typed by no affinity). Its collation is the operand's.
    /// A sign over a literal folds into the literal instead.
    Pos(Box<Expr>),
    /// Logical NOT.
    Not(Box<Expr>),
    IsNull(Box<Expr>, bool),
    InList(Box<Expr>, AVec<Expr>, bool),
    /// `x [NOT] IN (SELECT ...)` (v0.6).
    InSubquery(Box<Expr>, Box<SelectStmt>, bool),
    /// `(SELECT ...)` as a scalar operand (v0.6): the first
    /// column of the first row, or NULL when the subquery
    /// yields no rows.
    ScalarSubquery(Box<SelectStmt>),
    /// `EXISTS (SELECT ...)` (v0.6).
    ExistsSubquery(Box<SelectStmt>),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    /// CASE: optional base operand, (when, then) pairs, optional
    /// ELSE.
    Case {
        base: Option<Box<Expr>>,
        whens: AVec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// Scalar or aggregate function call; `star` is `count(*)`.
    Func {
        name: ABytes,
        distinct: bool,
        star: bool,
        args: AVec<Expr>,
    },
    Cast(Box<Expr>, ABytes),
    /// A hexadecimal literal too big for 64 bits (more than 16
    /// significant digits, or `-` over `0x8000000000000000`) at
    /// this byte offset (v0.16 review fix, evaluator builds):
    /// parsed, and refused with `ParseKind::HexLiteralTooBig`
    /// when it resolves — the pin's "hex literal too big"
    /// comes from code generation, so a CREATE TABLE holding one
    /// in a DEFAULT / CHECK / generated column is accepted and
    /// only the statement that evaluates it fails.
    #[cfg(feature = "v04")]
    HexTooBig(usize),
    /// `expr COLLATE name` (v0.16, `v16`): the operand with an
    /// explicit collating sequence, which wins every comparison
    /// it takes part in (the pin's `EP_Collate` rules).
    #[cfg(feature = "v16")]
    Collate(Box<Expr>, Coll),
    /// A bound parameter (v0.16, feature `params`): its 1-based
    /// index and its token text (`?`, `?2`, `:name`, ...). It
    /// never reaches the planner from a prepared statement: each
    /// execution plans a copy of the tree with every parameter
    /// replaced by its bound value as a literal (see
    /// `session::bind_ast`). In a stored schema text (where the
    /// pin silently reads one as NULL) it resolves to NULL.
    #[cfg(feature = "params")]
    Param(u16, ABytes),
    /// `~x`, bitwise NOT (v0.16, `v16`): the operand as a 64-bit
    /// integer, complemented; NULL stays NULL.
    #[cfg(feature = "v16")]
    BitNot(Box<Expr>),
    /// A row value `(a, b, ...)` of two or more expressions
    /// (v0.16, `v16`). Legal only as an operand of a comparison
    /// (`=`, `<>`, `<`, `<=`, `>`, `>=`, `IS [NOT]`), `BETWEEN`,
    /// `IN`, or a CASE base / WHEN operand, against a row value
    /// of the same size; anywhere else the planner refuses it
    /// (`SchemaKind::RowValueMisused`), as the pin does.
    #[cfg(feature = "v16")]
    Vector(AVec<Expr>),
    /// Element `i` (0-based) of a row value of `n` elements
    /// (v0.16, `v16`): the parser's split of `UPDATE ... SET
    /// (c1, ..., cn) = <row>` into one assignment per column. The
    /// row is a scalar subquery (column `i` of its first row) or
    /// any expression when `n` is 1; the planner checks the size.
    #[cfg(feature = "v16")]
    RowItem(Box<Expr>, u16, u16),
}

/// One SELECT result item (v0.4).
#[derive(Debug, Clone)]
pub enum SelectItem {
    Star,
    TableStar(ABytes),
    /// (expression, alias, source text of the expression) — the
    /// text is how SQLite names an unaliased expression column,
    /// which view column derivation needs.
    Expr(Expr, Option<ABytes>, ABytes),
}

/// How a FROM source joins the previous ones (v0.4; `Right` /
/// `Full` v0.16). The FROM clause is a left-deep tree: source
/// `i` joins the result of sources `0..i`. The pin's keyword
/// combinations map here as SQLite's `sqlite3JoinType` does —
/// LEFT and RIGHT together (`FULL`, or even `LEFT RIGHT`) are
/// `Full`, `OUTER` is only a modifier of LEFT / RIGHT / FULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
    /// `RIGHT [OUTER] JOIN` (v0.16): every row of this source
    /// appears; one that no left combination matched is emitted
    /// NULL-padded on the left after the whole left operand has
    /// been scanned (the pin's order).
    Right,
    /// `FULL [OUTER] JOIN` (v0.16): LEFT and RIGHT together.
    Full,
}

impl JoinKind {
    /// Unmatched left combinations are NULL-padded (LEFT, FULL).
    pub fn pads_right(self) -> bool {
        matches!(self, JoinKind::Left | JoinKind::Full)
    }
    /// Unmatched rows of this source are emitted NULL-padded on
    /// the left (RIGHT, FULL).
    pub fn pads_left(self) -> bool {
        matches!(self, JoinKind::Right | JoinKind::Full)
    }
}

/// The direction of one ORDER BY term (v0.16 adds NULL
/// placement): `nulls_first` is SQLite's default `!desc` (NULL
/// sorts smallest) unless `NULLS FIRST` / `NULLS LAST` was
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortOrder {
    pub desc: bool,
    pub nulls_first: bool,
    /// The term's collating sequence (v0.16): BINARY from the
    /// parser; the planner sets the term's collation (an explicit
    /// `COLLATE`, else the column's — the pin's
    /// `sqlite3ExprCollSeq`).
    pub coll: crate::record::Coll,
}

/// One FROM source (v0.4): a table (or view name, resolved at plan
/// time) with an optional alias, joined per `kind` with the
/// optional ON expression. The first source has kind Inner and no
/// ON. v0.9: the source may instead be a derived table —
/// `(SELECT ...) [[AS] alias]` — in which case `derived` holds
/// the subquery and `table` is empty. (The pinned oracle accepts
/// no column-name list after a derived alias: `... AS s (y)` is
/// a syntax error there, so none is parsed here either.)
#[derive(Debug, Clone)]
pub struct FromSource {
    pub table: ABytes,
    pub alias: Option<ABytes>,
    pub kind: JoinKind,
    pub on: Option<Expr>,
    /// A FROM-clause subquery (v0.9, feature `v09`).
    pub derived: Option<Box<SelectStmt>>,
    /// `NATURAL` was written on this join (v0.16): the join
    /// columns are this source's columns that any source to its
    /// left also has, in this source's column order. Never
    /// combined with `on` / `using` (the parser refuses that).
    #[cfg(feature = "v04")]
    pub natural: bool,
    /// `USING (col, ...)` column names as written (v0.16);
    /// empty when the clause is absent.
    #[cfg(feature = "v04")]
    pub using: AVec<ABytes>,
}

/// A compound SELECT operator (v0.9). All four operators have
/// the SAME precedence in SQLite and associate to the left —
/// verified on the pin: `SELECT 1 UNION SELECT 2 INTERSECT
/// SELECT 2` is `{2}` (i.e. `(1 UNION 2) INTERSECT 2`), not the
/// INTERSECT-binds-tighter reading other dialects use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    Union,
    UnionAll,
    Intersect,
    Except,
}

/// A full SELECT statement (v0.4 shape; the pre-v0.4 grammar
/// produces the single-source, simple-items special case).
#[derive(Debug, Clone)]
pub struct SelectStmt {
    /// `SELECT DISTINCT` (v0.4): duplicate output rows removed.
    pub distinct: bool,
    pub items: AVec<SelectItem>,
    pub from: AVec<FromSource>,
    pub filter: Option<Expr>,
    pub group_by: AVec<Expr>,
    pub having: Option<Expr>,
    /// (expression, direction) pairs (v0.16: the direction
    /// carries the NULLS FIRST / LAST placement).
    pub order_by: AVec<(Expr, SortOrder)>,
    /// `LIMIT` expression (v0.16: a general expression under
    /// the `v16` feature; an integer literal before it).
    pub limit: Option<Box<Expr>>,
    /// `OFFSET` expression (also the first term of the
    /// `LIMIT offset, count` form); None when absent.
    pub offset: Option<Box<Expr>>,
    /// Compound tail (v0.9, feature `v09`): `(op, core)` pairs
    /// in source order, applied left to right (all compound
    /// operators share one precedence on the pin — see
    /// `CompoundOp`). Each tail core is a plain SELECT or VALUES
    /// core with no compound, ORDER BY, or LIMIT of its own (the
    /// pin's grammar gives cores none); this statement's
    /// `order_by` / `limit` / `offset` then apply to the whole
    /// compound. Empty for an ordinary single-core SELECT.
    pub compound: AVec<(CompoundOp, SelectStmt)>,
    /// A VALUES core (v0.9, feature `v09`): when non-empty this
    /// "core" is `VALUES (row), (row), ...` and `items` / `from`
    /// are empty. All rows have the same width (validated at
    /// plan time). A VALUES core may head a compound or stand
    /// alone; when it is the LAST core of a select statement,
    /// the pin rejects a trailing ORDER BY / LIMIT as a syntax
    /// error, and so does the parser.
    pub values: AVec<AVec<Expr>>,
}

/// A statement conflict-resolution clause (v0.7): the `OR ...`
/// of `INSERT OR ...` / `UPDATE OR ...`, and the standalone
/// `REPLACE` verb (which is `INSERT` with `Replace`). `Abort`
/// covers ABORT, FAIL and ROLLBACK spellings: hipp's write-error
/// rule already aborts (and rolls back) the whole statement on
/// any error, which is the observable behaviour of all three in
/// autocommit and the documented v0.5 simplification inside an
/// explicit transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Abort,
    Ignore,
    Replace,
}

/// An `INSERT ... ON CONFLICT ...` upsert clause (v0.7).
#[derive(Debug, Clone)]
pub struct Upsert {
    /// Conflict-target column names as written; empty when the
    /// clause names no target (`ON CONFLICT DO ...` arbitrates
    /// any uniqueness conflict).
    pub target: AVec<ABytes>,
    /// Per-target-column explicit `COLLATE name` (v0.16),
    /// parallel to `target`: a target column with one matches
    /// only an index column of that collation (the pin's
    /// upsert-target rule); `None` matches any.
    pub target_colls: AVec<Option<crate::record::Coll>>,
    /// The target's optional index predicate (`ON CONFLICT (a)
    /// WHERE ...`): parsed and name-resolved at plan time (an
    /// unknown column errors, as in the oracle), but never
    /// evaluated — hipp has no partial indexes, and the oracle
    /// ignores the predicate for non-partial indexes too.
    pub target_filter: Option<Expr>,
    pub action: UpsertAction,
}

#[derive(Debug, Clone)]
pub enum UpsertAction {
    Nothing,
    Update {
        assignments: AVec<(ABytes, Expr)>,
        filter: Option<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum Ast {
    CreateTable {
        name: ABytes,
        if_not_exists: bool,
        columns: AVec<ColumnDef>,
        /// All PK columns, inline and table-level combined (v0.3);
        /// empty when the table declares no PRIMARY KEY.
        pk_columns: AVec<ABytes>,
        /// UNIQUE / PRIMARY KEY constraint sets in statement
        /// order (v0.3).
        constraint_sets: AVec<ConstraintSet>,
        /// CHECK constraint expression texts, column-level and
        /// table-level, in statement order (v0.16; the inside
        /// of each `CHECK (...)`).
        #[cfg(feature = "v04")]
        checks: AVec<ABytes>,
        /// The `STRICT` table option (v0.16, feature `v16`).
        #[cfg(feature = "v16")]
        strict: bool,
        /// `CREATE TABLE ... AS SELECT ...` (v0.16, feature
        /// `v16`): the columns are derived from the SELECT and
        /// `columns` is empty.
        #[cfg(feature = "v16")]
        as_select: Option<Box<SelectStmt>>,
    },
    /// `CREATE [UNIQUE] INDEX` (v0.3).
    CreateIndex {
        name: ABytes,
        table: ABytes,
        unique: bool,
        if_not_exists: bool,
        columns: AVec<IndexColumnDef>,
    },
    /// `DROP INDEX` (v0.3).
    DropIndex {
        name: ABytes,
        if_exists: bool,
    },
    Insert {
        table: ABytes,
        /// Explicit column list, if given.
        columns: Option<AVec<ABytes>>,
        /// One entry per `VALUES` row (v0.2: multi-row VALUES).
        /// v0.4: each value is an expression (a plain literal in
        /// the pre-v0.4 grammar), evaluated with no row context.
        /// Empty when the source is a SELECT (v0.8).
        rows: AVec<AVec<Expr>>,
        /// `INSERT INTO ... SELECT ...` source (v0.8, feature
        /// `v08`): the rows to insert are the SELECT's result
        /// rows, in result order. `None` for a VALUES source.
        select: Option<Box<SelectStmt>>,
        /// The `OR ...` conflict clause (v0.7; `REPLACE INTO`
        /// parses as Insert with `Replace`).
        conflict: ConflictAction,
        /// `ON CONFLICT ...` upsert clause (v0.7), if present.
        upsert: Option<Box<Upsert>>,
        /// `RETURNING ...` result items (v0.7), SELECT-item
        /// shaped; empty when the clause is absent.
        returning: AVec<SelectItem>,
    },
    /// `UPDATE [OR <action>] <t> [[AS] <alias>] SET <col> =
    /// <expr>, ... [FROM <sources>] [WHERE <expr>] [RETURNING
    /// ...]` (v0.5; FROM/RETURNING and OR REPLACE are v0.7). SET
    /// expressions and the filter evaluate against each row's
    /// original values (SQLite's rule, oracle-verified);
    /// duplicate SET columns are allowed and the last assignment
    /// wins. With FROM, a row matched by several FROM
    /// combinations is written once per combination, each time
    /// computed from its original values — the last combination
    /// wins (oracle-verified in the v0.7 probes).
    Update {
        table: ABytes,
        alias: Option<ABytes>,
        /// The `OR ...` conflict clause (v0.7): `Ignore` skips a
        /// row whose update would violate a UNIQUE constraint,
        /// `Replace` deletes the conflicting row(s) instead.
        conflict: ConflictAction,
        assignments: AVec<(ABytes, Expr)>,
        /// `FROM` sources (v0.7), in SELECT FROM shape; the
        /// target table is not among them (it is source 0 of the
        /// resolution scope, as in SQLite's model).
        from: AVec<FromSource>,
        filter: Option<Expr>,
        /// `RETURNING ...` result items (v0.7); empty when the
        /// clause is absent. Resolved against the target table
        /// only — SQLite refuses FROM columns there ("no such
        /// column", oracle-verified).
        returning: AVec<SelectItem>,
    },
    /// `DELETE FROM <t> [WHERE <expr>] [RETURNING ...]` (v0.5;
    /// RETURNING is v0.7).
    Delete {
        table: ABytes,
        filter: Option<Expr>,
        returning: AVec<SelectItem>,
    },
    /// `DROP TABLE [IF EXISTS] <name>` (v0.5).
    DropTable {
        name: ABytes,
        if_exists: bool,
    },
    Select(SelectStmt),
    /// `ALTER TABLE <t> ADD [COLUMN] <coldef>` (v0.4). `text` is
    /// the column definition exactly as written (SQL slice), used
    /// to rewrite the stored CREATE TABLE text the way SQLite
    /// does (`, <text>` inserted before the closing paren).
    AlterAddColumn {
        table: ABytes,
        column: ColumnDef,
        text: ABytes,
        /// The new column's CHECK constraint texts (v0.16): the
        /// pin verifies existing rows against them.
        #[cfg(feature = "v04")]
        checks: AVec<ABytes>,
    },
    /// `CREATE VIEW` (v0.4): stored SQL, inert — the SELECT is
    /// parsed and validated at plan time but never executed;
    /// selecting FROM a view is `Error::Unsupported`.
    CreateView {
        name: ABytes,
        if_not_exists: bool,
        /// Explicit column-name list, if written.
        columns: Option<AVec<ABytes>>,
        select: SelectStmt,
    },
    /// `DROP VIEW` (v0.4).
    DropView {
        name: ABytes,
        if_exists: bool,
    },
    /// `BEGIN [TRANSACTION]` (v0.2).
    Begin,
    /// `COMMIT [TRANSACTION]` / `END [TRANSACTION]` (v0.2).
    Commit,
    /// `ROLLBACK [TRANSACTION]` (v0.2).
    Rollback,
}
