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
    /// The column's own PRIMARY KEY constraint was written with
    /// `DESC` (the SQLite quirk: such a key is NOT the rowid alias
    /// even when the declared type is exactly INTEGER; a
    /// table-level `PRIMARY KEY (a DESC)` is, as on the pin).
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
    /// The DEFAULT clause's text as `PRAGMA table_info` shows it
    /// (v0.16, `v16`): the expression as written, the outer
    /// parentheses of `DEFAULT (expr)` removed.
    #[cfg(feature = "v16")]
    pub dflt_text: Option<ABytes>,
    /// The NOT NULL constraint's own `ON CONFLICT` clause (v0.16,
    /// `v07`): the last NOT NULL's, as on the pin; `None` when it
    /// has none (ABORT unless the statement's `OR` clause decides).
    #[cfg(feature = "v07")]
    pub nn_conflict: Option<ConflictAction>,
}

/// One FOREIGN KEY / REFERENCES clause of a `CREATE TABLE`
/// (v0.16, `v16`): `PRAGMA foreign_key_list` reports it, and the
/// `foreign_keys` leaf enforces it while `PRAGMA foreign_keys` is
/// ON. Actions are the pin's codes:
/// 0 NO ACTION, 1 RESTRICT, 2 SET NULL, 3 SET DEFAULT, 4 CASCADE.
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct ForeignKey {
    /// The child columns (one for a column-level REFERENCES).
    pub from: AVec<ABytes>,
    /// The parent table as written.
    pub table: ABytes,
    /// The parent columns; empty when none were written (the
    /// parent's PRIMARY KEY).
    pub to: AVec<ABytes>,
    pub on_update: u8,
    pub on_delete: u8,
    /// `DEFERRABLE INITIALLY DEFERRED` (the pin's `isDeferred`): a
    /// violation is counted against the transaction and checked at
    /// COMMIT. A column-level deferrable clause applies to the
    /// table's most recent foreign key, as on the pin.
    pub deferred: bool,
}

/// One UNIQUE or PRIMARY KEY constraint of a `CREATE TABLE`, in
/// statement order (v0.3): the catalog derives one implicit index
/// (`sqlite_autoindex_<table>_<n>`) per set that is not the rowid
/// alias, in exactly this order (verified against the oracle).
#[derive(Debug, Clone)]
pub struct ConstraintSet {
    pub primary_key: bool,
    pub columns: AVec<IndexColumnDef>,
    /// The constraint's own `ON CONFLICT` clause (v0.16, `v07`);
    /// `None` when it has none.
    #[cfg(feature = "v07")]
    pub conflict: Option<ConflictAction>,
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
    /// An expression key column (v0.16, `expr_index`): the item's
    /// text, `COLLATE` included (`name` is then empty). A bare
    /// column name, a lone string (the pin's `sqlite3StringToId`)
    /// and either under a `COLLATE` are column items, `None` here.
    #[cfg(feature = "expr_index")]
    pub expr: Option<ABytes>,
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
        /// The schema of a three-part name `schema.table.column`
        /// (v0.16, `v16`): the column of table `table` in that
        /// database (`main`, `temp`). `None` for one or two parts.
        #[cfg(feature = "v16")]
        schema: Option<ABytes>,
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
        /// The aggregate clauses written with the call (v0.16,
        /// `v16`): `f(args ORDER BY ...)` and `f(...) FILTER
        /// (WHERE ...)`. `None` when neither was written.
        #[cfg(feature = "v16")]
        clause: Option<Box<FuncClause>>,
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
    /// `RAISE(IGNORE)` / `RAISE(ROLLBACK | ABORT | FAIL, msg)`
    /// (v0.16, `triggers`), and whether it was parsed inside a
    /// trigger program: elsewhere it parses (a CHECK constraint may
    /// hold one, as on the pin) but does not resolve — the pin's
    /// "RAISE() may only be used within a trigger-program".
    /// Evaluating it ends the trigger program; the message is any
    /// expression, read as text.
    #[cfg(feature = "triggers")]
    Raise(RaiseKind, Option<Box<Expr>>, bool),
    /// A `NEW.col` / `OLD.col` reference of a trigger program,
    /// replaced by the firing row's value (v0.16, `triggers`; see
    /// `sql::trig`): the value, the comparison affinity it carries
    /// (the pin's: none for a column, INTEGER for the rowid) and its
    /// collating sequence (the column's; none for the rowid). Never
    /// produced by the parser.
    /// Also the `OLD.` / `NEW.` values of a foreign-key action
    /// program (`foreign_keys`).
    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
    TrigValue(Literal, Option<crate::record::Affinity>, Option<Coll>),
}

/// What `RAISE(...)` does (v0.16, `triggers`), as on the pin:
/// `Ignore` abandons the trigger program and the row change that
/// fired it (the statement goes on with its next row); `Rollback`
/// fails the statement and rolls back the transaction; `Abort`
/// fails and rolls back the statement; `Fail` fails the statement
/// keeping what it changed so far.
#[cfg(feature = "triggers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseKind {
    Ignore,
    Rollback,
    Abort,
    Fail,
}

/// When a trigger fires (v0.16, `triggers`).
#[cfg(feature = "triggers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrigTime {
    Before,
    After,
    /// `INSTEAD OF` (views only).
    Instead,
}

/// The row change a trigger fires on (v0.16, `triggers`).
#[cfg(feature = "triggers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrigEvent {
    Insert,
    Delete,
    Update,
}

/// A parsed `CREATE TRIGGER` (v0.16, `triggers`). The WHEN clause
/// and the body statements are kept as byte spans of the statement
/// text: a trigger program is parsed again (and planned) each time
/// it fires, with its `NEW.` / `OLD.` references bound to the row.
#[cfg(feature = "triggers")]
#[derive(Debug, Clone)]
pub struct CreateTrigger {
    /// The trigger's name (a `main.` / `temp.` qualifier dropped).
    pub name: ABytes,
    pub if_not_exists: bool,
    /// The name was written with a schema qualifier.
    pub qualified: bool,
    pub time: TrigTime,
    pub event: TrigEvent,
    /// `UPDATE OF col, ...`: empty for any UPDATE.
    pub of_cols: AVec<ABytes>,
    /// The table (or view) after `ON`, as written.
    pub table: ABytes,
    /// Whether the table was written `temp.` qualified (and,
    /// with `table_main`, `main.` qualified).
    pub table_temp: bool,
    pub table_main: bool,
    /// The WHEN expression's text span (start, end).
    pub when: Option<(usize, usize)>,
    /// Each body statement's text span, without its `;`.
    pub steps: AVec<(usize, usize)>,
    /// Byte offset of the trigger's name token: the stored text is
    /// `CREATE TRIGGER ` followed by the statement from here.
    pub name_at: usize,
    /// One past the closing `END`.
    pub end: usize,
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

/// The aggregate clauses of a function call (v0.16, `v16`): the
/// ORDER BY written inside its parentheses (empty when none) and
/// its `FILTER (WHERE ...)` expression. The planner refuses both
/// on a function that is not an aggregate, as the pin does.
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct FuncClause {
    pub order_by: AVec<(Expr, SortOrder)>,
    pub filter: Option<Expr>,
    /// The call's `OVER` clause (v0.16, `window`): the call is a
    /// window function. `None` when none was written.
    #[cfg(feature = "window")]
    pub over: Option<Over>,
}

/// A call's `OVER` clause (v0.16, `window`).
#[cfg(feature = "window")]
#[derive(Debug, Clone)]
pub enum Over {
    /// `OVER name`: the named window exactly as defined (its
    /// frame included).
    Named(ABytes),
    /// `OVER (...)`: a window definition, possibly naming a base
    /// window to inherit from.
    Spec(Box<WindowDef>),
}

/// A window definition (v0.16, `window`): `[base] [PARTITION BY
/// ...] [ORDER BY ...] [frame]`, as written in an `OVER (...)` or
/// a `WINDOW name AS (...)` clause. The parser has already copied
/// a `WINDOW` definition's base into it (the pin's
/// `sqlite3WindowChain`); an `OVER (base ...)`'s base is resolved
/// by the planner against the select's `WINDOW` list.
#[cfg(feature = "window")]
#[derive(Debug, Clone)]
pub struct WindowDef {
    /// The base window named first inside the parentheses.
    pub base: Option<ABytes>,
    pub partition: AVec<Expr>,
    pub order_by: AVec<(Expr, SortOrder)>,
    /// The frame; `None` when none was written (the pin's
    /// implicit `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT
    /// ROW`, which a derived window may still override).
    pub frame: Option<Frame>,
}

/// Inherit a window definition's base (v0.16, `window`; the pin's
/// `sqlite3WindowChain`): `w`'s base names the latest definition
/// of that name in `list`; `w` takes its PARTITION BY and, when
/// it has one, its ORDER BY. The pin refuses (`WindowOverride`) a
/// `w` that restates the PARTITION BY, that has an ORDER BY when
/// the base does, or whose base has an explicit frame.
#[cfg(feature = "window")]
pub fn chain_window(
    w: &mut WindowDef,
    list: &[(ABytes, WindowDef)],
    arena: &crate::arena::Arena,
) -> Result<(), crate::error::SchemaKind> {
    use crate::error::SchemaKind;
    let Some(base) = w.base else {
        return Ok(());
    };
    let name = crate::sql::handle_bytes(arena, base);
    let Some((_, b)) = list
        .iter()
        .rev()
        .find(|(n, _)| crate::sql::eq_ci(crate::sql::handle_bytes(arena, *n), name))
    else {
        return Err(SchemaKind::NoSuchWindow);
    };
    if !w.partition.is_empty()
        || (!b.order_by.is_empty() && !w.order_by.is_empty())
        || b.frame.is_some()
    {
        return Err(SchemaKind::WindowOverride);
    }
    w.partition = b.partition.clone();
    if !b.order_by.is_empty() {
        w.order_by = b.order_by.clone();
    }
    w.base = None;
    Ok(())
}

#[cfg(feature = "window")]
impl WindowDef {
    /// Every expression of the definition: PARTITION BY, ORDER BY,
    /// then the frame offsets.
    pub fn exprs_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        for p in self.partition.as_mut_slice() {
            f(p);
        }
        for (o, _) in self.order_by.as_mut_slice() {
            f(o);
        }
        if let Some(fr) = &mut self.frame {
            for b in [&mut fr.start, &mut fr.end] {
                if let FrameBound::Preceding(e) | FrameBound::Following(e) = b {
                    f(e);
                }
            }
        }
    }

    /// `exprs_mut`, read-only.
    pub fn exprs(&self, f: &mut dyn FnMut(&Expr)) {
        for p in self.partition.iter() {
            f(p);
        }
        for (o, _) in self.order_by.iter() {
            f(o);
        }
        if let Some(fr) = &self.frame {
            for b in [&fr.start, &fr.end] {
                if let FrameBound::Preceding(e) | FrameBound::Following(e) = b {
                    f(e);
                }
            }
        }
    }
}

/// A window frame (v0.16, `window`).
#[cfg(feature = "window")]
#[derive(Debug, Clone)]
pub struct Frame {
    pub unit: FrameUnit,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclude: FrameExclude,
}

/// `ROWS`, `RANGE` or `GROUPS` (v0.16, `window`).
#[cfg(feature = "window")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnit {
    Rows,
    Range,
    Groups,
}

/// One frame boundary (v0.16, `window`). An offset that is not a
/// constant expression is NULL, as the pin's
/// `sqlite3WindowOffsetExpr` makes it (so it fails with the
/// pin's "frame ... offset must be a non-negative ..." error
/// when a partition is read).
#[cfg(feature = "window")]
#[derive(Debug, Clone)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

/// A frame's `EXCLUDE` clause (v0.16, `window`). `NoOthers` is
/// kept apart from no clause at all: on the pin it selects the
/// full-scan frame engine (and makes the window compare unequal
/// to one without the clause).
#[cfg(feature = "window")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameExclude {
    None,
    NoOthers,
    CurrentRow,
    Group,
    Ties,
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
    /// A table-valued function's arguments (v0.16, `v16`):
    /// `pragma_table_info('t')` and friends. `None` for an
    /// ordinary table name.
    #[cfg(feature = "v16")]
    pub args: Option<AVec<Expr>>,
    /// The name was written with its schema qualifier (`main.t`;
    /// v0.16, `v16`): such a name never denotes a common table
    /// expression, as on the pin.
    #[cfg(feature = "v16")]
    pub qualified: bool,
    /// The qualifier was `temp` (v0.16, `v16`): the TEMP database's
    /// table, which a main statement reads there.
    #[cfg(feature = "v16")]
    pub in_temp: bool,
    /// The schema qualifier as written (v0.16, `attach`): `aux` of
    /// `aux.t` — `main` and `temp` included — or `None`. The planner
    /// reads an attached database's table through it
    /// (`session::attach`).
    #[cfg(feature = "attach")]
    pub schema: Option<ABytes>,
    /// What the planner's view / CTE expansion made of this
    /// source (v0.16, `v16`; `None` from the parser): see
    /// `CteUse`.
    #[cfg(feature = "v16")]
    pub cte: Option<Box<CteUse>>,
    /// `INDEXED BY name` / `NOT INDEXED` after the source (v0.16,
    /// `v16`).
    #[cfg(feature = "v16")]
    pub indexed: Option<IndexHint>,
    /// The pin's query flattener merges this FROM subquery (a view's
    /// or CTE's expansion, or a written one) into its reader (v0.17,
    /// `flatten`; `sqlite3FlattenSubquery`'s restrictions, settled by
    /// the view / CTE expansion): the planner plans its body without
    /// access paths and splices its sources into the reader's FROM
    /// clause (`session::flatten`). False from the parser.
    #[cfg(feature = "flatten")]
    pub flat: bool,
}

/// A FROM source's (or an UPDATE / DELETE target's) index clause
/// (v0.16, `v16`): `INDEXED BY name` makes the planner use that
/// index of the table (the pin's "no such index" when the table has
/// none of the name, "no query solution" when it is a partial index
/// the WHERE does not imply); `NOT INDEXED` makes it use none.
#[cfg(feature = "v16")]
#[derive(Debug, Clone, Copy)]
pub enum IndexHint {
    By(ABytes),
    Not,
}

/// A FROM source that the view / common-table-expression
/// expansion rewrote (v0.16, `v16`; `session::cte`). The parser
/// never builds one: the expansion turns a source naming a view
/// or a CTE into a derived table holding a copy of its SELECT and
/// records here what the derived table alone cannot say.
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct CteUse {
    /// The view's or CTE's column-name list, empty when none was
    /// written. The planner checks its length against the SELECT's
    /// width when the source is used (the pin's "expected N
    /// columns" / "table x has N values for M columns").
    pub columns: AVec<ABytes>,
    /// The recursion role of the source.
    pub rec: RecMark,
    /// The source is a view (not a CTE): its unaliased column
    /// references are named by their tables' own column names (the
    /// pin resolves a view's SELECT before naming its columns, a
    /// CTE's or FROM subquery's after: `SELECT rowid FROM t` names
    /// its column after t's INTEGER PRIMARY KEY only in a view).
    pub view: bool,
    /// The shared materialization this source reads (v0.16 review
    /// fix; 0: none, the derived table is computed for this
    /// reference alone). Every source with the same non-zero id
    /// reads one result, computed once per statement execution —
    /// the pin's reuse of a materialized CTE (and of a view that
    /// a FROM clause names twice); see `session::cte`.
    pub mat: u32,
    /// The source names a CTE (v0.17 review fix, `flatten`): the pin
    /// computes every result column of a CTE reference it does not
    /// flatten (`disableUnusedSubqueryResultColumns` skips `isCte`).
    #[cfg(feature = "flatten")]
    pub is_cte: bool,
    /// A `NOT MATERIALIZED` CTE read more than once (v0.17 review fix,
    /// `flatten`): `mat` is shared by the references the pin
    /// materializes, while one it can run as a co-routine
    /// (`fromClauseTermCanBeCoroutine`) is computed alone.
    #[cfg(feature = "flatten")]
    pub lazy: bool,
    /// The view group this reference belongs to (v0.17 review fix,
    /// `flatten`; 0: none): every reference of one view in one FROM
    /// clause, kept when `mat` is cleared, so the planner can settle
    /// the pin's `isSelfJoinView` reuse once it knows which copies
    /// lost unused columns.
    #[cfg(feature = "flatten")]
    pub grp: u32,
}

/// A CTE's `MATERIALIZED` / `NOT MATERIALIZED` hint (v0.16, `v16`).
#[cfg(feature = "v16")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum M10d {
    /// No hint.
    Any,
    /// `AS MATERIALIZED`: computed once, never flattened.
    Yes,
    /// `AS NOT MATERIALIZED`.
    No,
}

/// A source's part in a recursive CTE (v0.16, `v16`).
#[cfg(feature = "v16")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecMark {
    /// An ordinary view / CTE use.
    None,
    /// The derived table is a recursive CTE: compound cores
    /// `split..` (counting the head core as 0) are the recursive
    /// SELECTs, the ones before them the setup; `id` names the
    /// CTE's recursive references inside them.
    Body { split: usize, id: u32 },
    /// The one recursive reference (by `id`) in a recursive
    /// SELECT's FROM clause: it reads the row the queue just gave
    /// up.
    Ref { id: u32 },
}

/// A `WITH [RECURSIVE] cte, ...` clause (v0.16, `v16`). The
/// planner expands every reference before planning (the parser
/// keeps the clause where it was written).
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct With {
    pub ctes: AVec<Cte>,
}

/// One common table expression, `name [(col, ...)] AS [[NOT]
/// MATERIALIZED] (select)` (v0.16, `v16`). The hint decides, with
/// the number of references, whether the references share one
/// materialization (see `session::cte`).
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct Cte {
    pub name: ABytes,
    /// The column-name list, empty when none was written.
    pub columns: AVec<ABytes>,
    pub select: SelectStmt,
    /// The `[NOT] MATERIALIZED` hint.
    pub hint: M10d,
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
    /// The statement's `WITH` clause (v0.16, `v16`): its CTEs are
    /// visible to every core, every nested subquery and each
    /// other.
    #[cfg(feature = "v16")]
    pub with: Option<Box<With>>,
    /// The pin's name-resolution depth around this select (v0.16
    /// depth limits, `v16`): the height of the expression of the
    /// enclosing select it is resolved inside (`Parse.nHeight`
    /// grows by that much on the way in); 0 for a statement, a FROM
    /// subquery, a CTE or a view body. Set by the parser.
    #[cfg(feature = "v16")]
    pub rctx: u32,
    /// The deepest sum of nested expression heights that resolving
    /// this select on its own reaches (the parser's `sel_r`; v0.16,
    /// `v16`). A view or CTE body is resolved where it is read, so
    /// the expansion checks the reference's depth plus this.
    #[cfg(feature = "v16")]
    pub rdepth: u32,
    /// The core's `WINDOW name AS (...)` definitions (v0.16,
    /// `window`), in source order; a later definition of a name
    /// shadows an earlier one, as on the pin.
    #[cfg(feature = "window")]
    pub windows: AVec<(ABytes, WindowDef)>,
    /// The pin's select number (`Select.selId`; v0.16, `explain`):
    /// cores are numbered as the pin's parser reduces them (every
    /// subquery a core holds before the core itself, a compound's
    /// cores left to right, WITH bodies first); the copy of a view
    /// read by a statement is renumbered after the statement's own
    /// selects. EXPLAIN QUERY PLAN prints it (`SCALAR SUBQUERY n`,
    /// `(subquery-n)`).
    #[cfg(feature = "explain")]
    pub sel: u32,
    /// The select is a scalar or EXISTS subquery, which reads its
    /// first row only (v0.16, `expr_index`): a UNION compound there
    /// takes the pin's first-row shortcut, so its members are not
    /// planned for a merge key (`session::merge_member`).
    #[cfg(all(feature = "expr_index", feature = "v09"))]
    pub first_row: bool,
}

/// A statement conflict-resolution clause (v0.7): the `OR ...`
/// of `INSERT OR ...` / `UPDATE OR ...`, and the standalone
/// `REPLACE` verb (which is `INSERT` with `Replace`). Without
/// `v16`, `Abort` also covers the ROLLBACK spelling: hipp's
/// write-error rule there rolls back the whole transaction on any
/// error, which is ROLLBACK's behaviour and ABORT's in autocommit.
/// With `v16` a failed statement inside a transaction undoes only
/// itself (ABORT), and ROLLBACK has its own variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Abort,
    /// `OR FAIL` (v0.16 review fix, ungated): a constraint
    /// violation (UNIQUE / PRIMARY KEY, NOT NULL, CHECK) errors
    /// like `Abort`, but the rows the statement changed before it
    /// stay changed (committed in autocommit; an explicit
    /// transaction stays open). Other errors still roll back.
    /// Parsed only with UPDATE (v0.5) / INSERT OR (v0.7).
    #[cfg(feature = "v05")]
    Fail,
    Ignore,
    Replace,
    /// `OR ROLLBACK` (v0.16, `v16`): a constraint violation
    /// (UNIQUE / PRIMARY KEY, NOT NULL, CHECK) errors like `Abort`
    /// and also rolls back the whole transaction, ending it (in
    /// autocommit it is `Abort`). Without `v16` OR ROLLBACK parses
    /// as `Abort`: a failed statement then ends the transaction
    /// anyway.
    #[cfg(feature = "v16")]
    Rollback,
    /// No `OR` clause at all (v0.16, `v07`): each constraint's own
    /// `ON CONFLICT` clause decides, ABORT when it has none (the
    /// pin's `OE_Default`). Only the parser produces it; the
    /// planner resolves it per constraint and hands the executors
    /// `Abort` as the statement action.
    #[cfg(feature = "v07")]
    Unset,
}

impl ConflictAction {
    /// The action a constraint whose own clause is `own` resolves
    /// by under this statement action (v0.16, `v07`): an explicit
    /// `OR` clause wins, then the constraint's clause, then ABORT.
    #[cfg(feature = "v07")]
    pub fn over(self, own: Option<ConflictAction>) -> ConflictAction {
        match self {
            ConflictAction::Unset => match own {
                Some(ConflictAction::Unset) | None => ConflictAction::Abort,
                Some(a) => a,
            },
            a => a,
        }
    }

    /// Whether a conflict fails the statement without resolving
    /// it in place: ABORT, or (v0.16) ROLLBACK, which differs only
    /// in ending the transaction afterwards.
    pub fn aborts(self) -> bool {
        match self {
            ConflictAction::Abort => true,
            #[cfg(feature = "v16")]
            ConflictAction::Rollback => true,
            #[cfg(feature = "v07")]
            ConflictAction::Unset => true,
            _ => false,
        }
    }
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
    /// Per target item (v0.16, `expr_index`), parallel to `target`:
    /// an expression item (`ON CONFLICT (a + b)`), its `COLLATE`
    /// in `target_colls`; `None` for a column (named in `target`).
    #[cfg(feature = "expr_index")]
    pub target_exprs: AVec<Option<Expr>>,
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
        /// The `WITHOUT ROWID` table option (v0.16, feature
        /// `without_rowid`).
        #[cfg(feature = "without_rowid")]
        without_rowid: bool,
        /// `CREATE TABLE ... AS SELECT ...` (v0.16, feature
        /// `v16`): the columns are derived from the SELECT and
        /// `columns` is empty.
        #[cfg(feature = "v16")]
        as_select: Option<Box<SelectStmt>>,
        /// FOREIGN KEY / REFERENCES clauses in statement order
        /// (v0.16, `v16`).
        #[cfg(feature = "v16")]
        fkeys: AVec<ForeignKey>,
    },
    /// `CREATE [UNIQUE] INDEX` (v0.3).
    CreateIndex {
        name: ABytes,
        table: ABytes,
        unique: bool,
        if_not_exists: bool,
        columns: AVec<IndexColumnDef>,
        /// A partial index's `WHERE` text (v0.16, `expr_index`).
        #[cfg(feature = "expr_index")]
        filter: Option<ABytes>,
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
        /// The target's `INDEXED BY` / `NOT INDEXED` (v0.16, `v16`).
        #[cfg(feature = "v16")]
        indexed: Option<IndexHint>,
    },
    /// `DELETE FROM <t> [WHERE <expr>] [RETURNING ...]` (v0.5;
    /// RETURNING is v0.7).
    Delete {
        table: ABytes,
        filter: Option<Expr>,
        returning: AVec<SelectItem>,
        /// The target's `INDEXED BY` / `NOT INDEXED` (v0.16, `v16`).
        #[cfg(feature = "v16")]
        indexed: Option<IndexHint>,
    },
    /// `CREATE VIRTUAL TABLE [IF NOT EXISTS] name USING module
    /// [(args)]` (v0.16, `vtab`): `args` are the module arguments
    /// as written, `text` what the pin stores after `CREATE VIRTUAL
    /// TABLE ` (the statement from the name token on).
    #[cfg(feature = "vtab")]
    CreateVirtual {
        name: ABytes,
        if_not_exists: bool,
        module: ABytes,
        args: AVec<ABytes>,
        text: ABytes,
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
    /// `ALTER TABLE <t> RENAME TO <new>` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    AlterRename {
        table: ABytes,
        to: ABytes,
    },
    /// `ALTER TABLE <t> RENAME [COLUMN] <old> TO <new>` (v0.16,
    /// `v16`). `quoted`: the new name was written quoted (or as a
    /// string), so every edited token takes the quoted form, as on
    /// the pin.
    #[cfg(feature = "v16")]
    AlterRenameColumn {
        table: ABytes,
        from: ABytes,
        to: ABytes,
        quoted: bool,
    },
    /// `ALTER TABLE <t> DROP [COLUMN] <c>` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    AlterDropColumn {
        table: ABytes,
        column: ABytes,
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
    /// `BEGIN IMMEDIATE | EXCLUSIVE [TRANSACTION]` (v0.16, `v16`):
    /// a BEGIN that, like the pin's write lock taken at once, fixes
    /// an empty database's page size (`Pager::fix_layout`).
    #[cfg(feature = "v16")]
    BeginImmediate,
    /// `COMMIT [TRANSACTION]` / `END [TRANSACTION]` (v0.2).
    Commit,
    /// `ROLLBACK [TRANSACTION]` (v0.2).
    Rollback,
    /// `SAVEPOINT name` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Savepoint {
        name: ABytes,
    },
    /// `RELEASE [SAVEPOINT] name` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Release {
        name: ABytes,
    },
    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] name` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    RollbackTo {
        name: ABytes,
    },
    /// `VACUUM [schema] [INTO expr]` (v0.16, `v16`): `into` is the
    /// expression naming the target file (any expression with a
    /// text value: a literal, `?`, `'a' || 'b'`, a subquery).
    #[cfg(feature = "v16")]
    Vacuum {
        schema: Option<ABytes>,
        into: Option<Expr>,
    },
    /// `REINDEX [[schema .] name]` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Reindex {
        schema: Option<ABytes>,
        name: Option<ABytes>,
    },
    /// `ANALYZE [[schema .] name]` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Analyze {
        schema: Option<ABytes>,
        name: Option<ABytes>,
    },
    /// `WITH ... INSERT / REPLACE / UPDATE / DELETE` (v0.16,
    /// `v16`): the clause's CTEs are visible throughout the
    /// statement (its SELECT, FROM, expressions and subqueries,
    /// never as its target table). A `WITH ... SELECT` keeps its
    /// clause in `SelectStmt::with` instead.
    #[cfg(feature = "v16")]
    With {
        with: Box<With>,
        body: Box<Ast>,
    },
    /// `PRAGMA [schema .] name [= value | (value)]` (v0.16,
    /// `v16`). `value` is the argument's text as the pin hands it
    /// to its pragma code: a name or string dequoted, a number as
    /// written (with a leading `-` for a negative one, `+`
    /// dropped).
    #[cfg(feature = "v16")]
    Pragma {
        schema: Option<ABytes>,
        name: ABytes,
        value: Option<ABytes>,
        /// The argument was a number token (the pin treats
        /// `integrity_check(5)` as a limit, `('5')` as a name).
        numeric: bool,
    },
    /// `ATTACH [DATABASE] file AS name [KEY key]` (v0.16, `attach`):
    /// the operands as expressions (a lone identifier already read
    /// as its text, as on the pin).
    #[cfg(feature = "attach")]
    Attach {
        file: Expr,
        name: Expr,
        key: Option<Expr>,
    },
    /// `DETACH [DATABASE] name` (v0.16, `attach`).
    #[cfg(feature = "attach")]
    Detach {
        name: Expr,
    },
    /// `CREATE TRIGGER` (v0.16, `triggers`).
    #[cfg(feature = "triggers")]
    CreateTrigger(Box<CreateTrigger>),
    /// `DROP TRIGGER [IF EXISTS] [schema .] name` (v0.16,
    /// `triggers`).
    #[cfg(feature = "triggers")]
    DropTrigger {
        name: ABytes,
        if_exists: bool,
    },
}
