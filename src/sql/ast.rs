//! AST for the SQL grammar (SPEC §5.6; v0.2 adds multi-row
//! VALUES, `WHERE rowid =`, and transaction control; v0.3 adds
//! index statements and records UNIQUE/PK constraint sets).
//! Text/blob bytes live in the statement arena and are referenced
//! by `ABytes` handles, so the tree holds no self-referential
//! borrows.

use crate::arena::{ABytes, AVec};

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
    DropIndex { name: ABytes, if_exists: bool },
    Insert {
        table: ABytes,
        /// Explicit column list, if given.
        columns: Option<AVec<ABytes>>,
        /// One entry per `VALUES` row (v0.2: multi-row VALUES).
        rows: AVec<AVec<Literal>>,
    },
    Select {
        table: ABytes,
        list: SelectList,
        /// Optional `WHERE <column> = <literal>` (v0.2). The plan
        /// stage resolves whether the column is the rowid (or its
        /// alias); any other column is an executor-level rejection,
        /// and this pair is the seam general WHERE hangs off.
        predicate: Option<(ABytes, Literal)>,
    },
    /// `BEGIN [TRANSACTION]` (v0.2).
    Begin,
    /// `COMMIT [TRANSACTION]` / `END [TRANSACTION]` (v0.2).
    Commit,
    /// `ROLLBACK [TRANSACTION]` (v0.2).
    Rollback,
}
