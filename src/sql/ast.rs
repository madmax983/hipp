//! AST for the v0.1 grammar (SPEC §5.6). Text/blob bytes live in
//! the statement arena and are referenced by `ABytes` handles, so
//! the tree holds no self-referential borrows.

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
    /// A simple literal `DEFAULT`, when present and parseable as a
    /// literal (v0.1 materialises only literal defaults).
    pub default: Option<Literal>,
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
    },
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
