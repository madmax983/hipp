//! AST for the SQL grammar (SPEC §5.6; v0.2 adds multi-row
//! VALUES, `WHERE rowid =`, and transaction control; v0.3 adds
//! index statements and records UNIQUE/PK constraint sets).
//! Text/blob bytes live in the statement arena and are referenced
//! by `ABytes` handles, so the tree holds no self-referential
//! borrows.

use crate::arena::{ABytes, AVec};
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
    /// recorded for ALTER TABLE ADD COLUMN validation; still not
    /// enforced on INSERT, matching the v0.1 note).
    pub not_null: bool,
    /// A `DEFAULT` was present but is not a plain literal
    /// (e.g. `CURRENT_TIMESTAMP`, a function call) (v0.4).
    pub default_nonconst: bool,
    /// `GENERATED ALWAYS AS` appeared (v0.4: recorded so ADD
    /// COLUMN can refuse generated columns).
    pub generated: bool,
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

/// How a FROM source joins the previous ones (v0.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

/// One FROM source (v0.4): a table (or view name, resolved at plan
/// time) with an optional alias, joined per `kind` with the
/// optional ON expression. The first source has kind Inner and no
/// ON.
#[derive(Debug, Clone)]
pub struct FromSource {
    pub table: ABytes,
    pub alias: Option<ABytes>,
    pub kind: JoinKind,
    pub on: Option<Expr>,
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
    /// (expression, descending) pairs.
    pub order_by: AVec<(Expr, bool)>,
    pub limit: Option<i64>,
    pub offset: i64,
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
