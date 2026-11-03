//! Session API (SPEC §5.8): `Connection` / `Statement` over the
//! async-primitives `Storage` trait, with the planner and the
//! tree-walking executor (§5.7).
//!
//! Ownership encodes the spec's concurrency rules: a `Statement`
//! borrows its `Connection` mutably, so one operation is in flight
//! per connection (§5.1.2) and a catalog rebuild can only happen
//! when no statement is live (§7.5.5) — both by construction.

#[cfg(feature = "v04")]
use alloc::boxed::Box;

use crate::arena::{AVec, Arena};
use crate::btree::{self, ScanCursor};
use crate::error::{Error, Result, SchemaKind, Scope};
use crate::pager::Pager;
use crate::record::{OwnedValue, Value, apply_affinity, encode_record};
use crate::schema::{Catalog, build_catalog};
use crate::sql::{
    Ast, BinOp, ConflictAction, Expr, Literal, SelectItem, SelectStmt, eq_ci, handle_bytes,
};
use crate::storage::Storage;

#[cfg(feature = "indexes")]
use crate::schema::IndexOrigin;

#[cfg(feature = "v04")]
use crate::query::{Ctx, RExpr, RowData};

/// Undo a write statement that failed with error `$e` (§7.5.4: a
/// failed write leaves no trace). Inside a transaction with `v16`,
/// only the statement's own savepoint is rolled back and the
/// transaction stays open, as on the pin — unless the error is a
/// storage, memory or cache failure, after which (as the pin's
/// I/O and out-of-memory errors do) the whole transaction rolls
/// back (`Statement::stmt_rollback`). Otherwise (autocommit, or a
/// build without `v16`, where a failed statement still ends the
/// whole bracket) the transaction is rolled back. A macro, so the
/// tiny profile's abort stays the inline two statements it was.
macro_rules! abort_write {
    ($s:ident, $e:expr) => {{
        // A statement of a trigger program (v0.16, `triggers`) undoes
        // nothing itself: the statement that fired the trigger does,
        // as the failure's resolution says (`session::trigger`).
        // (A foreign-key action program's statements likewise.)
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        let undone = $s.conn.in_program() || $s.stmt_rollback($e).await;
        #[cfg(all(
            feature = "v16",
            not(any(feature = "triggers", feature = "foreign_keys"))
        ))]
        let undone = $s.stmt_rollback($e).await;
        #[cfg(not(feature = "v16"))]
        let undone = {
            let _ = $e;
            false
        };
        if !undone {
            let _ = $s.conn.pager.rollback().await;
            $s.conn.explicit_txn = false;
        }
    }};
}

#[cfg(feature = "v16")]
mod alter;
#[cfg(feature = "v16")]
mod cte;
#[cfg(feature = "foreign_keys")]
mod fkey;
#[cfg(feature = "v16")]
mod maint;
#[cfg(feature = "v16")]
mod pragma;
#[cfg(feature = "v16")]
mod temp;
#[cfg(feature = "triggers")]
mod trigger;

/// Output-column sentinel for the `rowid` pseudo-column in a scan
/// plan (also `oid` / `_rowid_` when no real column bears the name).
const ROWID_COL: usize = usize::MAX;

/// Caller configuration (SPEC §5.8, §7.5.3): page-pool size and
/// per-scope arena initial/maximum bytes.
#[derive(Debug, Clone)]
pub struct Config {
    pub cache_pages: usize,
    /// Page size used when creating a new database file.
    pub create_page_size: u32,
    pub stmt_init: usize,
    pub stmt_max: usize,
    pub txn_init: usize,
    pub txn_max: usize,
    pub catalog_init: usize,
    pub catalog_max: usize,
    /// Seed for journal nonces. Targets with real entropy should
    /// seed this; the built-in mixing is the documented floor
    /// (SPEC §5.3, RISKS R2).
    pub nonce_seed: u64,
    /// The wall clock (v0.16): a caller-supplied function
    /// returning the current Unix time in milliseconds, read by
    /// `DEFAULT CURRENT_TIME` / `CURRENT_DATE` /
    /// `CURRENT_TIMESTAMP` (at most once per statement). The
    /// core has no clock of its own; with `None` a statement
    /// that needs one fails with `Error::NoClock`.
    #[cfg(feature = "v04")]
    pub clock: Option<fn() -> i64>,
    /// The `VACUUM INTO 'file'` writer (v0.16, `v16`): called with
    /// the file name as written and the finished database image,
    /// it creates the file and returns `true`, or returns `false`
    /// (the statement then fails with `SchemaKind::VacuumTarget`)
    /// when the file already holds data or cannot be written. The
    /// core opens no files itself; with `None`, VACUUM INTO is
    /// `Error::Unsupported`. With feature `std` the default is
    /// `storage::write_new_file`.
    #[cfg(feature = "v16")]
    pub vacuum_into: Option<VacuumWriter>,
    /// How deep view and CTE bodies may nest inside one another
    /// (v0.16, `v16`): a statement reaching this many levels fails
    /// with `SchemaKind::NestedTooDeep`. The pin allows 999 nested
    /// views (1000 is its "nested too deep"). Each level costs
    /// hipp's planner and executor stack — measured on x86_64 up to
    /// about 24 KiB in a release build (a view reading the last one
    /// in an `IN` subquery) and 130 KiB in a debug one — on top of
    /// what `max_expr_depth` / `max_parser_depth` bound, so the
    /// default (250) suits an 8 MiB thread in release (about 32 MiB
    /// in debug); `Config::stack_budget` does not lower it. Raise it
    /// to 1000 for the pin's limit on a thread with the stack for it
    /// (the CLI does).
    #[cfg(feature = "v16")]
    pub max_nesting: usize,
    /// The deepest expression a statement may hold (v0.16), counted
    /// as the pin counts `SQLITE_LIMIT_EXPR_DEPTH` (whose default,
    /// 1000, is `sql::parser::PIN_EXPR_DEPTH`); deeper is
    /// `ParseKind::ExprTooDeep`. Every recursive walk of a statement
    /// — the parser, the resolver and planner, the evaluator, the
    /// tree's Clone and Drop — is bounded by it and by
    /// `max_parser_depth`. A view or CTE body counts where it is
    /// read, as on the pin: the depths of a chain of bodies read
    /// inside one another add up to this limit, and the chain is at
    /// most `max_nesting` long. Together they bound the stack a
    /// statement's own text needs: about `STACK_BASE + max_parser_depth *
    /// STACK_PER_SYMBOL + max_expr_depth * STACK_PER_HEIGHT` bytes
    /// (measured on x86_64; see `Config::stack_budget`). The
    /// defaults (1000, the pin's, and 500 symbols) need about
    /// 1.4 MiB in a release build and 5.3 MiB in a debug one; the
    /// pin's limits about 4.3 MiB and 13 MiB — set them on a thread
    /// with that stack (the CLI does), or derive limits from the
    /// stack you have with `stack_budget`. The views and CTEs a
    /// statement reads cost their nesting levels on top (see
    /// `max_nesting`).
    #[cfg(feature = "v04")]
    pub max_expr_depth: usize,
    /// The deepest nesting the parser takes (v0.16), counted in the
    /// pin's LALR parser-stack symbols (the pin: 2500,
    /// `sql::parser::PIN_PARSER_DEPTH` — about 2490 nested
    /// parentheses, 830 nested function calls, 410 nested FROM
    /// subqueries); deeper is `ParseKind::RecursionLimit` ("Recursion
    /// limit").
    #[cfg(feature = "v04")]
    pub max_parser_depth: usize,
    /// How deep trigger programs may nest (v0.16, `triggers`): a
    /// trigger that would run as program number
    /// `max_trigger_depth + 1` of a nest fails its statement with
    /// `SchemaKind::TriggerDepth` (the pin's limit is 1000; a program
    /// counts before its WHEN clause is read). Each level costs
    /// hipp's planner and executor stack — up to `STACK_PER_TRIGGER`
    /// (measured on x86_64 at about 15 KiB in a release build and
    /// 262 KiB in a debug one, a recursive trigger with a simple
    /// body), on top of what each level's statement needs by
    /// `max_expr_depth` / `max_parser_depth`. The default (24, v0.16
    /// review fix: 100 overflowed a 2 MiB thread) leaves a 2 MiB
    /// release thread room for the default statement limits, and
    /// fits an 8 MiB thread in a debug build; `Config::stack_budget`
    /// lowers it to the levels its stack holds. Raise it to 1000 for
    /// the pin's limit on a thread with the stack for it (about 15
    /// MiB in release, 260 MiB in debug; the CLI does). Only
    /// recursive triggers (`PRAGMA recursive_triggers`) nest deeper
    /// than the schema's number of triggers. Foreign-key action
    /// programs (`foreign_keys`: a cascade through a chain of rows)
    /// count as trigger programs, as on the pin.
    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
    pub max_trigger_depth: usize,
}

/// The `VACUUM INTO` writer's type (v0.16, `v16`; see
/// `Config::vacuum_into`): file name, image → written.
#[cfg(feature = "v16")]
pub type VacuumWriter = fn(&[u8], &[u8]) -> bool;

impl Default for Config {
    /// Hosted defaults (§5.8's example values).
    fn default() -> Config {
        Config {
            cache_pages: 16,
            create_page_size: 4096,
            stmt_init: 16 * 1024,
            stmt_max: 64 * 1024,
            txn_init: 8 * 1024,
            txn_max: 32 * 1024,
            catalog_init: 16 * 1024,
            catalog_max: 256 * 1024,
            nonce_seed: 0x6869_7070_5eed_0001,
            #[cfg(feature = "v04")]
            clock: None,
            #[cfg(all(feature = "v16", feature = "std"))]
            vacuum_into: Some(crate::storage::write_new_file),
            #[cfg(all(feature = "v16", not(feature = "std")))]
            vacuum_into: None,
            #[cfg(feature = "v16")]
            max_nesting: 250,
            #[cfg(feature = "v04")]
            max_expr_depth: DEFAULT_EXPR_DEPTH,
            #[cfg(feature = "v04")]
            max_parser_depth: DEFAULT_PARSER_DEPTH,
            #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
            max_trigger_depth: DEFAULT_TRIGGER_DEPTH,
        }
    }
}

/// `Config::default()`'s expression depth limit (v0.16): the pin's.
#[cfg(feature = "v04")]
pub const DEFAULT_EXPR_DEPTH: usize = crate::sql::parser::PIN_EXPR_DEPTH;
/// `Config::default()`'s parser depth limit (v0.16): a fifth of the
/// pin's 2500, so that the default limits fit a 2 MiB thread in a
/// release build (about 490 nested parentheses, 160 nested calls, 80
/// nested FROM subqueries).
#[cfg(feature = "v04")]
pub const DEFAULT_PARSER_DEPTH: usize = 500;

/// `Config::default()`'s trigger nesting limit (v0.16, `triggers` /
/// `foreign_keys`; see `Config::max_trigger_depth`).
#[cfg(any(feature = "triggers", feature = "foreign_keys"))]
pub const DEFAULT_TRIGGER_DEPTH: usize = 24;
/// The most stack one level of trigger (or foreign-key action)
/// program nesting costs (v0.16; measured on x86_64 over recursive
/// AFTER INSERT triggers with simple bodies, `FileStorage`).
#[cfg(all(any(feature = "triggers", feature = "foreign_keys"), debug_assertions))]
pub const STACK_PER_TRIGGER: usize = 272 * 1024;
#[cfg(all(
    any(feature = "triggers", feature = "foreign_keys"),
    not(debug_assertions)
))]
pub const STACK_PER_TRIGGER: usize = 16 * 1024;

/// The stack hipp needs for any statement, before nesting (v0.16;
/// measured on x86_64 — the step future and the call path to the
/// parser and executor).
#[cfg(all(feature = "v04", debug_assertions))]
pub const STACK_BASE: usize = 320 * 1024;
#[cfg(all(feature = "v04", not(debug_assertions)))]
pub const STACK_BASE: usize = 64 * 1024;
/// The most stack one symbol of `Config::max_parser_depth` costs
/// (v0.16; measured on x86_64, the worst construct being a nested
/// FROM subquery: its parse, plan and execution).
#[cfg(all(feature = "v04", debug_assertions))]
pub const STACK_PER_SYMBOL: usize = 4096;
#[cfg(all(feature = "v04", not(debug_assertions)))]
pub const STACK_PER_SYMBOL: usize = 1536;
/// The most stack one level of `Config::max_expr_depth` costs
/// (v0.16; measured on x86_64: the resolver, the evaluator and the
/// tree's Clone / Drop, each recursing once per level).
#[cfg(all(feature = "v04", debug_assertions))]
pub const STACK_PER_HEIGHT: usize = 3456;
#[cfg(all(feature = "v04", not(debug_assertions)))]
pub const STACK_PER_HEIGHT: usize = 704;

impl Config {
    /// The §4.1 tiny reference configuration: 16 KiB page pool,
    /// statement/txn/catalog maxima of 16/8/16 KiB — 56 KiB of
    /// arena+pool budget inside the 64 KiB engine-RAM gate.
    pub fn tiny() -> Config {
        Config {
            cache_pages: 4,
            create_page_size: 4096,
            stmt_init: 4 * 1024,
            stmt_max: 16 * 1024,
            txn_init: 2 * 1024,
            txn_max: 8 * 1024,
            catalog_init: 4 * 1024,
            catalog_max: 16 * 1024,
            nonce_seed: 0x6869_7070_5eed_0001,
            #[cfg(feature = "v04")]
            clock: None,
            #[cfg(feature = "v16")]
            vacuum_into: None,
            #[cfg(feature = "v16")]
            max_nesting: 16,
            #[cfg(feature = "v04")]
            max_expr_depth: 64,
            #[cfg(feature = "v04")]
            max_parser_depth: 64,
            #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
            max_trigger_depth: 8,
        }
    }

    pub fn cache_pages(mut self, n: usize) -> Config {
        self.cache_pages = n;
        self
    }
    pub fn statement_arena(mut self, init: usize, max: usize) -> Config {
        self.stmt_init = init;
        self.stmt_max = max;
        self
    }
    pub fn txn_arena(mut self, init: usize, max: usize) -> Config {
        self.txn_init = init;
        self.txn_max = max;
        self
    }
    pub fn catalog_arena(mut self, init: usize, max: usize) -> Config {
        self.catalog_init = init;
        self.catalog_max = max;
        self
    }
    pub fn nonce_seed(mut self, seed: u64) -> Config {
        self.nonce_seed = seed;
        self
    }
    /// Set `max_nesting` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub fn max_nesting(mut self, levels: usize) -> Config {
        self.max_nesting = levels;
        self
    }
    /// Set `max_trigger_depth` (v0.16, `triggers` / `foreign_keys`).
    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
    pub fn max_trigger_depth(mut self, levels: usize) -> Config {
        self.max_trigger_depth = levels;
        self
    }
    /// Set `max_expr_depth` and `max_parser_depth` (v0.16).
    #[cfg(feature = "v04")]
    pub fn depth_limits(mut self, expr: usize, parser: usize) -> Config {
        self.max_expr_depth = expr;
        self.max_parser_depth = parser;
        self
    }
    /// Set `max_expr_depth` and `max_parser_depth` for a thread of
    /// `bytes` of stack (v0.16): the largest limits, at most the
    /// pin's, whose worst case — `STACK_BASE + max_parser_depth *
    /// STACK_PER_SYMBOL + max_expr_depth * STACK_PER_HEIGHT`, the
    /// costs measured for this build profile — fits. Up to half the
    /// room past the base goes to the expression depth, the rest to
    /// the parser's. A stack below the base leaves both 0: nothing
    /// but the statements without expressions parses then. It
    /// budgets a statement's own text: lower `max_nesting` too when
    /// statements read views or CTEs on a small stack. With
    /// triggers or foreign keys it also lowers `max_trigger_depth`
    /// to the trigger levels a quarter of the room holds
    /// (`STACK_PER_TRIGGER` each; v0.16 review fix), never raising it.
    #[cfg(feature = "v04")]
    pub fn stack_budget(self, bytes: usize) -> Config {
        let room = bytes.saturating_sub(STACK_BASE);
        let expr = (room / 2 / STACK_PER_HEIGHT).min(crate::sql::parser::PIN_EXPR_DEPTH);
        let parser = (room.saturating_sub(expr * STACK_PER_HEIGHT) / STACK_PER_SYMBOL)
            .min(crate::sql::parser::PIN_PARSER_DEPTH);
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        let c = {
            let levels = room / 4 / STACK_PER_TRIGGER;
            let d = self.max_trigger_depth.min(levels);
            self.max_trigger_depth(d)
        };
        #[cfg(not(any(feature = "triggers", feature = "foreign_keys")))]
        let c = self;
        c.depth_limits(expr, parser)
    }
    /// The parser's nesting limits under this configuration
    /// (v0.16; the pin's constants without the v0.4 grammar, which
    /// does not nest).
    pub fn limits(&self) -> crate::sql::parser::Limits {
        crate::sql::parser::Limits {
            #[cfg(feature = "v04")]
            expr_depth: self.max_expr_depth,
            #[cfg(feature = "v04")]
            parser_depth: self.max_parser_depth,
        }
    }
    pub fn page_size(mut self, n: u32) -> Config {
        self.create_page_size = n;
        self
    }
    /// Supply the wall clock (v0.16): `now` returns the current
    /// Unix time in milliseconds (see the `clock` field).
    #[cfg(feature = "v04")]
    pub fn clock(mut self, now: fn() -> i64) -> Config {
        self.clock = Some(now);
        self
    }
    /// Supply the `VACUUM INTO` writer (v0.16, `v16`; see the
    /// `vacuum_into` field).
    #[cfg(feature = "v16")]
    pub fn vacuum_into(mut self, write: VacuumWriter) -> Config {
        self.vacuum_into = Some(write);
        self
    }
}

/// Result of one `Statement::step()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A row is available via `column*`.
    Row,
    /// The statement is finished.
    Done,
}

/// Access path the planner chose for a scan (v0.2 rowid paths;
/// v0.3 adds the secondary-index path). This is the executor seam
/// for general WHERE: richer predicates lower to these same paths
/// in later phases.
#[derive(Debug, Clone)]
enum Access {
    /// No usable predicate: full B-tree scan.
    FullScan,
    /// `WHERE rowid = <v>`: point lookup via `btree::seek_rowid`.
    Rowid(i64),
    /// `WHERE <indexed col> = <v>` (v0.3): secondary-index lookup
    /// on the index's first key column; `probe` is the literal
    /// after the column's affinity has been applied.
    #[cfg(feature = "indexes")]
    Index {
        root: u32,
        desc: AVec<crate::record::KeyOrder>,
        probe: OwnedValue,
    },
    /// A predicate that can never match (e.g. `rowid = NULL`,
    /// `rowid = 'abc'`): the scan yields no rows.
    Empty,
}

/// One index the INSERT executor must maintain (v0.3), resolved
/// from the catalog at plan time.
#[cfg(feature = "indexes")]
#[derive(Debug, Clone)]
struct IndexPlan {
    root: u32,
    unique: bool,
    /// Key columns (canonical form: None = rowid).
    cols: AVec<IdxColPlan>,
    /// Per-key-column direction and collation (v0.16), parallel
    /// to `cols`.
    desc: AVec<crate::record::KeyOrder>,
}

#[cfg(feature = "indexes")]
#[derive(Debug, Clone, Copy)]
struct IdxColPlan {
    col: Option<usize>,
    desc: bool,
    /// The key column's collating sequence (v0.16).
    coll: crate::record::Coll,
}

#[cfg(feature = "indexes")]
impl IdxColPlan {
    fn order(&self) -> crate::record::KeyOrder {
        crate::record::KeyOrder {
            desc: self.desc,
            coll: self.coll,
        }
    }
}

// Plans are built once per statement and never stored in bulk;
// the variant-size spread depends on the feature rung (v04 alone
// has the widest gap), so boxing would only add an indirection.
#[allow(clippy::large_enum_variant)]
enum Plan {
    CreateTable,
    /// `CREATE TABLE ... AS SELECT` (v0.16, `v16`): the source
    /// plan, the derived `sqlite_schema` text (the pin's
    /// `createTableStmt` form) and the new columns' affinities.
    /// `select: None` is the IF NOT EXISTS no-op: the name was
    /// taken when the statement was planned, so (as the pin's
    /// `sqlite3StartTable`) the SELECT was never resolved.
    #[cfg(feature = "v16")]
    CreateTableAs {
        select: Option<Box<crate::query::SubPlan>>,
        sql: AVec<u8>,
        affs: AVec<crate::record::Affinity>,
    },
    /// `CREATE [UNIQUE] INDEX` (v0.3): everything population needs
    /// is resolved at plan time.
    #[cfg(feature = "indexes")]
    CreateIndex {
        table_root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: AVec<OwnedValue>,
        cols: AVec<IdxColPlan>,
        unique: bool,
        /// The table's generated columns (v0.16): an index may
        /// key on a VIRTUAL column, computed per row here.
        #[cfg(feature = "v04")]
        gcols: Option<Box<crate::rules::GenCols>>,
    },
    /// `DROP INDEX` (v0.3); `root: None` is the IF EXISTS no-op.
    #[cfg(feature = "indexes")]
    DropIndex {
        root: Option<u32>,
    },
    Insert {
        root: u32,
        rowid_alias: Option<usize>,
        /// Per-table-column plan data (affinity + default).
        cols: AVec<InsCol>,
        /// Where each supplied value goes, parallel to one AST
        /// value row (every row of a multi-row VALUES shares it).
        targets: AVec<Target>,
        /// VALUES rows evaluated at plan time (v0.4: expressions
        /// in VALUES are constant-folded here; pre-v0.4 they are
        /// plain literals).
        rows_v: AVec<AVec<OwnedValue>>,
        /// VALUES rows that contain subqueries (v0.8), resolved
        /// at plan time and evaluated at step time — all of them
        /// BEFORE the first row is inserted (the oracle evaluates
        /// a multi-row VALUES' subqueries against the
        /// pre-statement table: `VALUES ((SELECT count(*) FROM
        /// t)), ((SELECT count(*) FROM t))` on an empty `t`
        /// inserts 0, 0 — probed on the pin). `rows_v` is empty
        /// when this is `Some`.
        #[cfg(feature = "v08")]
        rows_r: Option<AVec<AVec<RExpr>>>,
        /// `INSERT INTO ... SELECT` source (v0.8): a subquery
        /// plan against the empty enclosing scope; executed and
        /// fully materialised at step time before the first row
        /// is inserted, so a self-insert
        /// (`INSERT INTO t SELECT * FROM t`) terminates with the
        /// pre-statement rows, as in the oracle. Boxed: the
        /// plan is large and the enum's other variants are not
        /// (clippy::large_enum_variant).
        #[cfg(feature = "v08")]
        select: Option<Box<crate::query::SubPlan>>,
        /// The `OR ...` conflict clause (v0.7; the parser only
        /// produces a non-default value under the feature, and
        /// the executor only consults it there).
        conflict: ConflictAction,
        /// The planned upsert clause (v0.7).
        #[cfg(feature = "v07")]
        upsert: Option<UpsertPlan>,
        /// `RETURNING` expressions resolved against the target
        /// table (v0.7); empty when the clause is absent.
        #[cfg(feature = "v07")]
        returning: AVec<RExpr>,
        /// Indexes to maintain, in catalog order (v0.3).
        #[cfg(feature = "indexes")]
        indexes: AVec<IndexPlan>,
        /// The table's CHECK / generated / STRICT rules (v0.16);
        /// `None` when it has none.
        #[cfg(feature = "v04")]
        rules: Option<Box<crate::rules::TableRules>>,
        /// The table's name when it is AUTOINCREMENT (v0.16,
        /// `v16`): the `sqlite_sequence` row to keep.
        #[cfg(feature = "v16")]
        autoinc: Option<AVec<u8>>,
    },
    Scan {
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        out: AVec<usize>,
        /// Affinity per output column (read-side forcing, §5.5:
        /// a REAL column may store integral values with an integer
        /// serial type; fetching must yield a real).
        aff: AVec<crate::record::Affinity>,
        defaults: AVec<OwnedValue>,
        access: Access,
    },
    /// `BEGIN` (v0.2): state is validated again at step time.
    Begin,
    /// `BEGIN IMMEDIATE | EXCLUSIVE` (v0.16, `v16`): a `Begin`
    /// that also fixes an empty database's layout.
    #[cfg(feature = "v16")]
    BeginImmediate,
    /// `COMMIT` (v0.2).
    Commit,
    /// `ROLLBACK` (v0.2).
    Rollback,
    /// A v0.4 query (joins / aggregates / general expressions /
    /// ORDER BY / LIMIT): planned resolved expressions, executed
    /// by materialisation on first step.
    #[cfg(feature = "v04")]
    Query(QueryPlan),
    /// `UPDATE` (v0.5): SET expressions and the filter are
    /// resolved at plan time against the single source; rows are
    /// materialised at step time and each assignment evaluates
    /// against the row's original values.
    #[cfg(feature = "v05")]
    Update {
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: AVec<OwnedValue>,
        affs: AVec<crate::record::Affinity>,
        /// Per-column `NOT NULL` flags (v0.10), parallel to
        /// `defaults` / `affs`.
        not_null: AVec<bool>,
        /// (target, expression) pairs in statement order.
        sets: AVec<(Target, RExpr)>,
        filter: Option<RExpr>,
        /// The `OR ...` conflict clause (v0.7; before it, only
        /// the IGNORE widening existed, as `or_ignore`).
        conflict: ConflictAction,
        /// `FROM` sources (v0.7), materialised at step time and
        /// nested-loop joined onto each target row; empty when
        /// the clause is absent. The SET/filter expressions were
        /// resolved against target ++ these sources.
        #[cfg(feature = "v07")]
        from_sources: AVec<SourcePlan>,
        /// `RETURNING` expressions resolved against the target
        /// table only (v0.7); empty when the clause is absent.
        #[cfg(feature = "v07")]
        returning: AVec<RExpr>,
        /// Indexes to maintain, in catalog order.
        #[cfg(feature = "indexes")]
        indexes: AVec<IndexPlan>,
        /// The table's rules (v0.16), as for INSERT.
        rules: Option<Box<crate::rules::TableRules>>,
    },
    /// `DELETE` (v0.5).
    #[cfg(feature = "v05")]
    Delete {
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: AVec<OwnedValue>,
        affs: AVec<crate::record::Affinity>,
        filter: Option<RExpr>,
        /// `RETURNING` expressions resolved against the target
        /// table (v0.7); they read each row's pre-delete values.
        #[cfg(feature = "v07")]
        returning: AVec<RExpr>,
        /// Indexes to maintain, in catalog order.
        #[cfg(feature = "indexes")]
        indexes: AVec<IndexPlan>,
        /// The table's rules (v0.16): DELETE needs only its
        /// generated columns (VIRTUAL values for the filter,
        /// RETURNING and index keys).
        rules: Option<Box<crate::rules::TableRules>>,
    },
    /// `DROP TABLE` (v0.5); `root: None` is the IF EXISTS no-op.
    #[cfg(feature = "v05")]
    DropTable {
        root: Option<u32>,
        /// Roots of the table's indexes (explicit + implicit),
        /// in catalog order; their pages are freed with it.
        index_roots: AVec<u32>,
        /// An AUTOINCREMENT table's `sqlite_sequence` (root, the
        /// table's name): its rows of that name go with it (v0.16
        /// fix, as the pin's `DELETE FROM sqlite_sequence WHERE
        /// name = ...`).
        seq: Option<(u32, AVec<u8>)>,
    },
    /// A statement routed to the TEMP database (v0.16, `v16`;
    /// `session::temp`): its text there and its result width.
    #[cfg(feature = "v16")]
    Temp {
        sql: AVec<u8>,
        width: usize,
    },
    /// `ALTER TABLE ... RENAME TO / RENAME COLUMN / DROP COLUMN`
    /// (v0.16, `v16`; `session::alter`).
    #[cfg(feature = "v16")]
    Alter16 {
        table: AVec<u8>,
        op: alter::AlterOp,
    },
    /// `ALTER TABLE ... ADD COLUMN` (v0.4): the rewritten stored
    /// CREATE TABLE text is computed at plan time.
    #[cfg(feature = "v04")]
    AlterAdd {
        root: u32,
        table: AVec<u8>,
        new_sql: AVec<u8>,
        /// Existing rows to verify against the new definition
        /// (v0.16; see the planner).
        verify: Option<Box<AlterVerify>>,
        /// The column may be added only while the table has no
        /// rows (v0.16, the pin's `sqlite3ErrorIfNotEmpty`): a
        /// non-constant default, NOT NULL without a default, a
        /// STORED generated column; with `PRAGMA foreign_keys` ON a
        /// REFERENCES column with a non-NULL default. The error the
        /// step raises when the table has rows (the first refusal
        /// the pin emits).
        empty_only: Option<SchemaKind>,
    },
    /// `CREATE VIEW` (v0.4).
    #[cfg(feature = "v04")]
    CreateView,
    /// A statement with parameters whose plan for its current
    /// bindings is not made yet (v0.16, `params`): its plan for
    /// the unbound (all-NULL) state failed at prepare with an
    /// error a binding may change. `step` plans it first.
    #[cfg(feature = "params")]
    Deferred,
    /// `DROP VIEW` (v0.4); `exists: false` is the IF EXISTS no-op.
    #[cfg(feature = "v04")]
    DropView {
        exists: bool,
    },
    /// `PRAGMA` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Pragma(Box<pragma::PragmaPlan>),
    /// The savepoint and maintenance statements (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Maint(maint::Maint),
    /// `CREATE TRIGGER` (v0.16, `triggers`): the span of the
    /// statement text the stored text keeps (from the bare name to
    /// END), the name and the table; `skip` is the IF NOT EXISTS
    /// no-op.
    #[cfg(feature = "triggers")]
    CreateTrigger {
        span: (usize, usize),
        name: AVec<u8>,
        table: AVec<u8>,
        skip: bool,
    },
    /// `DROP TRIGGER` (v0.16, `triggers`); `None` is the IF EXISTS
    /// no-op.
    #[cfg(feature = "triggers")]
    DropTrigger {
        name: Option<AVec<u8>>,
    },
    /// A write of a view through its INSTEAD OF triggers (v0.16,
    /// `triggers`).
    #[cfg(feature = "triggers")]
    Instead(Box<trigger::InsteadPlan>),
}

/// One FROM source in a v0.4 query plan: the subquery
/// planner's `SubSource` (v0.16 merged the two identical
/// structs, so every join executor serves both plan kinds). A
/// derived source (v0.9) carries its subplan in `derived`,
/// executed once against the empty outer row at
/// materialisation time (`root` unused).
#[cfg(feature = "v04")]
type SourcePlan = crate::query::SubSource;

/// A v0.4 query plan: everything resolved at prepare time.
#[cfg(feature = "v04")]
#[derive(Debug, Clone)]
struct QueryPlan {
    sources: AVec<SourcePlan>,
    items: AVec<RExpr>,
    filter: Option<RExpr>,
    group_by: AVec<RExpr>,
    having: Option<RExpr>,
    order_by: AVec<(RExpr, crate::sql::SortOrder)>,
    distinct: bool,
    /// LIMIT / OFFSET (v0.16: resolved expressions; see
    /// `SubPlan::limit`).
    limit: Option<RExpr>,
    offset: Option<RExpr>,
    has_agg: bool,
    width: usize,
    /// Compound tail (v0.9; see `SubPlan::compound`). When
    /// non-empty, `order_by` is empty, `comp_order` carries
    /// the compound ORDER BY, and `limit` / `offset` apply to
    /// the combined result. Only filled under `v09`.
    #[cfg_attr(not(feature = "v09"), allow(dead_code))]
    compound: AVec<(crate::sql::CompoundOp, Box<QueryPlan>)>,
    /// A VALUES core's resolved rows (v0.9; see
    /// `SubPlan::values`).
    #[cfg_attr(not(feature = "v09"), allow(dead_code))]
    values: AVec<AVec<RExpr>>,
    /// Compound ORDER BY (v0.9; see `SubPlan::comp_order`).
    #[cfg_attr(not(feature = "v09"), allow(dead_code))]
    comp_order: AVec<(usize, crate::sql::SortOrder)>,
}

/// One compound member core's ORDER BY matching data (v0.16;
/// replaces v0.9's per-item `(qualifier, name)` keys): each
/// item's `AS` name (a VALUES core's `column1`, ...), its
/// resolved expression, and the core's own FROM scope. The
/// pin's `resolveCompoundOrderBy` tries each core left to
/// right: a bare name equal to an `AS` name matches that item;
/// otherwise the term resolves in the core's FROM scope alone
/// (no enclosing scopes, failures skip the core) and matches
/// the first item that is the same reference — so after
/// `a RIGHT JOIN b USING (id)` the term `id` is `b.id`, after a
/// FULL join `coalesce(a.id, b.id)`.
#[cfg(feature = "v04")]
#[cfg_attr(not(feature = "v09"), allow(dead_code))]
struct CompKeys {
    names: AVec<Option<AVec<u8>>>,
    items: AVec<RExpr>,
    scope: crate::query::ScopeGroup,
    /// Each result column's collation, if it has one (v0.16;
    /// a VALUES core's first row): the compound's column
    /// collation is the leftmost core's that has one.
    colls: AVec<Option<crate::record::Coll>>,
}

#[cfg(feature = "v09")]
impl CompKeys {
    /// The item term `table.name` (or bare `name`) matches in
    /// this core, if any.
    fn hit(&self, table: Option<&[u8]>, name: &[u8]) -> Option<usize> {
        if table.is_none()
            && let Some(i) = self
                .names
                .iter()
                .position(|n| n.as_ref().is_some_and(|n| eq_ci(n.as_slice(), name)))
        {
            return Some(i);
        }
        let r = crate::query::resolve_column_in_group::<()>(&self.scope, table, name)?.ok()?;
        self.items.iter().position(|it| same_ref(it, &r))
    }
}

/// Whether two resolved column references name the same thing
/// (v0.16, compound ORDER BY): the same slot, the same rowid
/// source, or the same function over such references (a FULL
/// join's USING `coalesce`).
#[cfg(feature = "v09")]
fn same_ref(a: &RExpr, b: &RExpr) -> bool {
    match (a, b) {
        (RExpr::Column(x, ..), RExpr::Column(y, ..)) => x == y,
        (RExpr::Rowid(x), RExpr::Rowid(y)) => x == y,
        (RExpr::Scalar(f, xs), RExpr::Scalar(g, ys)) => {
            f == g && xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| same_ref(x, y))
        }
        _ => false,
    }
}

/// The common planned body of one SELECT core (v0.16; see
/// `Connection::plan_core`): what `QueryPlan` and `SubPlan`
/// both carry.
#[cfg(feature = "v04")]
struct CorePlan {
    sources: AVec<SourcePlan>,
    items: AVec<RExpr>,
    filter: Option<RExpr>,
    group_by: AVec<RExpr>,
    having: Option<RExpr>,
    order_by: AVec<(RExpr, crate::sql::SortOrder)>,
    limit: Option<RExpr>,
    offset: Option<RExpr>,
    has_agg: bool,
    /// Result names / affinities: what a subquery plan records
    /// (top-level queries do not keep them).
    #[cfg_attr(not(feature = "v06"), allow(dead_code))]
    result_names: AVec<AVec<u8>>,
    #[cfg_attr(not(feature = "v06"), allow(dead_code))]
    result_affs: AVec<Option<crate::record::Affinity>>,
    #[cfg_attr(not(feature = "v06"), allow(dead_code))]
    result_named: AVec<bool>,
    /// Compound ORDER BY matching data for this core.
    keys: CompKeys,
}

/// Which clause a GROUP BY / HAVING / ORDER BY term belongs to
/// (v0.16): the pin's ordinal and alias rules differ per
/// clause (see `Connection::resolve_term`).
#[cfg(feature = "v04")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermClause {
    Order,
    Group,
    Having,
}

/// `a AND b`, or `b` alone when there is no `a` (v0.16: join
/// constraint and hoisted ON-term conjunctions).
#[cfg(feature = "v04")]
fn and_expr(a: Option<RExpr>, b: RExpr) -> RExpr {
    match a {
        None => b,
        Some(a) => RExpr::Binary(BinOp::And, Box::new(a), Box::new(b)),
    }
}

/// Resolve every joined source's USING / NATURAL columns
/// (v0.16, the pin's `sqlite3ProcessJoin`). For source `i`:
/// a NATURAL join's columns are its own columns (in its column
/// order) that some source on its left also has; a USING
/// column must exist in source `i` and in some source on its
/// left ("cannot join using column ... not present in both
/// tables" otherwise). Each column contributes `left = right`
/// to the source's join condition, where `left` is the
/// left-most source having the column — or, once the FROM
/// clause has a RIGHT / FULL join, `coalesce(...)` over every
/// left source having it, each of which (after the first) must
/// itself have it as a USING column ("ambiguous reference ...
/// in USING()" otherwise). Records the columns in the scope
/// (`SourceDesc::using`) for star expansion and unqualified
/// name resolution, and returns the per-source conditions.
#[cfg(feature = "v04")]
fn plan_using<E>(
    from: &[crate::sql::FromSource],
    arena: &Arena,
    cur: &mut crate::query::ScopeGroup,
) -> Result<AVec<Option<RExpr>>, E> {
    let any_right = from.iter().any(|s| s.kind.pads_left());
    let mut conds = AVec::new();
    for (i, s) in from.iter().enumerate() {
        let mut names: AVec<AVec<u8>> = AVec::new();
        let mut cond: Option<RExpr> = None;
        {
            let (left, right) = cur.sources.as_slice().split_at(i.min(cur.sources.len()));
            let Some(src) = right.first() else {
                break;
            };
            if s.natural {
                for (c, _) in src.columns.iter() {
                    if left.iter().any(|l| l.column_index(c.as_slice()).is_some()) {
                        names.push(c.clone());
                    }
                }
            } else {
                for u in s.using.iter() {
                    names.push(copy_vec(handle_bytes(arena, *u)));
                }
            }
            let missing = Error::Schema {
                kind: SchemaKind::UsingColumnMissing,
            };
            for n in names.iter() {
                let n = n.as_slice();
                let Some(rc) = src.column_index(n) else {
                    return Err(missing);
                };
                let rhs = src.column_ref(cur, i, rc);
                let mut sides = left
                    .iter()
                    .enumerate()
                    .filter_map(|(li, l)| l.column_index(n).map(|ci| (li, l, ci)));
                let Some((li, l, ci)) = sides.next() else {
                    return Err(missing);
                };
                let mut lhs = l.column_ref(cur, li, ci);
                if any_right {
                    let mut all: AVec<RExpr> = AVec::new();
                    for (li, l, ci) in sides {
                        if !l.uses(n) {
                            return Err(Error::Schema {
                                kind: SchemaKind::AmbiguousColumn,
                            });
                        }
                        if all.is_empty() {
                            all.push(lhs.clone());
                        }
                        all.push(l.column_ref(cur, li, ci));
                    }
                    if !all.is_empty() {
                        lhs = RExpr::Scalar(crate::query::ScalarFn::Coalesce, all);
                    }
                }
                cond = Some(and_expr(
                    cond,
                    RExpr::Binary(BinOp::Eq, Box::new(lhs), Box::new(rhs)),
                ));
            }
        }
        if let Some(src) = cur.sources.as_mut_slice().get_mut(i) {
            src.using = names;
        }
        conds.push(cond);
    }
    Ok(conds)
}

/// ALTER TABLE ADD COLUMN's existing-row verification (v0.16):
/// the new definition's rules and its row layout.
#[cfg(feature = "v04")]
#[derive(Debug, Clone)]
struct AlterVerify {
    rules: crate::rules::TableRules,
    defaults: AVec<OwnedValue>,
    affs: AVec<crate::record::Affinity>,
    not_null: AVec<bool>,
    rowid_alias: Option<usize>,
}

struct InsCol {
    affinity: crate::record::Affinity,
    default: OwnedValue,
    /// `NOT NULL` constraint on the column (v0.10).
    not_null: bool,
    /// A DEFAULT that reads connection state (`random()`,
    /// `last_insert_rowid()`, ...; v0.16 review fix, `v16`): its
    /// resolved expression and clock row, evaluated again for
    /// every row inserted — on the pin each row's DEFAULT sees the
    /// rowid the row before it inserted, and draws its own random
    /// value. `default` keeps the prepare-time value (which is
    /// where such a default's errors surface).
    #[cfg(feature = "v16")]
    per_row: Option<(RExpr, RowData)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Col(usize),
    Rowid,
}

/// A planned subquery compound tail plus its resolved
/// compound ORDER BY (v0.9).
#[cfg(feature = "v06")]
type SubCompoundParts = (
    AVec<(crate::sql::CompoundOp, Box<crate::query::SubPlan>)>,
    AVec<(usize, crate::sql::SortOrder)>,
);

/// A planned top-level compound tail plus its resolved
/// compound ORDER BY (v0.9).
#[cfg(feature = "v04")]
type QueryCompoundParts = (
    AVec<(crate::sql::CompoundOp, Box<QueryPlan>)>,
    AVec<(usize, crate::sql::SortOrder)>,
);

/// A planned `ON CONFLICT` upsert clause (v0.7). The SET
/// expressions and the filter are resolved against the combined
/// scope (original row's scope group first, then an `excluded`
/// group holding the proposed row's values under the qualifier
/// `excluded`), so bare column names bind the original row —
/// SQLite's rule — and `excluded.<col>` reaches the proposal.
#[cfg(feature = "v07")]
#[derive(Debug, Clone)]
struct UpsertPlan {
    /// Arbitration target as resolved column targets: `None`
    /// arbitrates any uniqueness conflict; `Some(cols)` only a
    /// conflict on the rowid (cols == [Rowid]) or on a unique
    /// index whose key columns are exactly this set.
    target: Option<AVec<Target>>,
    /// Per-target-column explicit collations (v0.16), parallel
    /// to `target`: such a column arbitrates only an index key
    /// column of that collation.
    target_colls: AVec<Option<crate::record::Coll>>,
    /// `DO UPDATE` when true; `DO NOTHING` when false (then
    /// `sets`/`filter` are empty / None).
    do_update: bool,
    sets: AVec<(Target, RExpr)>,
    filter: Option<RExpr>,
}

/// Whether a SELECT fits the pre-v0.4 fast paths: one table,
/// `*` (alone) or plain unqualified columns, and no filter or a
/// `WHERE <col> = <literal>` filter.
fn select_is_simple(stmt: &SelectStmt) -> bool {
    if stmt.distinct
        || stmt.from.len() != 1
        || !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || !stmt.compound.is_empty()
        || !stmt.values.is_empty()
        || stmt.from.as_slice()[0].derived.is_some()
    {
        return false;
    }
    if stmt.from.as_slice()[0].on.is_some() {
        return false;
    }
    // v0.16: `name(args)` is a table-valued function (or the
    // pin's "not a function"), for the general planner.
    #[cfg(feature = "v16")]
    if stmt.from.as_slice()[0].args.is_some() {
        return false;
    }
    let mut stars = 0usize;
    for it in stmt.items.iter() {
        match it {
            SelectItem::Star => stars += 1,
            SelectItem::Expr(Expr::Column { table: None, .. }, None, _) => {}
            _ => return false,
        }
    }
    if stars > 0 && stmt.items.len() != 1 {
        return false;
    }
    match &stmt.filter {
        None => true,
        Some(Expr::Binary(BinOp::Eq, l, r)) => {
            matches!(l.as_ref(), Expr::Column { table: None, .. })
                && matches!(r.as_ref(), Expr::Literal(_))
        }
        _ => false,
    }
}

pub struct Connection<S: Storage> {
    pager: Pager<S>,
    catalog_arena: Arena,
    catalog: Catalog,
    cfg: Config,
    /// An explicit `BEGIN` is open (v0.2). The pager transaction
    /// itself opens lazily at the first write, as in SQLite; while
    /// this flag is set, write statements do not autocommit.
    explicit_txn: bool,
    /// Cumulative subquery-plan executions on this connection
    /// (v0.13): one per `exec_subplan` call, including derived
    /// tables and INSERT ... SELECT sources. A diagnostic /
    /// test observable for the hoisting win — an uncorrelated
    /// expression subquery over N outer rows contributes 1,
    /// not N.
    #[cfg(feature = "v06")]
    sub_execs: usize,
    /// SubPlan node identity source (v0.13): every planned
    /// subquery node takes a unique id, which the executor's
    /// hoisting cache keys on. Ids only need uniqueness within
    /// one connection's statements, so this is a per-connection
    /// counter (a `Cell`, bumped from `&self` planning) rather
    /// than a global atomic: no engine statics, and no atomic
    /// read-modify-write, which targets such as Cortex-M0
    /// (`thumbv6m`) do not have.
    #[cfg(feature = "v06")]
    next_subplan_id: core::cell::Cell<usize>,
    /// The recursive CTEs whose recursive SELECTs are being
    /// planned (v0.16, `v16`): each one's id and column shape,
    /// which its recursive reference takes (`plan_rec_ref`). A
    /// `Cell` taken and put back around each use, for `&self`
    /// planning.
    #[cfg(feature = "v16")]
    rec_scope: core::cell::Cell<AVec<cte::RecShape>>,
    /// The connection state the evaluator reads (v0.16): the
    /// pin's last-insert rowid, change counters and random
    /// stream (`query::Env`; empty without `v16`).
    #[cfg(feature = "v04")]
    env: crate::query::Env,
    /// The PRAGMA flags (v0.16, `v16`).
    #[cfg(feature = "v16")]
    prag: pragma::PragState,
    /// The reference trace of a running ALTER TABLE (v0.16, `v16`;
    /// `query::AlterTrace`): set while it plans the schema's views.
    #[cfg(feature = "v16")]
    alter_trace: Option<alloc::rc::Rc<core::cell::RefCell<crate::query::AlterTrace>>>,
    /// The connection's TEMP database (v0.16, `v16`; `session::temp`),
    /// created on first use, never written to the file.
    #[cfg(feature = "v16")]
    temp: Option<Box<Connection<crate::storage::MemStorage>>>,
    /// This connection is another's TEMP database (v0.16, `v16`).
    #[cfg(feature = "v16")]
    is_temp: bool,
    /// The running trigger programs (v0.16, `triggers`).
    #[cfg(feature = "triggers")]
    trig: trigger::TrigState,
    /// The foreign-key violation counters and action programs
    /// (v0.16, `foreign_keys`).
    #[cfg(feature = "foreign_keys")]
    fk: fkey::FkState,
}

impl<S: Storage> Connection<S> {
    /// Open (creating if empty) a database on `storage`, recovering
    /// any hot journal, and build the schema catalog.
    pub async fn open(storage: S, cfg: Config) -> Result<Connection<S>, S::Error> {
        let txn_arena = Arena::new(Scope::Transaction, cfg.txn_init, cfg.txn_max);
        let (pager, _is_new) = Pager::open(
            storage,
            cfg.cache_pages,
            cfg.create_page_size,
            cfg.nonce_seed,
            txn_arena,
        )
        .await?;
        let catalog_arena = Arena::new(Scope::Catalog, cfg.catalog_init, cfg.catalog_max);
        #[cfg(feature = "v04")]
        let seed = cfg.nonce_seed;
        let mut conn = Connection {
            pager,
            catalog_arena,
            catalog: Catalog::empty(u32::MAX),
            cfg,
            explicit_txn: false,
            #[cfg(feature = "v06")]
            sub_execs: 0,
            #[cfg(feature = "v06")]
            next_subplan_id: core::cell::Cell::new(1),
            #[cfg(feature = "v16")]
            rec_scope: core::cell::Cell::new(AVec::new()),
            #[cfg(feature = "v04")]
            env: crate::query::Env::new(seed),
            #[cfg(feature = "v16")]
            prag: pragma::PragState::default(),
            #[cfg(feature = "v16")]
            alter_trace: None,
            #[cfg(feature = "v16")]
            temp: None,
            #[cfg(feature = "v16")]
            is_temp: false,
            #[cfg(feature = "triggers")]
            trig: trigger::TrigState::default(),
            #[cfg(feature = "foreign_keys")]
            fk: fkey::FkState::default(),
        };
        conn.rebuild_catalog().await?;
        Ok(conn)
    }

    async fn rebuild_catalog(&mut self) -> Result<(), S::Error> {
        self.catalog_arena.reset();
        // With `v16` a statement may rebuild mid-way and then roll
        // the file back to the cookie the old catalog was built
        // from: a failed build must not leave that catalog (its
        // arena is gone) looking current, so it is replaced by an
        // empty one the next prepare rebuilds.
        #[cfg(feature = "v16")]
        {
            self.catalog = Catalog::empty(self.pager.schema_cookie().wrapping_add(1));
        }
        self.catalog =
            build_catalog(&mut self.pager, &self.catalog_arena, self.cfg.limits()).await?;
        Ok(())
    }

    /// Prepare one statement (§5.6 grammar). A stale schema
    /// cookie forces a catalog rebuild first (§5.5); a commit left
    /// mid-flight by a dropped future must be resumed or rolled
    /// back before anything new is prepared (§5.1.1).
    ///
    /// With feature `params` the SQL may hold parameters (`?`,
    /// `?NNN`, `:name`, `@name`, `$name`), NULL until bound with
    /// `Statement::bind_*`. Such a statement is planned for its
    /// unbound state here; an error a binding could change (a
    /// value error of a constant row, say) is deferred to the
    /// first `step`, which plans with the values bound by then.
    pub async fn prepare(&mut self, sql: &str) -> Result<Statement<'_, S>, S::Error> {
        if self.pager.in_commit() {
            return Err(Error::OperationInFlight);
        }
        if self.pager.schema_cookie() != self.catalog.cookie {
            self.rebuild_catalog().await?;
        }
        let arena = Arena::new(Scope::Statement, self.cfg.stmt_init, self.cfg.stmt_max);
        // The original statement text, retained for CREATE TABLE:
        // it is what lands in `sqlite_schema.sql` (§5.5).
        let sql_text = arena.alloc_copy_handle(sql.as_bytes())?;
        // v0.16: a statement on the TEMP database runs there.
        #[cfg(feature = "v16")]
        // (A statement reading TEMP views over main tables runs on
        // main as the text `MainAs` gives.)
        #[cfg(feature = "v16")]
        let mut main_as = None;
        #[cfg(feature = "v16")]
        let routed = match self.route(sql.as_bytes(), &arena)? {
            temp::Route::Main => None,
            temp::Route::MainAs(text) => {
                main_as = Some(text);
                None
            }
            temp::Route::Temp(text) => Some(self.prepare_temp(text, &arena).await?),
        };
        #[cfg(feature = "v16")]
        let main_text = main_as.as_ref().map_or(sql.as_bytes(), |t| t.as_slice());
        #[cfg(not(feature = "v16"))]
        let main_text = sql.as_bytes();
        #[cfg(not(feature = "v16"))]
        let routed: Option<()> = None;
        #[cfg(not(feature = "params"))]
        let (ast, plan) = match routed {
            #[cfg(feature = "v16")]
            Some(p) => (Ast::Begin, p),
            _ => {
                let ast = crate::sql::parse::<S::Error>(main_text, &arena, self.cfg.limits())?;
                let plan = self.plan(&arena, &ast)?;
                (ast, plan)
            }
        };
        #[cfg(feature = "params")]
        let (ast, plan, binds) = match routed {
            #[cfg(feature = "v16")]
            Some(p) => {
                let Plan::Temp { sql: t, .. } = &p else {
                    return Err(Error::Unsupported);
                };
                let (_, table) =
                    crate::sql::parse_params::<S::Error>(t.as_slice(), &arena, self.cfg.limits())?;
                let mut values = AVec::new();
                for _ in 0..table.count {
                    values.push(OwnedValue::Null);
                }
                let binds = Binds {
                    values,
                    table,
                    src: None,
                    plan_ckpt: arena.checkpoint(),
                    stale: false,
                    started: false,
                };
                (Ast::Begin, p, binds)
            }
            _ => self.prepare_params(main_text, &arena)?,
        };
        let row_ckpt = arena.checkpoint();
        Ok(Statement {
            #[cfg(feature = "params")]
            binds,
            conn: self,
            arena,
            sql_text,
            ast,
            plan,
            phase: Phase::Fresh,
            cursor: None,
            row_buf: AVec::new(),
            current: AVec::new(),
            current_rowid: None,
            row_ckpt,
            #[cfg(feature = "indexes")]
            index_rowids: AVec::new(),
            #[cfg(feature = "indexes")]
            index_pos: 0,
            #[cfg(feature = "indexes")]
            index_loaded: false,
            #[cfg(feature = "v04")]
            query_rows: AVec::new(),
            #[cfg(feature = "v04")]
            query_rowids: AVec::new(),
            #[cfg(feature = "v04")]
            query_pos: 0,
            #[cfg(feature = "v04")]
            query_loaded: false,
            #[cfg(feature = "v06")]
            hoist: AVec::new(),
            #[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
            ret_hoist: AVec::new(),
            #[cfg(feature = "v07")]
            ret_rows: AVec::new(),
            #[cfg(feature = "v07")]
            ret_rowids: AVec::new(),
            #[cfg(feature = "v07")]
            ret_pos: 0,
            #[cfg(feature = "v07")]
            ret_loaded: false,
            #[cfg(feature = "v16")]
            deferred: None,
            #[cfg(feature = "v16")]
            rec_rows: AVec::new(),
            #[cfg(feature = "v16")]
            mats: AVec::new(),
            #[cfg(feature = "v16")]
            stmt_sp: None,
            #[cfg(feature = "v16")]
            upsert_failed: false,
            #[cfg(feature = "v16")]
            autoinc: None,
            #[cfg(feature = "triggers")]
            trig: None,
            #[cfg(feature = "foreign_keys")]
            fk: None,
            #[cfg(feature = "foreign_keys")]
            fk_del: None,
            #[cfg(feature = "foreign_keys")]
            fk_skip: [None; 2],
        })
    }

    /// Prepare a statement routed to the TEMP database (v0.16,
    /// `v16`): prepared there once now, so its errors are prepare
    /// errors as on the pin, and again when it runs.
    #[cfg(feature = "v16")]
    async fn prepare_temp(&mut self, text: AVec<u8>, _arena: &Arena) -> Result<Plan, S::Error> {
        let Ok(t) = core::str::from_utf8(text.as_slice()) else {
            return Err(Error::Unsupported);
        };
        let c = self.temp_conn().await?;
        let prep: core::pin::Pin<
            Box<
                dyn core::future::Future<
                        Output = Result<
                            Statement<'_, crate::storage::MemStorage>,
                            crate::storage::MemError,
                        >,
                    > + '_,
            >,
        > = Box::pin(c.prepare(t));
        let width = prep.await.map_err(temp::mem_err)?.column_count();
        Ok(Plan::Temp { sql: text, width })
    }

    /// Parse and plan a statement that may hold parameters (v0.16,
    /// `params`). Without parameters this is `parse` + `plan`.
    /// With them, the plan is made for the all-NULL bindings (the
    /// unbound state); a failure that a binding could change (a
    /// value error such as a NOT NULL or MustBeInt failure of a
    /// constant row) is not reported here but deferred to the
    /// first `step`, which plans again with the actual bindings —
    /// as the pin reports such errors only when it runs the
    /// statement. Every other error (grammar, name resolution, a
    /// table that already exists, storage) is reported here, as
    /// the pin's prepare reports it (`binding_dependent`).
    #[cfg(feature = "params")]
    fn prepare_params(&self, sql: &[u8], arena: &Arena) -> Result<(Ast, Plan, Binds), S::Error> {
        let (ast, table) = crate::sql::parse_params::<S::Error>(sql, arena, self.cfg.limits())?;
        let plan_ckpt = arena.checkpoint();
        let mut binds = Binds {
            values: AVec::new(),
            table,
            src: None,
            plan_ckpt,
            stale: false,
            started: false,
        };
        if binds.table.count == 0 {
            let plan = self.plan(arena, &ast)?;
            return Ok((ast, plan, binds));
        }
        for _ in 0..binds.table.count {
            binds.values.push(OwnedValue::Null);
        }
        let bound = crate::sql::bind::bind_ast::<S::Error>(&ast, binds.values.as_slice(), arena)?;
        binds.src = Some(ast);
        match self.plan(arena, &bound) {
            Ok(plan) => Ok((bound, plan, binds)),
            Err(e) if !binding_dependent(&e) => Err(e),
            Err(_) => {
                binds.stale = true;
                Ok((bound, Plan::Deferred, binds))
            }
        }
    }

    #[cfg_attr(not(feature = "triggers"), inline(always))]
    fn plan(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        // v0.16 (`triggers`): the triggers a write may fire resolve
        // when it is prepared, as on the pin.
        #[cfg(feature = "triggers")]
        {
            use crate::sql::ast::TrigEvent;
            let body = match ast {
                Ast::With { body, .. } => body.as_ref(),
                a => a,
            };
            let names = |a: &AVec<(crate::arena::ABytes, Expr)>| {
                let mut out = AVec::new();
                for (n, _) in a.iter() {
                    out.push(copy_vec(handle_bytes(arena, *n)));
                }
                out
            };
            // A REPLACE's deletions fire the DELETE triggers under
            // `recursive_triggers`: the pin compiles them too.
            let replace =
                |c: &ConflictAction| *c == ConflictAction::Replace && self.prag.recursive_triggers;
            match body {
                Ast::Insert { conflict, .. } | Ast::Update { conflict, .. }
                    if replace(conflict) =>
                {
                    let (Ast::Insert { table, .. } | Ast::Update { table, .. }) = body else {
                        return Err(Error::Unsupported);
                    };
                    self.validate_triggers(
                        handle_bytes(arena, *table),
                        TrigEvent::Delete,
                        None,
                        arena,
                    )?;
                }
                _ => {}
            }
            match body {
                Ast::Insert { table, upsert, .. } => {
                    let t = handle_bytes(arena, *table);
                    self.validate_triggers(t, TrigEvent::Insert, None, arena)?;
                    if let Some(u) = upsert
                        && let crate::sql::UpsertAction::Update { assignments, .. } = &u.action
                    {
                        let n = names(assignments);
                        self.validate_triggers(t, TrigEvent::Update, Some(n.as_slice()), arena)?;
                    }
                }
                Ast::Update {
                    table, assignments, ..
                } => {
                    let n = names(assignments);
                    let t = handle_bytes(arena, *table);
                    self.validate_triggers(t, TrigEvent::Update, Some(n.as_slice()), arena)?;
                }
                Ast::Delete { table, .. } => {
                    let t = handle_bytes(arena, *table);
                    self.validate_triggers(t, TrigEvent::Delete, None, arena)?;
                }
                _ => {}
            }
        }
        let plan = self.plan_inner(arena, ast)?;
        // v0.16 (`foreign_keys`): the keys a write uses resolve when
        // it is prepared, as on the pin.
        #[cfg(feature = "foreign_keys")]
        self.fk_validate(ast, &plan, arena)?;
        Ok(plan)
    }

    fn plan_inner(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        // v0.16 (`triggers`): a write of a view with INSTEAD OF
        // triggers for it (its sources are expanded there).
        #[cfg(feature = "triggers")]
        if let Some(p) = self.plan_instead(arena, ast)? {
            return Ok(p);
        }
        // v0.16: views and CTEs are read as derived tables — every
        // reference is expanded before anything else is planned.
        #[cfg(feature = "v16")]
        let expanded = self.expand_ast(arena, ast)?;
        #[cfg(feature = "v16")]
        let ast = expanded.as_ref().unwrap_or(ast);
        // The pin reserves every `sqlite_*` name (any case) for
        // itself (v0.16 fix, ungated: hipp created such tables,
        // indexes and views). Files that already hold one still
        // load: the catalog parses stored SQL without planning.
        let created = match ast {
            Ast::CreateTable { name, .. } => Some(*name),
            #[cfg(feature = "indexes")]
            Ast::CreateIndex { name, .. } => Some(*name),
            #[cfg(feature = "v04")]
            Ast::CreateView { name, .. } => Some(*name),
            _ => None,
        };
        if let Some(n) = created {
            reserved_name::<S::Error>(handle_bytes(arena, n))?;
        }
        // The pin's system tables (`sqlite_*`, v0.16 fix, ungated):
        // none may be indexed or altered, and only `sqlite_stat*`
        // (and `sqlite_parameters`) may be dropped — hipp indexed,
        // altered and dropped `sqlite_sequence`, breaking
        // AUTOINCREMENT for the pin.
        #[cfg(any(feature = "indexes", feature = "v04"))]
        {
            let sys = |h: crate::arena::ABytes| {
                let n = handle_bytes(arena, h);
                n.get(..7).is_some_and(|p| eq_ci(p, b"sqlite_"))
                    && self.catalog.find(&self.catalog_arena, n).is_some()
            };
            let refused = match ast {
                #[cfg(feature = "indexes")]
                Ast::CreateIndex { table, .. } => sys(*table),
                #[cfg(feature = "v04")]
                Ast::AlterAddColumn { table, .. } => sys(*table),
                #[cfg(feature = "v05")]
                Ast::DropTable { name, .. } => {
                    let n = handle_bytes(arena, *name);
                    sys(*name)
                        && !n.get(7..11).is_some_and(|p| eq_ci(p, b"stat"))
                        && !n.get(7..).is_some_and(|p| eq_ci(p, b"parameters"))
                }
                _ => false,
            };
            if refused {
                return Err(Error::Schema {
                    kind: SchemaKind::SystemTable,
                });
            }
        }
        // v0.16: `sqlite_schema` is readable, never writable; and
        // with `PRAGMA foreign_keys = ON` a table in a foreign key
        // is not writable either unless `foreign_keys` enforces
        // them.
        #[cfg(feature = "v16")]
        {
            let target = match ast {
                Ast::Insert { table, .. } | Ast::CreateIndex { table, .. } => Some(*table),
                Ast::AlterAddColumn { table, .. } => Some(*table),
                #[cfg(feature = "v05")]
                Ast::Update { table, .. } | Ast::Delete { table, .. } => Some(*table),
                #[cfg(feature = "v05")]
                Ast::DropTable { name, .. } => Some(*name),
                _ => None,
            };
            if let Some(h) = target {
                let n = handle_bytes(arena, h);
                if crate::schema::Catalog::is_schema_name(n) {
                    return Err(Error::Schema {
                        kind: SchemaKind::SchemaTableReadOnly,
                    });
                }
                // Without `foreign_keys` a table in a foreign key is
                // not writable while the pragma is ON (never skipped
                // silently); with it the keys are enforced.
                #[cfg(not(feature = "foreign_keys"))]
                if !matches!(ast, Ast::CreateIndex { .. }) {
                    self.fk_guard(n)?;
                }
            }
        }
        match ast {
            // Unwrapped by `expand_ast` above.
            #[cfg(feature = "v16")]
            Ast::With { .. } => Err(Error::Unsupported),
            #[cfg(feature = "triggers")]
            Ast::CreateTrigger(t) => self.plan_create_trigger(arena, t),
            #[cfg(feature = "triggers")]
            Ast::DropTrigger { name, if_exists } => {
                self.plan_drop_trigger(arena, *name, *if_exists)
            }
            #[cfg(feature = "v16")]
            Ast::Pragma {
                schema,
                name,
                value,
                numeric,
            } => self.plan_pragma(arena, *schema, *name, *value, *numeric),
            #[cfg(feature = "v16")]
            Ast::AlterRename { .. }
            | Ast::AlterRenameColumn { .. }
            | Ast::AlterDropColumn { .. } => self.plan_alter16(arena, ast),
            #[cfg(feature = "v16")]
            Ast::Savepoint { .. }
            | Ast::Release { .. }
            | Ast::RollbackTo { .. }
            | Ast::Vacuum { .. }
            | Ast::Reindex { .. }
            | Ast::Analyze { .. } => self.plan_maint(arena, ast),
            #[cfg(not(feature = "v04"))]
            Ast::CreateTable { .. } => Ok(Plan::CreateTable),
            #[cfg(feature = "v04")]
            Ast::CreateTable {
                name,
                columns,
                pk_columns,
                checks,
                #[cfg(feature = "v16")]
                strict,
                #[cfg(feature = "v16")]
                as_select,
                #[cfg(feature = "v16")]
                if_not_exists,
                ..
            } => {
                #[cfg(feature = "v16")]
                if let Some(sel) = as_select {
                    // The pin's `sqlite3StartTable` refuses a taken
                    // name (or makes IF NOT EXISTS a no-op) before
                    // the SELECT is resolved, at prepare.
                    let tname = handle_bytes(arena, *name);
                    let (ca, cat) = (&self.catalog_arena, &self.catalog);
                    if cat.find(ca, tname).is_some()
                        || cat.find_view(ca, tname).is_some()
                        || cat.find_index(ca, tname).is_some()
                    {
                        if *if_not_exists {
                            return Ok(Plan::CreateTableAs {
                                select: None,
                                sql: AVec::new(),
                                affs: AVec::new(),
                            });
                        }
                        return Err(Error::Schema {
                            kind: SchemaKind::DuplicateTable,
                        });
                    }
                    return self.plan_create_as(arena, tname, sel);
                }
                #[cfg(feature = "v16")]
                let strict = *strict;
                #[cfg(not(feature = "v16"))]
                let strict = false;
                // v0.16: refuse what the pin refuses at definition
                // time (DEFAULT / PK on a generated column, a
                // non-constant DEFAULT, a generated-column loop,
                // an unresolvable CHECK, a STRICT column type).
                // A too-big hex literal in a CHECK is the pin's
                // error only once a statement codes the check
                // (v0.16 review fix): the INSERT / UPDATE that
                // plans the rules reports it, so such a CHECK is
                // left out here. (One in a generated column the
                // pin refuses at once.)
                let mut kept = AVec::new();
                for h in checks.iter() {
                    if !crate::rules::has_big_hex::<S::Error>(
                        handle_bytes(arena, *h),
                        arena,
                        self.cfg.limits(),
                    )? {
                        kept.push(*h);
                    }
                }
                crate::rules::validate_def::<S::Error>(
                    arena,
                    self.cfg.limits(),
                    handle_bytes(arena, *name),
                    columns.as_slice(),
                    pk_columns.as_slice(),
                    kept.as_slice(),
                    strict,
                )?;
                #[cfg(feature = "v16")]
                if self.prag.like_redefined
                    && crate::rules::gen_uses_like::<S::Error>(
                        arena,
                        self.cfg.limits(),
                        columns.as_slice(),
                    )?
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::NonDeterministic,
                    });
                }
                Ok(Plan::CreateTable)
            }
            #[cfg(feature = "indexes")]
            Ast::CreateIndex {
                table,
                columns,
                unique,
                ..
            } => {
                let tname = handle_bytes(arena, *table);
                let t = self
                    .catalog
                    .find(&self.catalog_arena, tname)
                    .ok_or(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    })?
                    .usable()?;
                let mut cols = AVec::new();
                for cd in columns.iter() {
                    let nb = handle_bytes(arena, cd.name);
                    let col = match self.resolve_target(t, nb)? {
                        // `rowid` / `oid` / `_rowid_` name no column
                        // an index can hold (v0.16 fix, ungated: hipp
                        // built the index the pin refuses with "no
                        // such column"); an INTEGER PRIMARY KEY
                        // column by its own name is the rowid.
                        Target::Rowid => {
                            return Err(Error::Schema {
                                kind: SchemaKind::UnknownColumn,
                            });
                        }
                        Target::Col(i) => {
                            if Some(i) == t.rowid_alias {
                                None
                            } else {
                                Some(i)
                            }
                        }
                    };
                    // v0.16: the key collation — explicit, else
                    // the column's (BINARY for the rowid).
                    let coll = cd.coll.unwrap_or_else(|| {
                        col.and_then(|i| t.columns.as_slice().get(i))
                            .map(|c| c.coll)
                            .unwrap_or_default()
                    });
                    cols.push(IdxColPlan {
                        col,
                        desc: cd.desc,
                        coll,
                    });
                }
                let mut defaults = AVec::new();
                for c in t.columns.iter() {
                    defaults.push(read_default(&self.catalog_arena, c));
                }
                Ok(Plan::CreateIndex {
                    table_root: t.root,
                    rowid_alias: t.rowid_alias,
                    ncols: t.columns.len(),
                    defaults,
                    cols,
                    unique: *unique,
                    #[cfg(feature = "v04")]
                    gcols: if t.has_generated() {
                        self.table_rules(arena, t)?
                            .and_then(|r| r.gcols)
                            .map(Box::new)
                    } else {
                        None
                    },
                })
            }
            #[cfg(feature = "indexes")]
            Ast::DropIndex { name, if_exists } => {
                let nb = handle_bytes(arena, *name);
                match self.catalog.find_index(&self.catalog_arena, nb) {
                    None => {
                        if *if_exists {
                            Ok(Plan::DropIndex { root: None })
                        } else {
                            Err(Error::Schema {
                                kind: SchemaKind::UnknownIndex,
                            })
                        }
                    }
                    Some(ix) => {
                        if ix.origin == IndexOrigin::Auto {
                            return Err(Error::Schema {
                                kind: SchemaKind::DropAutoIndex,
                            });
                        }
                        Ok(Plan::DropIndex {
                            root: Some(ix.root),
                        })
                    }
                }
            }
            #[cfg(not(feature = "indexes"))]
            Ast::CreateIndex { .. } | Ast::DropIndex { .. } => Err(Error::Unsupported),
            Ast::Insert {
                table,
                columns,
                rows,
                select,
                conflict,
                upsert,
                returning,
            } => {
                let tname = handle_bytes(arena, *table);
                let t = self
                    .catalog
                    .find(&self.catalog_arena, tname)
                    .ok_or(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    })?
                    .usable()?;
                // v0.16, every profile: a table whose triggers this
                // build cannot run is not written (never skipping
                // them silently).
                #[cfg(not(feature = "triggers"))]
                let blocked = t.triggered;
                #[cfg(feature = "triggers")]
                let blocked = self.catalog.trigger_blocked(&self.catalog_arena, tname);
                if blocked {
                    return Err(Error::Schema {
                        kind: SchemaKind::TableHasTriggers,
                    });
                }
                // An AUTOINCREMENT table (in a file the pin made) is
                // read-only without `v16`: those builds keep no
                // `sqlite_sequence` (v0.16 fix: hipp inserted rows
                // and left the sequence behind, so the pin's next
                // INSERT reused a rowid AUTOINCREMENT had handed out).
                #[cfg(not(feature = "v16"))]
                crate::sql::parser::schema_text_start::<S::Error>(
                    handle_bytes(&self.catalog_arena, t.sql),
                    arena,
                )?;
                #[cfg(not(feature = "v08"))]
                let _ = select;
                #[cfg(not(feature = "v07"))]
                let _ = (upsert, returning);
                // v0.16: a generated column takes no value — an
                // INSERT without a column list supplies the other
                // columns, in order, and naming one is an error.
                #[cfg(feature = "v04")]
                let is_gen = |i: usize| t.columns.as_slice()[i].gen_expr.is_some();
                #[cfg(not(feature = "v04"))]
                let is_gen = |_: usize| false;
                let mut targets = AVec::new();
                match columns {
                    None => {
                        for i in 0..t.columns.len() {
                            if !is_gen(i) {
                                targets.push(Target::Col(i));
                            }
                        }
                    }
                    Some(names) => {
                        for n in names.iter() {
                            let nb = handle_bytes(arena, *n);
                            let tg = self.resolve_target(t, nb)?;
                            if matches!(tg, Target::Col(i) if is_gen(i)) {
                                return Err(Error::Schema {
                                    kind: SchemaKind::GeneratedColumnWrite,
                                });
                            }
                            targets.push(tg);
                        }
                    }
                }
                // Every row of a multi-row VALUES must match the
                // expected width (SQLite: "all VALUES must have
                // the same number of terms").
                let width = targets.len();
                for r in rows.iter() {
                    if r.len() != width {
                        return Err(Error::Schema {
                            kind: SchemaKind::ColumnCountMismatch,
                        });
                    }
                }
                let mut cols = AVec::new();
                // One clock read for the whole statement.
                #[cfg(feature = "v04")]
                let mut clock = crate::rules::Clock::new(self.cfg.clock);
                for (i, c) in t.columns.iter().enumerate() {
                    // v0.16: an expression DEFAULT the catalog
                    // could not fold is evaluated only when a row
                    // can need it — the column is not supplied,
                    // or OR REPLACE may substitute it for a NULL
                    // in a NOT NULL column — so its clock read or
                    // unknown function errors exactly when the
                    // pin's would.
                    #[cfg(feature = "v16")]
                    let mut per_row = None;
                    #[cfg(feature = "v04")]
                    let default = if c.default_expr.is_some()
                        && (!targets.iter().any(|tg| *tg == Target::Col(i))
                            || (c.not_null && *conflict == ConflictAction::Replace))
                    {
                        #[cfg(feature = "v16")]
                        {
                            let (v, pr) = self.column_default_per_row(arena, c, &mut clock)?;
                            per_row = pr;
                            v
                        }
                        #[cfg(not(feature = "v16"))]
                        self.column_default(arena, c, &mut clock)?
                    } else {
                        literal_owned(&self.catalog_arena, c.default)
                    };
                    #[cfg(not(feature = "v04"))]
                    let default = {
                        let _ = i;
                        literal_owned(&self.catalog_arena, c.default)
                    };
                    cols.push(InsCol {
                        affinity: c.affinity,
                        default,
                        not_null: c.not_null,
                        #[cfg(feature = "v16")]
                        per_row,
                    });
                }
                #[cfg(feature = "indexes")]
                let indexes = {
                    let mut out: AVec<IndexPlan> = AVec::new();
                    for ix in self.catalog.indexes_for(&self.catalog_arena, tname) {
                        if ix.opaque {
                            // An index hipp cannot maintain:
                            // refuse the write rather than let it
                            // silently desynchronise (schema.rs).
                            return Err(Error::Unsupported);
                        }
                        let mut cols = AVec::new();
                        let mut desc = AVec::new();
                        for c in ix.columns.iter() {
                            let p = IdxColPlan {
                                col: c.col,
                                desc: c.desc,
                                coll: c.coll,
                            };
                            desc.push(p.order());
                            cols.push(p);
                        }
                        out.push(IndexPlan {
                            root: ix.root,
                            unique: ix.unique,
                            cols,
                            desc,
                        });
                    }
                    // Checked most recently created first, as the pin
                    // walks its (prepended) index list (v0.16 review
                    // fix: which REPLACE victim goes first shows in
                    // its DELETE triggers).
                    out.as_mut_slice().reverse();
                    out
                };
                // v0.8: an INSERT ... SELECT source is planned
                // as a subquery against the empty enclosing
                // scope. Its result width must match the target
                // width, and an upsert clause after it is only
                // accepted when the SELECT's FROM clause is
                // terminated by a WHERE, ORDER BY or LIMIT (or
                // there is no FROM at all) — the pinned oracle's
                // parser rejects `... FROM t ON CONFLICT ...`
                // outright (probed: the bare-FROM upsert is a
                // syntax error there, the terminated and
                // FROM-less forms parse), so hipp refuses the
                // same shape rather than accepting a superset.
                #[cfg(feature = "v08")]
                let select_plan = match select {
                    Some(sel) => {
                        // A compound source (v0.9) terminates
                        // the SELECT before ON CONFLICT on the
                        // pin too, so the quirk refusal applies
                        // only to a bare single core. v0.16: so
                        // do GROUP BY / HAVING and a last join's
                        // ON / USING constraint — only an `ON`
                        // that could still start the last
                        // source's join constraint is the quirk
                        // (review regression: those shapes were
                        // refused; the pin accepts them).
                        if upsert.is_some()
                            && sel.compound.is_empty()
                            && sel
                                .from
                                .as_slice()
                                .last()
                                .is_some_and(|s| s.using.is_empty() && s.on.is_none())
                            && sel.filter.is_none()
                            && sel.group_by.is_empty()
                            && sel.having.is_none()
                            && sel.order_by.is_empty()
                            && sel.limit.is_none()
                        {
                            return Err(Error::Unsupported);
                        }
                        let p = self.plan_subquery(sel, arena, &[])?;
                        if p.width() != width {
                            return Err(Error::Schema {
                                kind: SchemaKind::ColumnCountMismatch,
                            });
                        }
                        Some(Box::new(p))
                    }
                    None => None,
                };
                #[cfg(feature = "v08")]
                let (rows_v, rows_r) = if select_plan.is_some() {
                    (AVec::new(), None)
                } else {
                    // Resolve the VALUES expressions against the
                    // empty scope with the real subquery planner.
                    // Rows without subqueries fold to constants
                    // here, exactly as `eval_value_expr` did; a
                    // row set containing subqueries is kept
                    // resolved for step-time evaluation (all rows
                    // evaluated before any insert — see the
                    // `rows_r` plan field).
                    let cur = crate::query::ScopeGroup::new(AVec::new(), AVec::new(), 0);
                    let mut planner = self.sub_planner(arena);
                    let mut resolved: AVec<AVec<RExpr>> = AVec::new();
                    let mut any_sub = false;
                    for r in rows.iter() {
                        let mut rr = AVec::new();
                        for e in r.iter() {
                            let re = crate::query::resolve_scoped::<S::Error>(
                                e,
                                arena,
                                &cur,
                                &[],
                                &mut planner,
                            )?;
                            if crate::query::has_agg(&re) {
                                return Err(Error::Unsupported);
                            }
                            if crate::query::has_sub(&re) {
                                any_sub = true;
                            }
                            // v0.16: a row reading last_insert_rowid()
                            // evaluates at step time, just before its
                            // own insert (see `exec_insert`).
                            #[cfg(feature = "v16")]
                            if reads_last_rowid(&re) {
                                any_sub = true;
                            }
                            rr.push(re);
                        }
                        resolved.push(rr);
                    }
                    if any_sub {
                        (AVec::new(), Some(resolved))
                    } else {
                        let empty = RowData::empty();
                        let mut folded = AVec::new();
                        let mut fold_err = None;
                        'rows: for rr in resolved.iter() {
                            let mut rv = AVec::new();
                            for re in rr.iter() {
                                match crate::query::eval::<S::Error>(
                                    re,
                                    &Ctx::of(&empty).with(&self.env),
                                ) {
                                    Ok(v) => rv.push(v),
                                    Err(e) => {
                                        fold_err = Some(e);
                                        break 'rows;
                                    }
                                }
                            }
                            folded.push(rv);
                        }
                        match fold_err {
                            None => (folded, None),
                            // v0.16 review fix: a row that fails to
                            // evaluate is evaluated again at step
                            // time, one row at a time (`exec_insert`'s
                            // per-row path): the rows before it are
                            // inserted first, as on the pin, so a
                            // failed statement leaves
                            // last_insert_rowid() at the last of them.
                            #[cfg(feature = "v16")]
                            Some(Error::Schema { .. }) => (AVec::new(), Some(resolved)),
                            Some(e) => return Err(e),
                        }
                    }
                };
                #[cfg(not(feature = "v08"))]
                let rows_v = {
                    let mut rows_v = AVec::new();
                    for r in rows.iter() {
                        let mut rv = AVec::new();
                        for e in r.iter() {
                            rv.push(self.eval_value_expr(arena, e)?);
                        }
                        rows_v.push(rv);
                    }
                    rows_v
                };
                #[cfg(feature = "v07")]
                let upsert_plan = match upsert {
                    Some(u) => Some(self.plan_upsert(arena, t, tname, u)?),
                    None => None,
                };
                #[cfg(feature = "v07")]
                let returning_r = self.plan_returning(arena, t, tname, returning)?;
                Ok(Plan::Insert {
                    root: t.root,
                    rowid_alias: t.rowid_alias,
                    cols,
                    targets,
                    rows_v,
                    #[cfg(feature = "v08")]
                    rows_r,
                    #[cfg(feature = "v08")]
                    select: select_plan,
                    conflict: *conflict,
                    #[cfg(feature = "v07")]
                    upsert: upsert_plan,
                    #[cfg(feature = "v07")]
                    returning: returning_r,
                    #[cfg(feature = "indexes")]
                    indexes,
                    #[cfg(feature = "v04")]
                    rules: self.table_rules(arena, t)?,
                    #[cfg(feature = "v16")]
                    autoinc: if t.autoinc {
                        Some(copy_vec(handle_bytes(&self.catalog_arena, t.name)))
                    } else {
                        None
                    },
                })
            }
            Ast::Begin => {
                if self.explicit_txn {
                    return Err(Error::TransactionState { nested: true });
                }
                Ok(Plan::Begin)
            }
            #[cfg(feature = "v16")]
            Ast::BeginImmediate => {
                if self.explicit_txn {
                    return Err(Error::TransactionState { nested: true });
                }
                Ok(Plan::BeginImmediate)
            }
            Ast::Commit => Ok(Plan::Commit),
            Ast::Rollback => Ok(Plan::Rollback),
            Ast::Select(stmt) => self.plan_select(stmt, arena),
            #[cfg(feature = "v04")]
            Ast::AlterAddColumn {
                table,
                column,
                text,
                ..
            } => {
                let tname = handle_bytes(arena, *table);
                let t = match self.catalog.find(&self.catalog_arena, tname) {
                    Some(t) => t.usable()?,
                    None => {
                        if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                            return Err(Error::Unsupported);
                        }
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownTable,
                        });
                    }
                };
                // The pin's "too many columns on t" (v0.16 fix,
                // ungated: the 2001st column made a schema it
                // cannot load).
                if t.columns.len() >= crate::sql::parser::MAX_COLUMN {
                    return Err(Error::Schema {
                        kind: SchemaKind::TooManyColumns,
                    });
                }
                let cname = handle_bytes(arena, column.name);
                if t.columns
                    .iter()
                    .any(|c| eq_ci(handle_bytes(&self.catalog_arena, c.name), cname))
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::DuplicateColumn,
                    });
                }
                // v0.16: a VIRTUAL generated column may be added
                // (it occupies no record field); a STORED one may
                // not ("cannot add a STORED column").
                if column.primary_key || column.pk_desc || column.unique {
                    return Err(Error::Schema {
                        kind: SchemaKind::CannotAddColumn,
                    });
                }
                let mut empty_only = None;
                // v0.16 fix (ungated): with `PRAGMA foreign_keys` ON a
                // REFERENCES column takes no non-NULL default while
                // the table has rows (the pin's "Cannot add a
                // REFERENCES column with non-NULL default value",
                // through `sqlite3ErrorIfNotEmpty` and emitted before
                // its other such refusals; v0.16 review fix: an empty
                // table takes the column).
                #[cfg(feature = "v16")]
                if self.prag.foreign_keys
                    && (column.default_expr.is_some()
                        || !matches!(literal_owned(arena, column.default), OwnedValue::Null))
                    && def_references::<S::Error>(handle_bytes(arena, *text), arena)
                {
                    empty_only = Some(SchemaKind::ReferencesDefault);
                }
                // The pin's other refusals fire only when the table
                // has rows (`sqlite3ErrorIfNotEmpty`, checked at
                // step time): a STORED generated column, a DEFAULT
                // outside the `sqlite3ValueFromExpr` shape — a
                // (signed / CAST) literal: `-'3'` and `-x'01'` add
                // anywhere, `(1+2)` and CURRENT_TIMESTAMP only to an
                // empty table (v0.16: they were always refused) —
                // and NOT NULL with no default or a literal NULL.
                let mut refuse = column.generated && column.stored;
                // The new column's value in every existing row.
                let new_default = match column.default_expr {
                    Some(h) if !column.generated => {
                        let dt = handle_bytes(arena, h);
                        if crate::rules::alter_default_ok::<S::Error>(dt, arena, self.cfg.limits())?
                        {
                            crate::rules::eval_default::<S::Error>(
                                dt,
                                arena,
                                self.cfg.limits(),
                                &mut crate::rules::Clock::new(None),
                                Some(&self.env),
                            )?
                        } else {
                            refuse = true;
                            OwnedValue::Null
                        }
                    }
                    _ => literal_owned(arena, column.default),
                };
                if column.not_null
                    && !column.generated
                    && column.default_expr.is_none()
                    && matches!(new_default, OwnedValue::Null)
                {
                    refuse = true;
                }
                if refuse && empty_only.is_none() {
                    empty_only = Some(SchemaKind::CannotAddColumn);
                }
                let base = handle_bytes(&self.catalog_arena, t.sql);
                let pos = add_column_offset::<S::Error>(base, arena)?;
                let mut new_sql = AVec::new();
                new_sql.extend_from_slice(&base[..pos]);
                new_sql.extend_from_slice(b", ");
                new_sql.extend_from_slice(handle_bytes(arena, *text));
                new_sql.extend_from_slice(&base[pos..]);
                // v0.16: the rewritten definition must stand as a
                // CREATE TABLE of its own (a generated column's
                // expression, a new CHECK), and — like the pin's
                // post-ADD `quick_check` — when it has CHECK
                // constraints, a generated NOT NULL column or is
                // STRICT, every existing row is verified against
                // it at step time (the new column reading its
                // default, or computed).
                let Ast::CreateTable {
                    columns: ncols_def,
                    pk_columns,
                    checks,
                    #[cfg(feature = "v16")]
                    strict,
                    ..
                } = crate::sql::parse::<S::Error>(new_sql.as_slice(), arena, self.cfg.limits())?
                else {
                    return Err(Error::Unsupported);
                };
                #[cfg(feature = "v16")]
                let strict_def = strict;
                #[cfg(not(feature = "v16"))]
                let strict_def = false;
                #[allow(unused_mut)]
                let mut rules = crate::rules::validate_def::<S::Error>(
                    arena,
                    self.cfg.limits(),
                    tname,
                    ncols_def.as_slice(),
                    pk_columns.as_slice(),
                    checks.as_slice(),
                    strict_def,
                )?;
                // The pin re-checks the whole rewritten definition.
                #[cfg(feature = "v16")]
                if self.prag.like_redefined
                    && crate::rules::gen_uses_like::<S::Error>(
                        arena,
                        self.cfg.limits(),
                        ncols_def.as_slice(),
                    )?
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::NonDeterministic,
                    });
                }
                // `PRAGMA ignore_check_constraints` (v0.16).
                #[cfg(feature = "v16")]
                if self.prag.ignore_checks {
                    rules.checks = AVec::new();
                }
                let verify = if !rules.checks.is_empty()
                    || (column.generated && column.not_null)
                    || strict_def
                {
                    let mut defaults = AVec::new();
                    let mut not_null = AVec::new();
                    for c in t.columns.iter() {
                        defaults.push(read_default(&self.catalog_arena, c));
                        not_null.push(c.not_null);
                    }
                    not_null.push(column.not_null);
                    let mut affs = AVec::new();
                    for c in ncols_def.iter() {
                        let d = handle_bytes(arena, c.decl);
                        affs.push(
                            if strict_def && eq_ci(crate::record::decl_token(d), b"ANY") {
                                crate::record::Affinity::Blob
                            } else {
                                crate::record::affinity_of(d)
                            },
                        );
                    }
                    let aff = affs
                        .as_slice()
                        .last()
                        .copied()
                        .unwrap_or(crate::record::Affinity::Blob);
                    defaults.push(apply_affinity(new_default, aff));
                    Some(Box::new(AlterVerify {
                        rules,
                        defaults,
                        affs,
                        not_null,
                        rowid_alias: t.rowid_alias,
                    }))
                } else {
                    None
                };
                Ok(Plan::AlterAdd {
                    root: t.root,
                    table: copy_vec(tname),
                    new_sql,
                    verify,
                    empty_only,
                })
            }
            // v0.16: the pin stores a view without resolving it; a
            // missing table, a column-count mismatch or a cycle is
            // reported when the view is read.
            #[cfg(feature = "v16")]
            Ast::CreateView { .. } => Ok(Plan::CreateView),
            #[cfg(all(feature = "v04", not(feature = "v16")))]
            Ast::CreateView {
                columns, select, ..
            } => {
                let width = self.view_select_width(select, arena)?;
                if let Some(cols) = columns
                    && cols.len() != width
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::ColumnCountMismatch,
                    });
                }
                Ok(Plan::CreateView)
            }
            #[cfg(feature = "v04")]
            Ast::DropView { name, if_exists } => {
                let nb = handle_bytes(arena, *name);
                match self.catalog.find_view(&self.catalog_arena, nb) {
                    None => {
                        // The oracle refuses DROP VIEW on a table
                        // even under IF EXISTS (v0.5 probe).
                        if self.catalog.find(&self.catalog_arena, nb).is_some() {
                            return Err(Error::Schema {
                                kind: SchemaKind::IsTable,
                            });
                        }
                        if *if_exists {
                            Ok(Plan::DropView { exists: false })
                        } else {
                            Err(Error::Schema {
                                kind: SchemaKind::UnknownView,
                            })
                        }
                    }
                    Some(_) => Ok(Plan::DropView { exists: true }),
                }
            }
            #[cfg(not(feature = "v04"))]
            Ast::AlterAddColumn { .. } | Ast::CreateView { .. } | Ast::DropView { .. } => {
                Err(Error::Unsupported)
            }
            #[cfg(feature = "v05")]
            Ast::Update {
                table,
                alias,
                conflict,
                assignments,
                from,
                filter,
                returning,
            } => self.plan_update(
                arena,
                table,
                alias,
                *conflict,
                assignments,
                from,
                filter,
                returning,
            ),
            #[cfg(feature = "v05")]
            Ast::Delete {
                table,
                filter,
                returning,
            } => self.plan_delete(arena, table, filter, returning),
            #[cfg(feature = "v05")]
            Ast::DropTable { name, if_exists } => {
                let nb = handle_bytes(arena, *name);
                match self.catalog.find(&self.catalog_arena, nb) {
                    None => {
                        // The oracle refuses DROP TABLE on a view
                        // even under IF EXISTS (v0.5 probe).
                        if self.catalog.find_view(&self.catalog_arena, nb).is_some() {
                            return Err(Error::Schema {
                                kind: SchemaKind::IsView,
                            });
                        }
                        if *if_exists {
                            Ok(Plan::DropTable {
                                root: None,
                                index_roots: AVec::new(),
                                seq: None,
                            })
                        } else {
                            Err(Error::Schema {
                                kind: SchemaKind::UnknownTable,
                            })
                        }
                    }
                    Some(t) => {
                        let mut index_roots = AVec::new();
                        for ix in self.catalog.indexes_for(&self.catalog_arena, nb) {
                            index_roots.push(ix.root);
                        }
                        let seq = match self.catalog.find(&self.catalog_arena, b"sqlite_sequence") {
                            Some(sq)
                                if table_autoinc::<S::Error>(t, &self.catalog_arena, arena) =>
                            {
                                Some((sq.root, copy_vec(handle_bytes(&self.catalog_arena, t.name))))
                            }
                            _ => None,
                        };
                        Ok(Plan::DropTable {
                            root: Some(t.root),
                            index_roots,
                            seq,
                        })
                    }
                }
            }
            #[cfg(not(feature = "v05"))]
            Ast::Update { .. } | Ast::Delete { .. } | Ast::DropTable { .. } => {
                Err(Error::Unsupported)
            }
        }
    }

    /// The table an UPDATE/DELETE writes to: a view is refused
    /// with `IsView` (the oracle: "cannot modify ... because it
    /// is a view"), a missing name with `UnknownTable`.
    #[cfg(feature = "v05")]
    fn write_table(
        &self,
        arena: &Arena,
        table: &crate::arena::ABytes,
    ) -> Result<&crate::schema::TableDef, S::Error> {
        let tname = handle_bytes(arena, *table);
        // v0.16, every profile: as for INSERT.
        if self.catalog.trigger_blocked(&self.catalog_arena, tname) {
            return Err(Error::Schema {
                kind: SchemaKind::TableHasTriggers,
            });
        }
        match self.catalog.find(&self.catalog_arena, tname) {
            Some(t) => t.usable(),
            None => {
                if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                    return Err(Error::Schema {
                        kind: SchemaKind::IsView,
                    });
                }
                Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                })
            }
        }
    }

    /// Plan `CREATE TABLE name AS SELECT ...` (v0.16, `v16`):
    /// the SELECT plans as a subquery against the empty scope;
    /// the new table's columns take the pin's names (alias, else
    /// the column's own name — `rowid` for a rowid reference, or
    /// the rowid-alias column's name — else the expression text;
    /// duplicates suffixed `:1`, `:2`, ...) and the affinity of
    /// each result item (`sqlite3ExprAffinity`, with the pin's
    /// demotion across compound cores), which also gives the
    /// declared type in the derived schema text.
    #[cfg(feature = "v16")]
    fn plan_create_as(
        &self,
        arena: &Arena,
        tname: &[u8],
        sel: &SelectStmt,
    ) -> Result<Plan, S::Error> {
        use crate::record::Affinity;
        use crate::rules::item_affinity;
        let plan = self.plan_subquery(sel, arena, &[])?;
        let w = plan.width();
        let mut names = self.result_col_names(&plan, (0, 0))?;
        let mut affs: AVec<Affinity> = AVec::new();
        for i in 0..w {
            let item = plan.items.as_slice().get(i);
            // Demoted across compound cores; a numeric CAST is the
            // pin's FLEXNUM, declared ` NUM`.
            let aff = crate::rules::compound_affinity(
                item.map_or(Affinity::Blob, item_affinity),
                item,
                plan.compound.iter().map(|(_, t)| &**t),
                i,
            );
            affs.push(aff);
        }
        crate::rules::unique_names::<S::Error>(names.as_mut_slice())?;
        let mut cols = AVec::new();
        for (n, a) in names.iter().zip(affs.iter()) {
            cols.push((n.clone(), *a));
        }
        Ok(Plan::CreateTableAs {
            select: Some(Box::new(plan)),
            sql: crate::rules::ctas_sql(tname, cols.as_slice()),
            affs,
        })
    }

    /// A planned SELECT's result column names the pin's way
    /// (`sqlite3ColumnsFromExprList`; v0.16): an alias, else for an
    /// unaliased column reference (COLLATE skipped) its source's own
    /// column name — `A` and `s.A` name the column `a` —, for a
    /// rowid reference `rowid` or the rowid-alias column's name, else
    /// the result name (the expression's text). CREATE TABLE ... AS,
    /// derived tables, views and CTEs name their columns by it.
    /// Not made unique. `base` is the plan's enclosing scopes'
    /// `(width, source count)`, where its own slots begin.
    #[cfg(feature = "v16")]
    fn result_col_names(
        &self,
        plan: &crate::query::SubPlan,
        base: (usize, usize),
    ) -> Result<AVec<AVec<u8>>, S::Error> {
        let mut names: AVec<AVec<u8>> = AVec::new();
        for i in 0..plan.width() {
            let mut name = plan
                .result_names
                .as_slice()
                .get(i)
                .cloned()
                .unwrap_or_default();
            let item = plan.items.as_slice().get(i);
            // The pin's `sqlite3ColumnsFromExprList` over the
            // resolved list: an unaliased column reference (COLLATE
            // skipped; parentheses are no node, unary `+` is) takes
            // its table's own column name — `A` and `s.A` name the
            // column `a` —, a rowid reference `rowid` or the
            // rowid-alias column's name.
            let mut bare = item;
            while let Some(RExpr::Collate(x, _)) = bare {
                bare = Some(&**x);
            }
            if !plan.result_named.as_slice().get(i).copied().unwrap_or(true) {
                match bare {
                    Some(RExpr::Rowid(src)) => {
                        name = copy_vec(b"rowid");
                        if let Some(sp) = src
                            .checked_sub(base.1)
                            .and_then(|k| plan.sources.as_slice().get(k))
                            && let Some(a) = sp.rowid_alias
                            && let Some(n) = self.source_col_name(sp, a)?
                        {
                            name = n;
                        }
                    }
                    Some(RExpr::Column(slot, ..)) => {
                        let mut at = base.0;
                        for sp in plan.sources.iter() {
                            if let Some(k) = slot.checked_sub(at).filter(|k| *k < sp.ncols) {
                                if let Some(n) = self.source_col_name(sp, k)? {
                                    name = n;
                                }
                                break;
                            }
                            at = at.saturating_add(sp.ncols);
                        }
                    }
                    _ => {}
                }
            }
            names.push(name);
        }
        Ok(names)
    }

    /// The name of column `k` of one planned FROM source (v0.16,
    /// CREATE TABLE ... AS SELECT naming): the table's column, or
    /// a derived table's (deduplicated) result column.
    #[cfg(feature = "v16")]
    fn source_col_name(
        &self,
        sp: &crate::query::SubSource,
        k: usize,
    ) -> Result<Option<AVec<u8>>, S::Error> {
        if let Some(d) = &sp.derived {
            let mut names = d.result_names.clone();
            crate::rules::unique_names::<S::Error>(names.as_mut_slice())?;
            return Ok(names.as_slice().get(k).cloned());
        }
        Ok(self
            .catalog
            .tables
            .iter()
            .find(|t| t.root == sp.root)
            .and_then(|t| t.columns.as_slice().get(k))
            .map(|c| copy_vec(handle_bytes(&self.catalog_arena, c.name))))
    }

    /// A table's CHECK / generated / STRICT rules resolved for
    /// one statement (v0.16); `None` for a table with none.
    #[cfg(feature = "v04")]
    fn table_rules(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
    ) -> Result<Option<Box<crate::rules::TableRules>>, S::Error> {
        if !t.has_rules() {
            return Ok(None);
        }
        let ca = &self.catalog_arena;
        let mut specs = AVec::new();
        for c in t.columns.iter() {
            specs.push(crate::rules::ColSpec {
                name: handle_bytes(ca, c.name),
                affinity: c.affinity,
                decl: handle_bytes(ca, c.decl),
                gen_text: c.gen_expr.map(|h| handle_bytes(ca, h)),
                stored: c.stored,
                coll: c.coll,
            });
        }
        let mut checks: AVec<&[u8]> = AVec::new();
        for h in t.checks.iter() {
            checks.push(handle_bytes(ca, *h));
        }
        // `PRAGMA ignore_check_constraints` (v0.16).
        #[cfg(feature = "v16")]
        if self.prag.ignore_checks {
            checks.clear();
        }
        #[cfg(feature = "v16")]
        let strict = t.strict;
        #[cfg(not(feature = "v16"))]
        let strict = false;
        #[allow(unused_mut)]
        let mut rules = crate::rules::plan_rules::<S::Error>(
            arena,
            self.cfg.limits(),
            handle_bytes(ca, t.name),
            specs.as_slice(),
            t.rowid_alias,
            checks.as_slice(),
            strict,
        )?;
        #[cfg(feature = "v16")]
        if let Some(g) = rules.gcols.as_mut() {
            g.case_like = self.env.case_like.get();
        }
        Ok(Some(Box::new(rules)))
    }

    /// One column's DEFAULT value for a statement (v0.16): the
    /// catalog literal, or — for an expression the catalog could
    /// not fold — its evaluation now (reading the clock if it
    /// names one; an unknown function errors here, as on the
    /// pin, only because the default is needed).
    #[cfg(feature = "v04")]
    fn column_default(
        &self,
        arena: &Arena,
        c: &crate::schema::Column,
        clock: &mut crate::rules::Clock,
    ) -> Result<OwnedValue, S::Error> {
        match c.default_expr {
            Some(h) => crate::rules::eval_default::<S::Error>(
                handle_bytes(&self.catalog_arena, h),
                arena,
                self.cfg.limits(),
                clock,
                Some(&self.env),
            ),
            None => Ok(literal_owned(&self.catalog_arena, c.default)),
        }
    }

    /// `column_default` for an INSERT (v0.16 review fix, `v16`):
    /// also the prepared expression when the default reads
    /// connection state, for `InsCol::per_row`.
    #[cfg(feature = "v16")]
    #[allow(clippy::type_complexity)]
    fn column_default_per_row(
        &self,
        arena: &Arena,
        c: &crate::schema::Column,
        clock: &mut crate::rules::Clock,
    ) -> Result<(OwnedValue, Option<(RExpr, RowData)>), S::Error> {
        let Some(h) = c.default_expr else {
            return Ok((literal_owned(&self.catalog_arena, c.default), None));
        };
        let (r, row) = crate::rules::prepare_default::<S::Error>(
            handle_bytes(&self.catalog_arena, h),
            arena,
            self.cfg.limits(),
            clock,
        )?;
        let v = crate::rules::eval_prepared_default::<S::Error>(&r, &row, Some(&self.env))?;
        let per_row = crate::query::any_scalar(&r, &crate::query::ScalarFn::nondeterministic)
            .then_some((r, row));
        Ok((v, per_row))
    }

    /// One FROM source description for resolving an UPDATE/DELETE
    /// statement's expressions (qualifier = alias if given, else
    /// the table name — SQLite's rule, as in plan_query).
    #[cfg(feature = "v05")]
    fn single_source(
        &self,
        t: &crate::schema::TableDef,
        qualifier: &[u8],
    ) -> (AVec<crate::query::SourceDesc>, AVec<usize>) {
        let mut cols = AVec::new();
        for c in t.columns.iter() {
            cols.push((
                copy_vec(handle_bytes(&self.catalog_arena, c.name)),
                c.affinity,
            ));
        }
        let mut sources = AVec::new();
        let mut src = crate::query::SourceDesc::new(copy_vec(qualifier), cols, t.rowid_alias, true);
        src.colls = table_colls(t);
        sources.push(src);
        let mut offsets = AVec::new();
        offsets.push(0);
        (sources, offsets)
    }

    /// Index plans for an UPDATE/DELETE on `tname` (v0.5): the
    /// same resolution INSERT performs — an index hipp cannot
    /// maintain refuses the write.
    #[cfg(all(feature = "v05", feature = "indexes"))]
    fn write_index_plans(&self, tname: &[u8]) -> Result<AVec<IndexPlan>, S::Error> {
        let mut out: AVec<IndexPlan> = AVec::new();
        for ix in self.catalog.indexes_for(&self.catalog_arena, tname) {
            if ix.opaque {
                return Err(Error::Unsupported);
            }
            let mut cols = AVec::new();
            let mut desc = AVec::new();
            for c in ix.columns.iter() {
                let p = IdxColPlan {
                    col: c.col,
                    desc: c.desc,
                    coll: c.coll,
                };
                desc.push(p.order());
                cols.push(p);
            }
            out.push(IndexPlan {
                root: ix.root,
                unique: ix.unique,
                cols,
                desc,
            });
        }
        // Most recently created first, as INSERT's (the pin's order).
        out.as_mut_slice().reverse();
        Ok(out)
    }

    /// The subquery planner handed to `resolve_scoped` (v0.6):
    /// stacks the containing query's scope group onto the
    /// enclosing groups and plans the subquery against the
    /// combined scope. Without the `v06` feature the planner
    /// always fails — the parser never produces subquery nodes
    /// there, so it is never actually called.
    #[cfg(feature = "v04")]
    fn sub_planner<'a>(
        &'a self,
        arena: &'a Arena,
    ) -> impl FnMut(
        &SelectStmt,
        &crate::query::ScopeGroup,
        &[crate::query::ScopeGroup],
    ) -> Result<crate::query::SubPlan, S::Error>
    + 'a {
        #[cfg(feature = "v06")]
        {
            move |stmt: &SelectStmt,
                  cur: &crate::query::ScopeGroup,
                  enclosing: &[crate::query::ScopeGroup]| {
                let mut full: AVec<crate::query::ScopeGroup> = AVec::new();
                full.push(cur.clone());
                for g in enclosing {
                    full.push(g.clone());
                }
                self.plan_subquery(stmt, arena, full.as_slice())
            }
        }
        #[cfg(not(feature = "v06"))]
        {
            let _ = arena;
            move |_: &SelectStmt, _: &crate::query::ScopeGroup, _: &[crate::query::ScopeGroup]| {
                Err(Error::Unsupported)
            }
        }
    }

    /// Resolve one GROUP BY / HAVING / ORDER BY term (v0.4,
    /// scoped in v0.6, per-clause rules v0.16 — the pin's
    /// `resolveOrderGroupBy`):
    ///
    /// - ORDER BY / GROUP BY: an integer literal that fits the
    ///   pin's 32-bit "is integer" test is an output ordinal —
    ///   outside 1..=items it is an error (it used to sort by a
    ///   constant silently). HAVING has no ordinals: `HAVING 1`
    ///   is the constant 1 (it used to read item 1).
    /// - A bare name equal to an item alias: ORDER BY takes the
    ///   alias first; GROUP BY and HAVING only when no source
    ///   of the current scope has a column so named (a real
    ///   column shadows the alias there — the pin groups
    ///   `SELECT a+1 AS b ... GROUP BY b` by the column `b`).
    /// - Anything else resolves as an ordinary expression
    ///   (under `v16` the scope's aliases are visible inside it).
    #[cfg(feature = "v04")]
    #[allow(clippy::too_many_arguments)]
    fn resolve_term(
        &self,
        e: &Expr,
        clause: TermClause,
        arena: &Arena,
        items: &[RExpr],
        aliases: &[Option<AVec<u8>>],
        cur: &crate::query::ScopeGroup,
        enclosing: &[crate::query::ScopeGroup],
        planner: crate::query::SubPlanner<'_, S::Error>,
    ) -> Result<RExpr, S::Error> {
        // `<term> COLLATE name` (v0.16, `v16`): the term's own
        // ordinal / alias rules apply to the operand and the
        // collation stays on top (the pin's `resolveAlias`
        // keeps it), so `ORDER BY 1 COLLATE NOCASE` sorts item 1
        // by NOCASE.
        #[cfg(feature = "v16")]
        if let Expr::Collate(inner, c) = e {
            let r = self.resolve_term(
                inner, clause, arena, items, aliases, cur, enclosing, planner,
            )?;
            return Ok(RExpr::Collate(Box::new(r), *c));
        }
        if clause != TermClause::Having
            && let Expr::Literal(Literal::Integer(n)) = e
            && (-0x7fff_ffff..=0x7fff_ffff).contains(n)
        {
            return usize::try_from(*n)
                .ok()
                .and_then(|k| k.checked_sub(1))
                .and_then(|k| items.get(k))
                .cloned()
                .ok_or(Error::Schema {
                    kind: SchemaKind::TermOutOfRange,
                });
        }
        if let Expr::Column { table: None, name } = e {
            let nb = handle_bytes(arena, *name);
            if let Some(item) = aliases
                .iter()
                .position(|a| a.as_ref().is_some_and(|x| eq_ci(x.as_slice(), nb)))
                .and_then(|idx| items.get(idx))
                && (clause == TermClause::Order
                    || crate::query::resolve_column_in_group::<S::Error>(cur, None, nb).is_none())
            {
                return Ok(item.clone());
            }
        }
        crate::query::resolve_scoped::<S::Error>(e, arena, cur, enclosing, planner)
    }

    /// Stamp a freshly planned subquery with its node id and
    /// its uncorrelated verdict (v0.13): `base_width` /
    /// `base_src` are the enclosing scopes' combined width and
    /// source count — the absolute threshold below which a
    /// reference is bound to an enclosing scope.
    #[cfg(feature = "v06")]
    fn stamp_subplan(&self, plan: &mut crate::query::SubPlan, base_width: usize, base_src: usize) {
        let id = self.next_subplan_id.get();
        // Wrapping past usize::MAX would take 2^64 plans; skip 0,
        // which marks an unstamped plan.
        self.next_subplan_id.set(id.wrapping_add(1).max(1));
        plan.id = id;
        plan.uncorrelated = crate::query::plan_is_uncorrelated(plan, base_width, base_src);
    }

    /// Plan one subquery (v0.6) against the combined scope:
    /// `enclosing` holds the scope groups of every textually
    /// enclosing query, nearest first, with absolute slot/source
    /// numbering into the combined evaluation row. The
    /// subquery's own sources are numbered after all enclosing
    /// sources. Mirrors `plan_query`, including term resolution
    /// and the aggregate-argument subquery rule.
    #[cfg(feature = "v06")]
    fn plan_subquery(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
    ) -> Result<crate::query::SubPlan, S::Error> {
        self.plan_subquery_keyed(stmt, arena, enclosing)
            .map(|(p, _)| p)
    }

    /// `plan_subquery`, also returning the head core's compound
    /// ORDER BY matching data (v0.16; a compound tail core is
    /// planned through this to supply its own).
    #[cfg(feature = "v06")]
    fn plan_subquery_keyed(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
    ) -> Result<(crate::query::SubPlan, CompKeys), S::Error> {
        use crate::query::SubPlan;
        let (base_width, base_src) = crate::query::ScopeGroup::stack_end(enclosing);
        // A VALUES core (v0.9): no sources of its own; the rows
        // resolve against the enclosing scopes alone.
        #[cfg(feature = "v09")]
        if !stmt.values.is_empty() {
            let (values, keys) = self.plan_values(stmt, arena, base_src, enclosing)?;
            let width = keys.names.len();
            let (compound, comp_order) = if stmt.compound.is_empty() {
                (AVec::new(), AVec::new())
            } else {
                self.plan_sub_compound(stmt, arena, enclosing, &keys, width)?
            };
            let mut result_names: AVec<AVec<u8>> = AVec::new();
            let mut result_affs: AVec<Option<crate::record::Affinity>> = AVec::new();
            let mut result_named = AVec::new();
            for n in keys.names.iter() {
                result_names.push(n.clone().unwrap_or_default());
                result_affs.push(None);
                result_named.push(false);
            }
            let mut plan = SubPlan {
                sources: AVec::new(),
                items: AVec::new(),
                filter: None,
                group_by: AVec::new(),
                having: None,
                order_by: AVec::new(),
                distinct: false,
                limit: self.plan_limit(stmt.limit.as_deref(), arena)?,
                offset: self.plan_limit(stmt.offset.as_deref(), arena)?,
                has_agg: false,
                compound,
                values,
                comp_order,
                result_names,
                result_affs,
                result_named,
                id: 0,
                uncorrelated: false,
                scalar_col: 0,
                #[cfg(feature = "v16")]
                rec: None,
                #[cfg(feature = "v16")]
                mat: 0,
            };
            self.stamp_subplan(&mut plan, base_width, base_src);
            return Ok((plan, keys));
        }
        let core = self.plan_core(stmt, arena, enclosing)?;
        #[cfg(feature = "v09")]
        let (compound, comp_order) = if stmt.compound.is_empty() {
            (AVec::new(), AVec::new())
        } else {
            self.plan_sub_compound(stmt, arena, enclosing, &core.keys, core.items.len())?
        };
        #[cfg(not(feature = "v09"))]
        let (compound, comp_order): SubCompoundParts = (AVec::new(), AVec::new());
        let mut plan = SubPlan {
            sources: core.sources,
            items: core.items,
            filter: core.filter,
            group_by: core.group_by,
            having: core.having,
            order_by: core.order_by,
            distinct: stmt.distinct,
            limit: core.limit,
            offset: core.offset,
            has_agg: core.has_agg,
            compound,
            values: AVec::new(),
            comp_order,
            result_names: core.result_names,
            result_affs: core.result_affs,
            result_named: core.result_named,
            id: 0,
            uncorrelated: false,
            scalar_col: 0,
            #[cfg(feature = "v16")]
            rec: None,
            #[cfg(feature = "v16")]
            mat: 0,
        };
        // A compound's cores sort by its ORDER BY (and, under
        // UNION / INTERSECT / EXCEPT, every column) on the pin,
        // which copies their window columns (v0.16 review fix,
        // `window`; see `window::compound_sorts`).
        #[cfg(feature = "window")]
        if !plan.compound.is_empty() {
            let mut ops = AVec::new();
            for (op, _) in plan.compound.iter() {
                ops.push(*op);
            }
            let mut cols = AVec::new();
            for (j, _) in plan.comp_order.iter() {
                cols.push(*j);
            }
            let sorts =
                crate::window::compound_sorts(ops.as_slice(), cols.as_slice(), plan.items.len());
            let mut it = sorts.iter();
            if let Some(c) = it.next() {
                crate::window::compound_copies(
                    plan.items.as_mut_slice(),
                    &mut plan.order_by,
                    c.as_slice(),
                    plan.distinct,
                );
            }
            for ((_, t), c) in plan.compound.as_mut_slice().iter_mut().zip(it) {
                crate::window::compound_copies(
                    t.items.as_mut_slice(),
                    &mut t.order_by,
                    c.as_slice(),
                    t.distinct,
                );
            }
        }
        self.stamp_subplan(&mut plan, base_width, base_src);
        Ok((plan, core.keys))
    }

    /// Resolve a VALUES core's rows (v0.9; shared by top-level
    /// and subquery planning since v0.16) against `enclosing`,
    /// with its compound ORDER BY keys: the `column1`, ...
    /// names, no FROM scope.
    #[cfg(feature = "v09")]
    fn plan_values(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        base_src: usize,
        enclosing: &[crate::query::ScopeGroup],
    ) -> Result<(AVec<AVec<RExpr>>, CompKeys), S::Error> {
        use crate::query::{ScopeGroup, has_agg, resolve_scoped};
        let cur = ScopeGroup::new(AVec::new(), AVec::new(), base_src);
        let mut planner = self.sub_planner(arena);
        let mut rows_r: AVec<AVec<RExpr>> = AVec::new();
        let mut width: Option<usize> = None;
        for r in stmt.values.iter() {
            match width {
                None => width = Some(r.len()),
                Some(w) if w != r.len() => {
                    return Err(Error::Schema {
                        kind: SchemaKind::ColumnCountMismatch,
                    });
                }
                _ => {}
            }
            let mut rr = AVec::new();
            // Each VALUES row is a SELECT core of its own on the pin:
            // its window calls (v0.16, `window`) run over that one
            // row.
            #[cfg(feature = "window")]
            {
                cur.win.ok.set(true);
                cur.win.next.set(0);
            }
            for e in r.iter() {
                let re = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                if has_agg(&re) {
                    return Err(Error::Unsupported);
                }
                rr.push(re);
            }
            #[cfg(feature = "window")]
            {
                cur.win.ok.set(false);
                crate::window::classify(rr.as_mut_slice(), &mut []);
            }
            rows_r.push(rr);
        }
        let mut names = AVec::new();
        for i in 0..width.unwrap_or(0) {
            names.push(Some(values_col_name(i)));
        }
        let colls = core_colls(&[], rows_r.as_slice());
        Ok((
            rows_r,
            CompKeys {
                names,
                items: AVec::new(),
                scope: cur,
                colls,
            },
        ))
    }

    /// Plan one subquery statement's compound tail (v0.9):
    /// each tail core plans against the same enclosing scopes;
    /// all cores must match the head's width (the pin: "SELECTs
    /// to the left and right of UNION do not have the same
    /// number of result columns"). The statement's ORDER BY
    /// resolves to output-column indices over every core's
    /// `CompKeys`, head first.
    #[cfg(feature = "v09")]
    fn plan_sub_compound(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
        head: &CompKeys,
        width: usize,
    ) -> Result<SubCompoundParts, S::Error> {
        let mut tails: AVec<(crate::sql::CompoundOp, Box<crate::query::SubPlan>)> = AVec::new();
        let mut keys: AVec<CompKeys> = AVec::new();
        for (op, core) in stmt.compound.iter() {
            let (p, k) = self.plan_subquery_keyed(core, arena, enclosing)?;
            if p.width() != width {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            tails.push((*op, Box::new(p)));
            keys.push(k);
        }
        let comp_order = resolve_compound_order::<S::Error>(
            stmt.order_by.as_slice(),
            arena,
            head,
            keys.as_slice(),
            width,
        )?;
        Ok((tails, comp_order))
    }

    /// Plan one top-level statement's compound tail (v0.9):
    /// the `plan_sub_compound` mirror for `QueryPlan`.
    #[cfg(feature = "v09")]
    fn plan_query_compound(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        head: &CompKeys,
        width: usize,
    ) -> Result<QueryCompoundParts, S::Error> {
        let mut tails: AVec<(crate::sql::CompoundOp, Box<QueryPlan>)> = AVec::new();
        let mut keys: AVec<CompKeys> = AVec::new();
        for (op, core) in stmt.compound.iter() {
            let (p, k) = self.plan_query_keyed(core, arena)?;
            if p.width != width {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            tails.push((*op, Box::new(p)));
            keys.push(k);
        }
        let comp_order = resolve_compound_order::<S::Error>(
            stmt.order_by.as_slice(),
            arena,
            head,
            keys.as_slice(),
            width,
        )?;
        Ok((tails, comp_order))
    }

    /// Resolve a `RETURNING` item list (v0.7) against one table:
    /// `*` expands to the table's columns, expressions resolve
    /// in the table's single-source scope (subqueries plan
    /// against it as their enclosing scope), and aggregates are
    /// refused — the oracle calls them a misuse in RETURNING.
    #[cfg(feature = "v07")]
    fn plan_returning(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        qualifier: &[u8],
        items: &AVec<SelectItem>,
    ) -> Result<AVec<RExpr>, S::Error> {
        let (sources, offsets) = self.single_source(t, qualifier);
        self.plan_returning_in(arena, sources, offsets, items)
    }

    /// `plan_returning` over the given source (v0.16: also a view's
    /// columns, for a write through INSTEAD OF triggers).
    #[cfg(feature = "v07")]
    pub(super) fn plan_returning_in(
        &self,
        arena: &Arena,
        sources: AVec<crate::query::SourceDesc>,
        offsets: AVec<usize>,
        items: &AVec<SelectItem>,
    ) -> Result<AVec<RExpr>, S::Error> {
        use crate::query::{has_agg, resolve_scoped};
        let mut cur = crate::query::ScopeGroup::new(sources, offsets, 0);
        cur.agg_owner = false;
        let mut planner = self.sub_planner(arena);
        let mut out = AVec::new();
        for it in items.iter() {
            match it {
                SelectItem::Star => {
                    let src = &cur.sources.as_slice()[0];
                    for ci in 0..src.columns.len() {
                        out.push(src.column_ref(&cur, 0, ci));
                    }
                }
                // The pin refuses `t.*` in RETURNING (v0.16 review
                // fix: hipp used to expand it).
                SelectItem::TableStar(_) => {
                    return Err(Error::Schema {
                        kind: SchemaKind::ReturningTableStar,
                    });
                }
                SelectItem::Expr(e, _, _) => {
                    let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                    if has_agg(&r) {
                        return Err(Error::Unsupported);
                    }
                    out.push(r);
                }
            }
        }
        // The pin's "too many columns in result set" (v0.16 review
        // fix, ungated): a RETURNING list is a result set, counted
        // once `*` is expanded and checked before any row changes.
        if out.len() > crate::sql::parser::MAX_COLUMN {
            return Err(Error::Schema {
                kind: SchemaKind::TooManyColumns,
            });
        }
        Ok(out)
    }

    /// Plan an upsert clause (v0.7): resolve and validate the
    /// conflict target, then resolve the DO UPDATE assignments
    /// and filter against (original row, excluded row) scopes.
    #[cfg(feature = "v07")]
    fn plan_upsert(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        tname: &[u8],
        upsert: &crate::sql::Upsert,
    ) -> Result<UpsertPlan, S::Error> {
        use crate::query::{ScopeGroup, SourceDesc, has_agg, resolve_scoped};
        if let Some(tf) = &upsert.target_filter {
            // Oracle behaviour, probed in v0.7 (pinned 3.54.1):
            // the conflict-target WHERE is name-resolved (an
            // unknown column errors) but never evaluated for a
            // non-partial index — WHERE 0 arbitrates exactly like
            // WHERE 1. hipp creates no partial indexes (CREATE
            // INDEX ... WHERE is refused), so resolve for
            // validation and then drop the predicate.
            let (sources, offsets) = self.single_source(t, tname);
            let cur = ScopeGroup::new(sources, offsets, 0);
            let mut planner = self.sub_planner(arena);
            let r = resolve_scoped::<S::Error>(tf, arena, &cur, &[], &mut planner)?;
            if has_agg(&r) {
                return Err(Error::Unsupported);
            }
        }
        // Canonical target set: the rowid-alias column and the
        // rowid names all canonicalise to Target::Rowid.
        let canon = |this: &Self, name: &[u8]| -> Result<Target, S::Error> {
            match this.resolve_target(t, name)? {
                Target::Col(i) if Some(i) == t.rowid_alias => Ok(Target::Rowid),
                other => Ok(other),
            }
        };
        let target = if upsert.target.is_empty() {
            None
        } else {
            let mut cols = AVec::new();
            for n in upsert.target.iter() {
                cols.push(canon(self, handle_bytes(arena, *n))?);
            }
            // The target must name the rowid / primary key or
            // exactly the key columns of one unique index (the
            // oracle: "ON CONFLICT clause does not match any
            // PRIMARY KEY or index" otherwise).
            let is_rowid = cols.len() == 1 && cols.as_slice()[0] == Target::Rowid;
            let mut matches_index = false;
            if !is_rowid {
                for ix in self.catalog.indexes_for(&self.catalog_arena, tname) {
                    if !ix.unique || ix.opaque {
                        continue;
                    }
                    let mut keyset = AVec::new();
                    for c in ix.columns.iter() {
                        keyset.push((
                            match c.col {
                                None => Target::Rowid,
                                Some(i) if Some(i) == t.rowid_alias => Target::Rowid,
                                Some(i) => Target::Col(i),
                            },
                            c.coll,
                        ));
                    }
                    if keyset_matches(
                        cols.as_slice(),
                        upsert.target_colls.as_slice(),
                        keyset.as_slice(),
                    ) {
                        matches_index = true;
                        break;
                    }
                }
            }
            if !is_rowid && !matches_index {
                return Err(Error::Unsupported);
            }
            Some(cols)
        };
        let (do_update, sets, filter) = match &upsert.action {
            crate::sql::UpsertAction::Nothing => (false, AVec::new(), None),
            crate::sql::UpsertAction::Update {
                assignments,
                filter: ufilter,
            } => {
                // Scope: the original row's group is current (so
                // bare names bind it); the proposed row sits in an
                // enclosing group under the qualifier `excluded`,
                // reached by qualified references only.
                let (sources, offsets) = self.single_source(t, tname);
                let cur = ScopeGroup::new(sources, offsets, 0);
                // The pin reads `excluded.<col>` as a register
                // reference: no column affinity and no column
                // collation (BINARY) in comparisons, whatever the
                // target column declares.
                let mut ex_cols = AVec::new();
                for c in t.columns.iter() {
                    ex_cols.push((
                        copy_vec(handle_bytes(&self.catalog_arena, c.name)),
                        crate::record::Affinity::Blob,
                    ));
                }
                let mut ex_sources = AVec::new();
                let mut ex = SourceDesc::new(copy_vec(b"excluded"), ex_cols, t.rowid_alias, true);
                ex.registers = true;
                ex_sources.push(ex);
                let mut ex_offsets = AVec::new();
                ex_offsets.push(t.columns.len());
                let mut ex_group = ScopeGroup::new(ex_sources, ex_offsets, 1);
                ex_group.agg_owner = false;
                let enclosing = [ex_group];
                let mut planner = self.sub_planner(arena);
                let mut sets = AVec::new();
                for (colname, e) in assignments.iter() {
                    let target = canon(self, handle_bytes(arena, *colname))?;
                    if matches!(target, Target::Col(i) if t.columns.as_slice()[i].gen_expr.is_some())
                    {
                        return Err(Error::Schema {
                            kind: SchemaKind::GeneratedColumnWrite,
                        });
                    }
                    let r = resolve_scoped::<S::Error>(
                        e,
                        arena,
                        &cur,
                        enclosing.as_slice(),
                        &mut planner,
                    )?;
                    if has_agg(&r) {
                        return Err(Error::Unsupported);
                    }
                    sets.push((target, r));
                }
                let filter = match ufilter {
                    None => None,
                    Some(e) => {
                        let r = resolve_scoped::<S::Error>(
                            e,
                            arena,
                            &cur,
                            enclosing.as_slice(),
                            &mut planner,
                        )?;
                        if has_agg(&r) {
                            return Err(Error::Unsupported);
                        }
                        Some(r)
                    }
                };
                (true, sets, filter)
            }
        };
        Ok(UpsertPlan {
            target,
            target_colls: upsert.target_colls.clone(),
            do_update,
            sets,
            filter,
        })
    }

    /// Plan an UPDATE (v0.5; FROM and RETURNING are v0.7):
    /// resolve the SET expressions and the filter once, at
    /// prepare time, against target ++ FROM sources; aggregates
    /// are not legal in either (SQLite agrees). RETURNING
    /// resolves against the target table only.
    #[cfg(feature = "v05")]
    #[allow(clippy::too_many_arguments)]
    fn plan_update(
        &self,
        arena: &Arena,
        table: &crate::arena::ABytes,
        alias: &Option<crate::arena::ABytes>,
        conflict: ConflictAction,
        assignments: &AVec<(crate::arena::ABytes, Expr)>,
        from: &AVec<crate::sql::FromSource>,
        filter: &Option<Expr>,
        returning: &AVec<SelectItem>,
    ) -> Result<Plan, S::Error> {
        use crate::query::{has_agg, resolve_scoped};
        #[cfg(not(feature = "v07"))]
        let _ = returning;
        let tname = handle_bytes(arena, *table);
        let t = self.write_table(arena, table)?;
        let qualifier: &[u8] = match alias {
            Some(a) => handle_bytes(arena, *a),
            None => tname,
        };
        // Combined scope: target first, then the FROM sources
        // (empty without the v07 parser, which never fills
        // `from`); SourcePlans are kept for the executor. The
        // FROM clause is its own join (v0.16, as on the pin,
        // which plans it like a subquery): its USING / NATURAL
        // columns and ON clauses see only the FROM sources —
        // `... FROM a JOIN b ON t.id = b.y` is "no such column"
        // there — and a RIGHT / FULL join's unmatched rows
        // NULL-pad the FROM sources only, never the target.
        let (mut sources, mut offsets) = self.single_source(t, qualifier);
        let mut from_plans: AVec<SourcePlan> = AVec::new();
        let mut fdescs = AVec::new();
        let mut foffsets = AVec::new();
        self.plan_from(
            from.as_slice(),
            arena,
            &[],
            t.columns.len(),
            &mut fdescs,
            &mut foffsets,
            &mut from_plans,
        )?;
        // The pin refuses a FROM item that repeats the target
        // item: the same table under the same alias, or both
        // unaliased (`UPDATE t ... FROM t`, `UPDATE t AS x ...
        // FROM t AS x`; `FROM t AS t` and `FROM t AS t2` are
        // accepted). Review finding: hipp ran it as a self-join.
        let alias_b = alias.map(|a| handle_bytes(arena, a));
        if from.iter().any(|s| {
            s.derived.is_none()
                && eq_ci(handle_bytes(arena, s.table), tname)
                && match (s.alias.map(|a| handle_bytes(arena, a)), alias_b) {
                    (None, None) => true,
                    (Some(x), Some(y)) => eq_ci(x, y),
                    _ => false,
                }
        }) {
            return Err(Error::Schema {
                kind: SchemaKind::UpdateTargetInFrom,
            });
        }
        let mut fscope = crate::query::ScopeGroup::new(fdescs, foffsets, 1);
        let using = plan_using::<S::Error>(from.as_slice(), arena, &mut fscope)?;
        let mut planner = self.sub_planner(arena);
        let hoisted = self.plan_on(
            from.as_slice(),
            arena,
            &fscope,
            &[],
            from_plans.as_mut_slice(),
            using,
            &mut planner,
        )?;
        for (d, o) in fscope.sources.iter().zip(fscope.offsets.iter()) {
            sources.push(d.clone());
            offsets.push(*o);
        }
        let cur = crate::query::ScopeGroup::new(sources, offsets, 0);
        let mut sets = AVec::new();
        for (colname, e) in assignments.iter() {
            let target = self.resolve_target(t, handle_bytes(arena, *colname))?;
            // v0.16: a generated column is never assigned.
            if matches!(target, Target::Col(i) if t.columns.as_slice()[i].gen_expr.is_some()) {
                return Err(Error::Schema {
                    kind: SchemaKind::GeneratedColumnWrite,
                });
            }
            let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
            if has_agg(&r) {
                return Err(Error::Unsupported);
            }
            sets.push((target, r));
        }
        let filter = match filter {
            None => hoisted,
            Some(e) => {
                let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                Some(and_expr(hoisted, r))
            }
        };
        let mut defaults = AVec::new();
        let mut affs = AVec::new();
        let mut not_null = AVec::new();
        let mut clock = crate::rules::Clock::new(self.cfg.clock);
        for c in t.columns.iter() {
            // v0.16: an unfolded expression DEFAULT matters to an
            // UPDATE only as OR REPLACE's NOT NULL substitute.
            defaults.push(
                if c.default_expr.is_some() && c.not_null && conflict == ConflictAction::Replace {
                    self.column_default(arena, c, &mut clock)?
                } else {
                    read_default(&self.catalog_arena, c)
                },
            );
            affs.push(c.affinity);
            not_null.push(c.not_null);
        }
        #[cfg(feature = "v07")]
        let returning_r = self.plan_returning(arena, t, qualifier, returning)?;
        Ok(Plan::Update {
            root: t.root,
            rowid_alias: t.rowid_alias,
            ncols: t.columns.len(),
            defaults,
            affs,
            not_null,
            sets,
            filter,
            conflict,
            #[cfg(feature = "v07")]
            from_sources: from_plans,
            #[cfg(feature = "v07")]
            returning: returning_r,
            #[cfg(feature = "indexes")]
            indexes: self.write_index_plans(tname)?,
            rules: self.table_rules(arena, t)?,
        })
    }

    /// Plan a DELETE (v0.5; RETURNING is v0.7).
    #[cfg(feature = "v05")]
    fn plan_delete(
        &self,
        arena: &Arena,
        table: &crate::arena::ABytes,
        filter: &Option<Expr>,
        returning: &AVec<SelectItem>,
    ) -> Result<Plan, S::Error> {
        use crate::query::{has_agg, resolve_scoped};
        #[cfg(not(feature = "v07"))]
        let _ = returning;
        let tname = handle_bytes(arena, *table);
        let t = self.write_table(arena, table)?;
        let (sources, offsets) = self.single_source(t, tname);
        let cur = crate::query::ScopeGroup::new(sources, offsets, 0);
        let mut planner = self.sub_planner(arena);
        let filter = match filter {
            None => None,
            Some(e) => {
                let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                Some(r)
            }
        };
        let mut defaults = AVec::new();
        let mut affs = AVec::new();
        for c in t.columns.iter() {
            defaults.push(read_default(&self.catalog_arena, c));
            affs.push(c.affinity);
        }
        #[cfg(feature = "v07")]
        let returning_r = self.plan_returning(arena, t, tname, returning)?;
        Ok(Plan::Delete {
            root: t.root,
            rowid_alias: t.rowid_alias,
            ncols: t.columns.len(),
            defaults,
            affs,
            filter,
            #[cfg(feature = "v07")]
            returning: returning_r,
            #[cfg(feature = "indexes")]
            indexes: self.write_index_plans(tname)?,
            rules: if t.has_generated() {
                self.table_rules(arena, t)?
            } else {
                None
            },
        })
    }

    /// Evaluate one INSERT VALUES expression at plan time
    /// (v0.4: constant expressions; pre-v0.4: plain literals).
    /// Superseded under `v08` by the resolved-expression fold in
    /// the INSERT planner (which needs the resolved form to
    /// detect subqueries), so it only compiles there.
    #[cfg(not(feature = "v08"))]
    fn eval_value_expr(&self, arena: &Arena, e: &Expr) -> Result<OwnedValue, S::Error> {
        #[cfg(feature = "v04")]
        {
            crate::query::eval_const::<S::Error>(e, arena)
        }
        #[cfg(not(feature = "v04"))]
        {
            match e {
                Expr::Literal(l) => Ok(literal_owned(arena, Some(*l))),
                _ => Err(Error::Unsupported),
            }
        }
    }

    /// SELECT planning (v0.4): the legacy fast paths (full scan /
    /// rowid seek / index lookup, zero materialisation) serve the
    /// simple single-table shape; anything richer goes to the
    /// general query planner when the `v04` feature is on. A
    /// simple shape whose only legacy failure is `Unsupported`
    /// (a general WHERE predicate) also falls through to it.
    fn plan_select(&self, stmt: &SelectStmt, arena: &Arena) -> Result<Plan, S::Error> {
        if select_is_simple(stmt) {
            match self.plan_legacy_select(stmt, arena) {
                Ok(p) => return Ok(p),
                Err(e) => {
                    if !matches!(e, Error::Unsupported) {
                        return Err(e);
                    }
                    #[cfg(not(feature = "v04"))]
                    {
                        return Err(e);
                    }
                }
            }
        }
        #[cfg(feature = "v04")]
        {
            Ok(Plan::Query(self.plan_query(stmt, arena)?))
        }
        #[cfg(not(feature = "v04"))]
        {
            Err(Error::Unsupported)
        }
    }

    /// The pre-v0.4 SELECT planner: single table, `*` or plain
    /// columns, optional `WHERE <col> = <literal>`.
    fn plan_legacy_select(&self, stmt: &SelectStmt, arena: &Arena) -> Result<Plan, S::Error> {
        let tname = handle_bytes(arena, stmt.from.as_slice()[0].table);
        let t = match self.catalog.find(&self.catalog_arena, tname) {
            Some(t) => t.usable()?,
            // v0.16: `sqlite_schema` and the table-valued pragma
            // functions read through the general planner.
            #[cfg(feature = "v16")]
            None if crate::schema::Catalog::is_schema_name(tname)
                || tname.get(..7).is_some_and(|p| eq_ci(p, b"pragma_"))
                || tname.get(..11).is_some_and(|p| eq_ci(p, b"sqlite_temp")) =>
            {
                return Err(Error::Unsupported);
            }
            None => {
                if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                    // Views are inert in v0.4 (SPEC §2).
                    return Err(Error::Unsupported);
                }
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
        };
        // v0.16: a table with generated columns reads through
        // the general planner, which computes VIRTUAL columns.
        #[cfg(feature = "v04")]
        if t.has_generated() {
            return Err(Error::Unsupported);
        }
        let predicate: Option<(crate::arena::ABytes, Literal)> = match &stmt.filter {
            None => None,
            Some(Expr::Binary(BinOp::Eq, l, r)) => match (l.as_ref(), r.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(lit)) => Some((*name, *lit)),
                _ => None,
            },
            _ => None,
        };
        // WHERE <col> = <literal> lowers to a rowid access
        // path when <col> is the rowid or its alias; any
        // other real column is beyond v0.2's executor.
        let access = match predicate {
            None => Access::FullScan,
            Some((colname, lit)) => {
                let nb = handle_bytes(arena, colname);
                match self.resolve_target(t, nb)? {
                    Target::Rowid => literal_rowid(arena, lit),
                    Target::Col(i) if Some(i) == t.rowid_alias => literal_rowid(arena, lit),
                    Target::Col(i) => {
                        // v0.3: an index on this column
                        // turns the predicate into a
                        // secondary-index lookup.
                        #[cfg(feature = "indexes")]
                        {
                            match self.plan_index_access(t, tname, i, arena, lit) {
                                Some(a) => a,
                                None => return Err(Error::Unsupported),
                            }
                        }
                        #[cfg(not(feature = "indexes"))]
                        {
                            let _ = i;
                            return Err(Error::Unsupported);
                        }
                    }
                }
            }
        };
        let mut out = AVec::new();
        let mut aff = AVec::new();
        if stmt.items.len() == 1 && matches!(stmt.items.as_slice()[0], SelectItem::Star) {
            for (i, c) in t.columns.iter().enumerate() {
                out.push(i);
                aff.push(c.affinity);
            }
        } else {
            for it in stmt.items.iter() {
                let SelectItem::Expr(Expr::Column { name, .. }, _, _) = it else {
                    return Err(Error::Unsupported);
                };
                let nb = handle_bytes(arena, *name);
                match self.resolve_target(t, nb)? {
                    Target::Col(i) => {
                        out.push(i);
                        aff.push(t.columns.as_slice()[i].affinity);
                    }
                    Target::Rowid => {
                        out.push(ROWID_COL);
                        aff.push(crate::record::Affinity::Integer);
                    }
                }
            }
        }
        let mut defaults = AVec::new();
        for c in t.columns.iter() {
            defaults.push(read_default(&self.catalog_arena, c));
        }
        Ok(Plan::Scan {
            root: t.root,
            rowid_alias: t.rowid_alias,
            ncols: t.columns.len(),
            out,
            aff,
            defaults,
            access,
        })
    }

    /// Width of a SELECT's result (for CREATE VIEW validation).
    #[cfg(all(feature = "v04", not(feature = "v16")))]
    fn view_select_width(&self, stmt: &SelectStmt, arena: &Arena) -> Result<usize, S::Error> {
        match self.plan_select(stmt, arena)? {
            Plan::Scan { out, .. } => Ok(out.len()),
            Plan::Query(q) => Ok(q.width),
            _ => Err(Error::Unsupported),
        }
    }

    /// The v0.4 general query planner: resolve every expression
    /// against the FROM sources once, at prepare time.
    #[cfg(feature = "v04")]
    fn plan_query(&self, stmt: &SelectStmt, arena: &Arena) -> Result<QueryPlan, S::Error> {
        self.plan_query_keyed(stmt, arena).map(|(p, _)| p)
    }

    /// `plan_query`, also returning the head core's compound
    /// ORDER BY matching data (v0.16; see `plan_subquery_keyed`).
    #[cfg(feature = "v04")]
    fn plan_query_keyed(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
    ) -> Result<(QueryPlan, CompKeys), S::Error> {
        // A VALUES core (v0.9): no sources; rows resolve
        // against the empty scope.
        #[cfg(feature = "v09")]
        if !stmt.values.is_empty() {
            let (values, keys) = self.plan_values(stmt, arena, 0, &[])?;
            let width = keys.names.len();
            let (compound, comp_order) = if stmt.compound.is_empty() {
                (AVec::new(), AVec::new())
            } else {
                self.plan_query_compound(stmt, arena, &keys, width)?
            };
            let qp = QueryPlan {
                sources: AVec::new(),
                items: AVec::new(),
                filter: None,
                group_by: AVec::new(),
                having: None,
                order_by: AVec::new(),
                distinct: false,
                limit: self.plan_limit(stmt.limit.as_deref(), arena)?,
                offset: self.plan_limit(stmt.offset.as_deref(), arena)?,
                has_agg: false,
                width,
                compound,
                values,
                comp_order,
            };
            return Ok((qp, keys));
        }
        let core = self.plan_core(stmt, arena, &[])?;
        let width = core.items.len();
        #[cfg(feature = "v09")]
        let (compound, comp_order) = if stmt.compound.is_empty() {
            (AVec::new(), AVec::new())
        } else {
            self.plan_query_compound(stmt, arena, &core.keys, width)?
        };
        #[cfg(not(feature = "v09"))]
        let (compound, comp_order): QueryCompoundParts = (AVec::new(), AVec::new());
        #[cfg_attr(not(feature = "window"), allow(unused_mut))]
        let mut plan = QueryPlan {
            sources: core.sources,
            items: core.items,
            filter: core.filter,
            group_by: core.group_by,
            having: core.having,
            order_by: core.order_by,
            distinct: stmt.distinct,
            limit: core.limit,
            offset: core.offset,
            has_agg: core.has_agg,
            width,
            compound,
            values: AVec::new(),
            comp_order,
        };
        // A compound's cores sort by its ORDER BY (and, under
        // UNION / INTERSECT / EXCEPT, every column) on the pin,
        // which copies their window columns (v0.16 review fix,
        // `window`; see `window::compound_sorts`).
        #[cfg(feature = "window")]
        if !plan.compound.is_empty() {
            let mut ops = AVec::new();
            for (op, _) in plan.compound.iter() {
                ops.push(*op);
            }
            let mut cols = AVec::new();
            for (j, _) in plan.comp_order.iter() {
                cols.push(*j);
            }
            let sorts =
                crate::window::compound_sorts(ops.as_slice(), cols.as_slice(), plan.items.len());
            let mut it = sorts.iter();
            if let Some(c) = it.next() {
                crate::window::compound_copies(
                    plan.items.as_mut_slice(),
                    &mut plan.order_by,
                    c.as_slice(),
                    plan.distinct,
                );
            }
            for ((_, t), c) in plan.compound.as_mut_slice().iter_mut().zip(it) {
                crate::window::compound_copies(
                    t.items.as_mut_slice(),
                    &mut t.order_by,
                    c.as_slice(),
                    t.distinct,
                );
            }
        }
        Ok((plan, core.keys))
    }

    /// The shared core planner (v0.16: the common body of
    /// `plan_query` and `plan_subquery`, formerly duplicated):
    /// FROM sources and their join constraints, result items,
    /// WHERE, GROUP BY, HAVING, ORDER BY and LIMIT / OFFSET of
    /// one SELECT core. The core's sources are numbered after
    /// every slot / source of `enclosing` (nearest first);
    /// derived tables plan against `enclosing` only — SQLite has
    /// no LATERAL, so a FROM subquery never sees its siblings.
    #[cfg(feature = "v04")]
    fn plan_core(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
    ) -> Result<CorePlan, S::Error> {
        use crate::query::{ScopeGroup, has_agg, item_result_affinity, resolve_scoped};
        let (base_width, base_src) = ScopeGroup::stack_end(enclosing);
        let mut descs = AVec::new();
        let mut offsets = AVec::new();
        let mut sources: AVec<SourcePlan> = AVec::new();
        self.plan_from(
            stmt.from.as_slice(),
            arena,
            enclosing,
            base_width,
            &mut descs,
            &mut offsets,
            &mut sources,
        )?;
        let mut cur = ScopeGroup::new(descs, offsets, base_src);
        // v0.16 (`window`): the core's WINDOW clause, and window
        // calls allowed in its result list.
        #[cfg(feature = "window")]
        {
            cur.win.defs = stmt.windows.clone();
            cur.win.ok.set(true);
        }
        let using = plan_using::<S::Error>(stmt.from.as_slice(), arena, &mut cur)?;
        let mut planner = self.sub_planner(arena);
        // Items, with star expansion (USING-aware, v0.16);
        // aliases are remembered for term resolution.
        let mut items: AVec<RExpr> = AVec::new();
        let mut item_aliases: AVec<Option<AVec<u8>>> = AVec::new();
        let mut result_names: AVec<AVec<u8>> = AVec::new();
        let mut result_affs: AVec<Option<crate::record::Affinity>> = AVec::new();
        let mut result_named: AVec<bool> = AVec::new();
        for it in stmt.items.iter() {
            let qual = match it {
                SelectItem::Expr(e, alias, text) => {
                    let r = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                    result_names.push(item_name(arena, e, alias, text));
                    result_affs.push(item_result_affinity(&r));
                    result_named.push(alias.is_some());
                    items.push(r);
                    item_aliases.push(alias.map(|a| copy_vec(handle_bytes(arena, a))));
                    continue;
                }
                SelectItem::Star => None,
                SelectItem::TableStar(q) => Some(handle_bytes(arena, *q)),
            };
            // `*` over no FROM clause (v0.16 review fix: one row of
            // no columns; the pin's "no tables specified").
            if qual.is_none() && cur.sources.is_empty() {
                return Err(Error::Schema {
                    kind: SchemaKind::NoTablesSpecified,
                });
            }
            #[cfg(feature = "v16")]
            if let SelectItem::TableStar(q) = it {
                crate::query::trace_star(&cur, *q, arena);
            }
            let mut expanded = AVec::new();
            let hit = crate::query::expand_star::<S::Error>(&cur, qual, &mut expanded)?;
            if !hit && qual.is_some() {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownColumn,
                });
            }
            for (r, nm) in expanded.iter() {
                result_affs.push(item_result_affinity(r));
                result_named.push(false);
                items.push(r.clone());
                item_aliases.push(None);
                result_names.push(nm.clone());
            }
        }
        #[cfg(feature = "window")]
        cur.win.ok.set(false);
        // The pin's "too many columns in result set" once `*` is
        // expanded (v0.16 fix, ungated; the parser counts the
        // written items).
        if items.len() > crate::sql::parser::MAX_COLUMN {
            return Err(Error::Schema {
                kind: SchemaKind::TooManyColumns,
            });
        }
        // The pin's "HAVING clause on a non-aggregate query":
        // HAVING needs a GROUP BY or an aggregate in the result
        // list (aggregates elsewhere do not count).
        if stmt.having.is_some()
            && stmt.group_by.is_empty()
            && !items.iter().any(crate::query::has_result_agg)
        {
            return Err(Error::Schema {
                kind: SchemaKind::HavingWithoutAggregate,
            });
        }
        // v16: result-column aliases become visible to every
        // clause resolved below (and to subqueries inside them),
        // after the current scope's own columns (the pin's
        // `lookupName` order). The result list above never sees
        // them — there an alias is "no such column" on the pin.
        #[cfg(feature = "v16")]
        for (a, r) in item_aliases.iter().zip(items.iter()) {
            if let Some(a) = a {
                cur.aliases.push((a.clone(), r.clone()));
            }
        }
        let hoisted = self.plan_on(
            stmt.from.as_slice(),
            arena,
            &cur,
            enclosing,
            sources.as_mut_slice(),
            using,
            &mut planner,
        )?;
        let filter = match &stmt.filter {
            None => hoisted,
            Some(e) => {
                let r = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                // None of this query's aggregates, and none of an
                // enclosing query's unless this query aggregates
                // (v0.16: the pin's "misuse of aggregate").
                let aggregates =
                    !stmt.group_by.is_empty() || items.iter().any(crate::query::has_result_agg);
                if crate::query::where_agg(&r, aggregates) {
                    return Err(Error::Schema {
                        kind: SchemaKind::MisusedAggregate,
                    });
                }
                Some(and_expr(hoisted, r))
            }
        };
        // GROUP BY / HAVING / ORDER BY terms: an output ordinal
        // or alias substitutes the item expression itself
        // (SQLite's rule, per clause — see `resolve_term`), so
        // evaluation never needs an output-row context.
        let mut group_by = AVec::new();
        for e in stmt.group_by.iter() {
            let r = self.resolve_term(
                e,
                TermClause::Group,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                enclosing,
                &mut planner,
            )?;
            // A window call there (through an ordinal or alias):
            // the pin's "misuse of window function" (v0.16,
            // `window`).
            #[cfg(feature = "window")]
            if crate::window::has_win(&r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedWindow,
                });
            }
            // The pin: "aggregate functions are not allowed in
            // the GROUP BY clause" (directly, or through an
            // ordinal / alias naming an aggregate item).
            if has_agg(&r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedAggregate,
                });
            }
            group_by.push(r);
        }
        let having = match &stmt.having {
            None => None,
            Some(e) => Some(self.resolve_term(
                e,
                TermClause::Having,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                enclosing,
                &mut planner,
            )?),
        };
        #[cfg(feature = "window")]
        if having.as_ref().is_some_and(crate::window::has_win) {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedWindow,
            });
        }
        // A compound head's ORDER BY resolves against the
        // combined result (comp_order), not this core's rows,
        // so the term loop is skipped for it.
        let mut order_by: AVec<(RExpr, crate::sql::SortOrder)> = AVec::new();
        // Which result column each ORDER BY term names, if any
        // (v0.16 review fix, `window`; see `window::order_copies`).
        #[cfg(feature = "window")]
        let mut order_refs: AVec<Option<usize>> = AVec::new();
        if stmt.compound.is_empty() {
            // Window calls are allowed in ORDER BY terms too.
            #[cfg(feature = "window")]
            cur.win.ok.set(true);
            for (e, dir) in stmt.order_by.iter() {
                let r = self.resolve_term(
                    e,
                    TermClause::Order,
                    arena,
                    items.as_slice(),
                    item_aliases.as_slice(),
                    &cur,
                    enclosing,
                    &mut planner,
                )?;
                #[cfg(feature = "window")]
                order_refs.push(order_ref(
                    e,
                    &r,
                    arena,
                    items.as_slice(),
                    item_aliases.as_slice(),
                ));
                // v0.16: the term sorts by its collation.
                let mut dir = *dir;
                dir.coll = crate::query::expr_coll(&r).unwrap_or_default();
                order_by.push((r, dir));
            }
            #[cfg(feature = "window")]
            cur.win.ok.set(false);
        }
        // The window classes (v0.16, `window`).
        #[cfg(feature = "window")]
        {
            crate::window::order_copies(
                items.as_mut_slice(),
                order_by.as_mut_slice(),
                order_refs.as_slice(),
                stmt.distinct,
            );
            crate::window::classify(items.as_mut_slice(), order_by.as_mut_slice());
        }
        // A subquery inside an aggregate's arguments needs
        // per-group-row execution (v0.11, the `v10` feature);
        // without it the plan is refused, as in v0.6.
        if cfg!(not(feature = "v10"))
            && (items.iter().any(crate::query::sub_in_agg_args)
                || having.as_ref().is_some_and(crate::query::sub_in_agg_args)
                || order_by
                    .iter()
                    .any(|(e, _)| crate::query::sub_in_agg_args(e)))
        {
            return Err(Error::Unsupported);
        }
        // Only GROUP BY or an aggregate in the result list makes
        // the query an aggregate one (v0.16 fix, ungated: an
        // aggregate in ORDER BY alone is the pin's "misuse of
        // aggregate" — hipp aggregated the query to one row).
        // HAVING needs one of them already (checked above).
        // (An EXISTS's inert aggregate counts here: the pin
        // aggregates `SELECT EXISTS(SELECT max(t.a) FROM u) FROM
        // t` to one row.)
        let has_agg = !group_by.is_empty() || items.iter().any(crate::query::has_result_agg);
        if !has_agg && order_by.iter().any(|(e, _)| crate::query::has_agg(e)) {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedAggregate,
            });
        }
        // An aggregate inside another's arguments (`sum(max(a))`,
        // or one of this query's inside a subquery argument).
        if items.iter().any(crate::query::has_nested_agg)
            || having.as_ref().is_some_and(crate::query::has_nested_agg)
            || order_by
                .iter()
                .any(|(e, _)| crate::query::has_nested_agg(e))
        {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedAggregate,
            });
        }
        cur.aliases.clear();
        let keys = CompKeys {
            names: item_aliases,
            colls: core_colls(items.as_slice(), &[]),
            items: items.clone(),
            scope: cur,
        };
        Ok(CorePlan {
            sources,
            items,
            filter,
            group_by,
            having,
            order_by,
            limit: self.plan_limit(stmt.limit.as_deref(), arena)?,
            offset: self.plan_limit(stmt.offset.as_deref(), arena)?,
            has_agg,
            result_names,
            result_affs,
            result_named,
            keys,
        })
    }

    /// A FROM source that names no table or view: a table-valued
    /// pragma function or the empty temp schema, as the derived
    /// table of its rows (v0.16, `v16`). Out of line: `plan_from` is
    /// on the stack once per level of nested FROM subqueries.
    #[cfg(feature = "v16")]
    #[inline(never)]
    fn synth_source(
        &self,
        s: &crate::sql::FromSource,
        arena: &Arena,
    ) -> Result<Option<Box<crate::sql::SelectStmt>>, S::Error> {
        let n = handle_bytes(arena, s.table);
        let ca = &self.catalog_arena;
        let exists =
            self.catalog.find_readable(ca, n).is_some() || self.catalog.find_view(ca, n).is_some();
        if exists && s.args.is_some() {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownFunction,
            });
        }
        if exists {
            return Ok(None);
        }
        match self.pragma_source(arena, n, s.args.as_ref())? {
            Some(sel) => Ok(Some(Box::new(sel))),
            None if s.args.is_some() => Err(Error::Schema {
                kind: SchemaKind::UnknownFunction,
            }),
            None => Ok(None),
        }
    }

    /// One derived table of a FROM clause (v0.9; a view's or CTE's
    /// expansion under `v16`): planned as a subquery against
    /// `derived_scope` only (no LATERAL), its description and plan
    /// pushed; returns its width. Out of line: `plan_from` is on
    /// the stack once per level of nested FROM subqueries.
    #[cfg(feature = "v09")]
    #[inline(never)]
    fn plan_derived_source(
        &self,
        s: &crate::sql::FromSource,
        d: &crate::sql::SelectStmt,
        arena: &Arena,
        derived_scope: &[crate::query::ScopeGroup],
        descs: &mut AVec<crate::query::SourceDesc>,
        plans: &mut AVec<SourcePlan>,
    ) -> Result<usize, S::Error> {
        use crate::query::SourceDesc;
        // v0.16: an expanded view or CTE, possibly recursive.
        #[cfg(feature = "v16")]
        let mut dplan = match s.cte.as_ref().map(|c| c.rec) {
            Some(crate::sql::RecMark::Body { split, id }) => {
                self.plan_recursive(d, split, id, s.cte.as_deref(), arena, derived_scope)?
            }
            _ => self.plan_subquery(d, arena, derived_scope)?,
        };
        #[cfg(not(feature = "v16"))]
        let dplan = self.plan_subquery(d, arena, derived_scope)?;
        let w = dplan.width();
        let mut cols = AVec::new();
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        // The pin names a derived table's duplicate
        // columns `id`, `id:1`, ... — so only the first
        // is `id` to `s.id`, NATURAL and USING (review
        // regression: every duplicate stayed `id`, and
        // `SELECT *` over a USING join dropped them all).
        // A shared materialization (`CteUse::mat`) is read
        // only by a plan that needs nothing of its reader.
        #[cfg(feature = "v16")]
        {
            dplan.mat = match s.cte.as_ref() {
                Some(c) if dplan.uncorrelated => c.mat,
                _ => 0,
            };
        }
        #[cfg(feature = "v16")]
        if s.cte.as_ref().is_some_and(|c| c.view) {
            dplan.result_names =
                self.result_col_names(&dplan, crate::query::ScopeGroup::stack_end(derived_scope))?;
        }
        #[cfg(feature = "v16")]
        let mut names = Self::cte_names(arena, s.cte.as_deref(), &dplan)?;
        // A column list renames the result for every reader
        // (CREATE TABLE ... AS names its columns by it).
        #[cfg(feature = "v16")]
        {
            dplan.result_names = names.clone();
        }
        #[cfg(not(feature = "v16"))]
        let mut names = dplan.result_names.clone();
        crate::rules::unique_names::<S::Error>(names.as_mut_slice())?;
        for (i, (nm, aff)) in names.iter().zip(dplan.result_affs.iter()).enumerate() {
            // A compound's column affinity is demoted across
            // its cores, as on the pin (ungated fix: `(SELECT
            // text_col ... UNION ALL SELECT 5)` compared as
            // TEXT).
            let aff = derived_col_affinity(&dplan, i, *aff);
            cols.push((nm.clone(), aff));
            affs.push(crate::record::Affinity::Blob);
            defaults.push(OwnedValue::Null);
        }
        let qual = match s.alias {
            Some(a) => copy_vec(handle_bytes(arena, a)),
            // A table-valued function is named by its
            // name (v0.16).
            None if s.derived.is_none() => copy_vec(handle_bytes(arena, s.table)),
            None => AVec::new(),
        };
        let mut desc = SourceDesc::new(qual, cols, None, false);
        desc.kind = s.kind;
        // v0.16: a derived column compares by its
        // leftmost core's item collation (BINARY when
        // the item has none — still a column's).
        for it in dplan.items.iter() {
            desc.colls
                .push(crate::query::expr_coll(it).unwrap_or_default());
        }
        for v in dplan
            .values
            .as_slice()
            .first()
            .iter()
            .flat_map(|r| r.iter())
        {
            desc.colls
                .push(crate::query::expr_coll(v).unwrap_or_default());
        }
        descs.push(desc);
        plans.push(SourcePlan {
            root: 0,
            kind: s.kind,
            on: None,
            rowid_alias: None,
            ncols: w,
            affs,
            defaults,
            derived: Some(Box::new(dplan)),
            gcols: None,
            #[cfg(feature = "v16")]
            rec: 0,
        });
        Ok(w)
    }

    /// Plan a FROM clause's sources (v0.4; one helper for
    /// SELECT cores, subqueries and UPDATE ... FROM since
    /// v0.16): each table or derived table gets its resolution
    /// description (qualifier, columns, join kind), its absolute
    /// first slot (counting from `total`), and its executor
    /// plan. Join constraints are resolved later
    /// (`plan_using`, `plan_on`) once the whole scope exists.
    #[cfg(feature = "v04")]
    #[allow(clippy::too_many_arguments)]
    fn plan_from(
        &self,
        from: &[crate::sql::FromSource],
        arena: &Arena,
        derived_scope: &[crate::query::ScopeGroup],
        mut total: usize,
        descs: &mut AVec<crate::query::SourceDesc>,
        offsets: &mut AVec<usize>,
        plans: &mut AVec<SourcePlan>,
    ) -> Result<(), S::Error> {
        use crate::query::SourceDesc;
        #[cfg(not(feature = "v09"))]
        let _ = derived_scope;
        for s in from.iter() {
            offsets.push(total);
            // v0.16: a recursive CTE's recursive reference reads
            // the row its queue gave up.
            #[cfg(feature = "v16")]
            if let Some(crate::sql::RecMark::Ref { id }) = s.cte.as_ref().map(|c| c.rec) {
                total += self.plan_rec_ref(s, id, arena, descs, plans)?;
                continue;
            }
            // v0.16: a table-valued pragma function, or the empty
            // temp schema, as the derived table of its rows.
            // A table or view of the name shadows the function; given
            // arguments it is the pin's "'t' is not a function" (v0.16
            // fix: the arguments were ignored).
            #[cfg(feature = "v16")]
            let synth = if s.derived.is_none() {
                self.synth_source(s, arena)?
            } else {
                None
            };
            #[cfg(feature = "v16")]
            let derived = synth.as_ref().or(s.derived.as_ref());
            #[cfg(all(feature = "v09", not(feature = "v16")))]
            let derived = s.derived.as_ref();
            // A derived table (v0.9): planned as a subquery
            // against `derived_scope` only (no LATERAL).
            #[cfg(feature = "v09")]
            if let Some(d) = derived {
                total += self.plan_derived_source(s, d, arena, derived_scope, descs, plans)?;
                continue;
            }
            let tname = handle_bytes(arena, s.table);
            #[cfg(feature = "v16")]
            let found = self.catalog.find_readable(&self.catalog_arena, tname);
            #[cfg(not(feature = "v16"))]
            let found = self.catalog.find(&self.catalog_arena, tname);
            let t = match found {
                Some(t) => t.usable()?,
                None => {
                    if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                        // Views are inert (SPEC §2): stored,
                        // never executed.
                        return Err(Error::Unsupported);
                    }
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    });
                }
            };
            let mut cols = AVec::new();
            let mut affs = AVec::new();
            let mut defaults = AVec::new();
            for c in t.columns.iter() {
                cols.push((
                    copy_vec(handle_bytes(&self.catalog_arena, c.name)),
                    c.affinity,
                ));
                affs.push(c.affinity);
                defaults.push(read_default(&self.catalog_arena, c));
            }
            let qual = match s.alias {
                Some(a) => copy_vec(handle_bytes(arena, a)),
                None => copy_vec(tname),
            };
            let mut desc = SourceDesc::new(qual, cols, t.rowid_alias, true);
            desc.kind = s.kind;
            desc.colls = table_colls(t);
            desc.schema_table = t.root == 1 && s.alias.is_none();
            // An ALTER TABLE tracing references to its table (v0.16).
            #[cfg(feature = "v16")]
            if let Some(tr) = &self.alter_trace
                && eq_ci(tr.borrow().table.as_slice(), tname)
            {
                tr.borrow_mut().from.push(s.table.pos);
                desc.trace = Some((tr.clone(), s.alias.is_some()));
            }
            descs.push(desc);
            let gcols = if t.has_generated() {
                self.table_rules(arena, t)?
                    .and_then(|r| r.gcols)
                    .map(Box::new)
            } else {
                None
            };
            plans.push(SourcePlan {
                root: t.root,
                kind: s.kind,
                on: None,
                rowid_alias: t.rowid_alias,
                ncols: t.columns.len(),
                affs,
                defaults,
                derived: None,
                gcols,
                #[cfg(feature = "v16")]
                rec: 0,
            });
            total += t.columns.len();
        }
        Ok(())
    }

    /// Resolve each joined source's ON clause (v0.16) and fold
    /// it — or the USING / NATURAL equalities `plan_using`
    /// built — into the source's executor `on`. An ON term that
    /// references a source joined LATER follows the pin: on an
    /// outer join (LEFT / RIGHT / FULL), or anywhere once the
    /// FROM clause has a RIGHT / FULL join, it is an error;
    /// otherwise (an inner join's ON) it is hoisted into WHERE —
    /// the returned conjunction — exactly as SQLite moves it,
    /// which for an inner join changes no result.
    #[cfg(feature = "v04")]
    #[allow(clippy::too_many_arguments)]
    fn plan_on(
        &self,
        from: &[crate::sql::FromSource],
        arena: &Arena,
        cur: &crate::query::ScopeGroup,
        enclosing: &[crate::query::ScopeGroup],
        plans: &mut [SourcePlan],
        using: AVec<Option<RExpr>>,
        planner: crate::query::SubPlanner<'_, S::Error>,
    ) -> Result<Option<RExpr>, S::Error> {
        use crate::query::{RefRange, expr_refs_range, has_agg, resolve_scoped};
        let any_right = from.iter().any(|s| s.kind.pads_left());
        let n = cur.sources.len();
        let end = cur.offsets.as_slice().last().copied().unwrap_or(0)
            + cur.sources.as_slice().last().map_or(0, |s| s.columns.len());
        let mut hoisted = None;
        let mut using = using.as_slice().iter();
        for (i, (s, plan)) in from.iter().zip(plans.iter_mut()).enumerate() {
            let mut cond = using.next().cloned().flatten();
            if let Some(on) = &s.on {
                let r = resolve_scoped::<S::Error>(on, arena, cur, enclosing, &mut *planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                let later = RefRange {
                    w: (
                        cur.offsets.as_slice().get(i + 1).copied().unwrap_or(end),
                        end,
                    ),
                    s: (cur.first_src + i + 1, cur.first_src + n),
                };
                if expr_refs_range(&r, &later) {
                    if any_right || s.kind.pads_left() || s.kind.pads_right() {
                        return Err(Error::Schema {
                            kind: SchemaKind::OnReferencesLaterSource,
                        });
                    }
                    hoisted = Some(and_expr(hoisted, r));
                } else {
                    cond = Some(and_expr(cond, r));
                }
            }
            plan.on = cond;
        }
        Ok(hoisted)
    }

    /// Plan a LIMIT / OFFSET expression (v0.16): resolved
    /// against an empty scope with no enclosing scopes — the
    /// pin resolves LIMIT with an empty name context, so a
    /// column reference (even an outer one) is "no such
    /// column" and any subquery in it is uncorrelated — and
    /// aggregate-free ("misuse of aggregate").
    #[cfg(feature = "v04")]
    fn plan_limit(&self, e: Option<&Expr>, arena: &Arena) -> Result<Option<RExpr>, S::Error> {
        let Some(e) = e else {
            return Ok(None);
        };
        let empty = crate::query::ScopeGroup::new(AVec::new(), AVec::new(), 0);
        let mut planner = self.sub_planner(arena);
        let r = crate::query::resolve_scoped::<S::Error>(e, arena, &empty, &[], &mut planner)?;
        if crate::query::has_agg(&r) {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedAggregate,
            });
        }
        Ok(Some(r))
    }

    /// The v0.3 planner seam for `WHERE <col> = <literal>` on a
    /// non-rowid column: pick an index whose first key column is
    /// `col` (preferring a UNIQUE one), convert the literal with
    /// the column's affinity, and lower to `Access::Index`.
    /// Returns None when no usable index exists.
    #[cfg(feature = "indexes")]
    fn plan_index_access(
        &self,
        t: &crate::schema::TableDef,
        tname: &[u8],
        col: usize,
        arena: &Arena,
        lit: Literal,
    ) -> Option<Access> {
        if matches!(lit, Literal::Null) {
            return Some(Access::Empty);
        }
        let mut chosen: Option<(u32, bool, AVec<crate::record::KeyOrder>)> = None;
        let col_coll = t.columns.as_slice().get(col).map(|c| c.coll);
        for ix in self.catalog.indexes_for(&self.catalog_arena, tname) {
            if ix.opaque {
                continue;
            }
            let Some(first) = ix.columns.as_slice().first() else {
                continue;
            };
            // v0.16: `col = lit` compares by the column's
            // collation; only an index keyed by that same
            // collation can answer it.
            if first.col != Some(col) || Some(first.coll) != col_coll {
                continue;
            }
            let mut desc = AVec::new();
            for c in ix.columns.iter() {
                desc.push(crate::record::KeyOrder {
                    desc: c.desc,
                    coll: c.coll,
                });
            }
            match &chosen {
                None => chosen = Some((ix.root, ix.unique, desc)),
                Some((_, unique, _)) if ix.unique && !unique => {
                    chosen = Some((ix.root, ix.unique, desc));
                }
                _ => {}
            }
        }
        let (root, _, desc) = chosen?;
        let probe = apply_affinity(
            literal_owned(arena, Some(lit)),
            t.columns.as_slice()[col].affinity,
        );
        if matches!(probe, OwnedValue::Null) {
            return Some(Access::Empty);
        }
        Some(Access::Index { root, desc, probe })
    }

    fn resolve_target(&self, t: &crate::schema::TableDef, name: &[u8]) -> Result<Target, S::Error> {
        for (i, c) in t.columns.iter().enumerate() {
            if eq_ci(handle_bytes(&self.catalog_arena, c.name), name) {
                return Ok(Target::Col(i));
            }
        }
        if eq_ci(name, b"rowid") || eq_ci(name, b"oid") || eq_ci(name, b"_rowid_") {
            return Ok(Target::Rowid);
        }
        Err(Error::Schema {
            kind: SchemaKind::UnknownColumn,
        })
    }

    /// Drive a transaction left mid-commit by a dropped future to
    /// completion (SPEC §5.1.1's `resume_txn`). Returns whether a
    /// transaction was in flight.
    pub async fn resume_txn(&mut self) -> Result<bool, S::Error> {
        if !self.pager.in_txn() {
            return Ok(false);
        }
        self.pager.drive_commit().await?;
        // The resumed transaction is committed, which closes any
        // explicit bracket that contained it (v0.2).
        self.explicit_txn = false;
        Ok(true)
    }

    /// Roll back the connection's open transaction, if any.
    pub async fn rollback(&mut self) -> Result<(), S::Error> {
        self.pager.rollback().await?;
        self.explicit_txn = false;
        Ok(())
    }

    /// The rowid of the most recent row an INSERT (or REPLACE, or
    /// an upsert's insert branch) put into a table on this
    /// connection, 0 before any (v0.16, `v16`; the pin's
    /// `sqlite3_last_insert_rowid`, also read by the SQL function
    /// `last_insert_rowid()`). An upsert's DO UPDATE, UPDATE and
    /// CREATE TABLE ... AS SELECT leave it alone; a failed
    /// statement keeps the rowids it inserted before failing.
    #[cfg(feature = "v16")]
    pub fn last_insert_rowid(&self) -> i64 {
        self.env.last_rowid.get()
    }

    /// The rows the most recent successful INSERT, UPDATE or DELETE
    /// changed (v0.16, `v16`; the pin's `sqlite3_changes64`, also
    /// `changes()`): inserted rows (REPLACE's deletions not
    /// counted), rows updated (once per row, also by an upsert's
    /// DO UPDATE) and rows deleted. A failed write sets it to 0;
    /// other statements (SELECT, DDL, CREATE TABLE ... AS SELECT,
    /// transaction control) leave it alone.
    #[cfg(feature = "v16")]
    pub fn changes(&self) -> i64 {
        self.env.changes.get()
    }

    /// The sum of `changes()` over every successful INSERT, UPDATE
    /// and DELETE since the connection opened (v0.16, `v16`; the
    /// pin's `sqlite3_total_changes64`, also `total_changes()`) —
    /// a write later rolled back still counts.
    #[cfg(feature = "v16")]
    pub fn total_changes(&self) -> i64 {
        self.env.total_changes.get()
    }

    /// Whether a write transaction is currently open on this
    /// connection (including one left behind by a dropped future).
    pub fn in_transaction(&self) -> bool {
        self.pager.in_txn()
    }

    /// Whether an explicit `BEGIN` transaction is open (v0.2).
    /// Distinct from `in_transaction`, which reports the pager's
    /// write transaction — that one opens lazily at first write.
    pub fn in_explicit_transaction(&self) -> bool {
        self.explicit_txn
    }

    /// Catalog entries for tools (the CLI's `.tables` / `.schema`
    /// / `.dump`): table name + stored CREATE SQL, in schema
    /// (creation) order.
    pub fn tables(&self) -> AVec<TableMeta<'_>> {
        let mut out = AVec::new();
        for t in self.catalog.tables.iter() {
            let mut columns = AVec::new();
            let mut generated = AVec::new();
            for c in t.columns.iter() {
                columns.push(handle_bytes(&self.catalog_arena, c.name));
                #[cfg(feature = "v04")]
                generated.push(c.gen_expr.is_some());
                #[cfg(not(feature = "v04"))]
                generated.push(false);
            }
            out.push(TableMeta {
                name: handle_bytes(&self.catalog_arena, t.name),
                sql: handle_bytes(&self.catalog_arena, t.sql),
                columns,
                generated,
            });
        }
        out
    }

    /// Index catalog entries for tools (v0.3): every index in
    /// schema order; `sql` is None for implicit (constraint-owned)
    /// indexes, which have no CREATE INDEX text.
    pub fn indexes(&self) -> AVec<IndexMeta<'_>> {
        let mut out = AVec::new();
        for i in self.catalog.indexes.iter() {
            out.push(IndexMeta {
                name: handle_bytes(&self.catalog_arena, i.name),
                table: handle_bytes(&self.catalog_arena, i.table),
                sql: i.sql.map(|h| handle_bytes(&self.catalog_arena, h)),
                unique: i.unique,
            });
        }
        out
    }

    /// View catalog entries for tools (v0.4): name, stored SQL,
    /// and derived column names, in schema order.
    pub fn views(&self) -> AVec<ViewMeta<'_>> {
        let mut out = AVec::new();
        for v in self.catalog.views.iter() {
            let mut cols = AVec::new();
            for c in v.columns.iter() {
                cols.push(handle_bytes(&self.catalog_arena, *c));
            }
            out.push(ViewMeta {
                name: handle_bytes(&self.catalog_arena, v.name),
                sql: handle_bytes(&self.catalog_arena, v.sql),
                columns: cols,
            });
        }
        out
    }

    /// The connection's TEMP database (v0.16, `v16`), once a
    /// statement has used it: a private in-memory connection whose
    /// `tables()` / `indexes()` / `views()` are the temp schema's
    /// (tools print them as `temp.name`). `None` before first use.
    #[cfg(feature = "v16")]
    pub fn temp_db(&self) -> Option<&Connection<crate::storage::MemStorage>> {
        self.temp.as_deref()
    }

    /// Mutable pager access for tools/tests driving the layers
    /// below the session API (cursor-level scans, §4.1 gates).
    pub fn pager_mut(&mut self) -> &mut Pager<S> {
        &mut self.pager
    }

    /// The underlying storage (for tests/tools that snapshot bytes).
    pub fn storage(&self) -> &S {
        self.pager.storage()
    }

    /// Cumulative subquery-plan executions on this connection
    /// (v0.13; see the `sub_execs` field). Tests assert the
    /// hoisting win with it: an uncorrelated subquery node's
    /// executions per statement drop from O(outer rows) to 1.
    #[cfg(feature = "v06")]
    pub fn subquery_executions(&self) -> usize {
        self.sub_execs
    }

    /// Arena high-water marks + pool bytes (SPEC §7.5.3 stats).
    pub fn arena_stats(&self) -> ArenaStats {
        ArenaStats {
            catalog_high_water: self.catalog_arena.high_water(),
            txn_high_water: self.pager.txn_arena.high_water(),
            pool_bytes: self.pager.pool_bytes(),
            page_count: self.pager.page_count(),
        }
    }
}

/// One catalog row for tools: a table's name and its stored
/// `CREATE TABLE` text (SPEC §5.5 stores the statement as issued).
#[derive(Debug, Clone)]
pub struct TableMeta<'a> {
    pub name: &'a [u8],
    pub sql: &'a [u8],
    /// Column names, in table order (v0.16).
    pub columns: AVec<&'a [u8]>,
    /// Per column, whether it is a generated column (v0.16): a
    /// dump replays only the others, as the pin's shell does
    /// (`INSERT INTO t VALUES (...)` takes non-generated values).
    pub generated: AVec<bool>,
}

/// One view catalog row for tools (v0.4).
#[derive(Debug, Clone)]
pub struct ViewMeta<'a> {
    pub name: &'a [u8],
    pub sql: &'a [u8],
    pub columns: AVec<&'a [u8]>,
}

/// One index catalog row for tools (v0.3).
#[derive(Debug, Clone, Copy)]
pub struct IndexMeta<'a> {
    pub name: &'a [u8],
    pub table: &'a [u8],
    pub sql: Option<&'a [u8]>,
    pub unique: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ArenaStats {
    pub catalog_high_water: usize,
    pub txn_high_water: usize,
    pub pool_bytes: usize,
    pub page_count: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Fresh,
    Running,
    Done,
}

/// A statement's parameters and re-execution state (v0.16,
/// `params`).
#[cfg(feature = "params")]
struct Binds {
    /// The numbering the lexer assigned (count, names).
    table: crate::sql::lexer::ParamTable,
    /// The bound values, one per index (`table.count` of them);
    /// NULL until bound.
    values: AVec<OwnedValue>,
    /// The statement as parsed, its parameters unbound; `None`
    /// when it has none (then `Statement::ast` is the source).
    src: Option<Ast>,
    /// The statement arena's mark before planning: re-planning
    /// rewinds here, so a re-executed statement does not grow
    /// its arena.
    plan_ckpt: usize,
    /// The plan must be made again before the next execution:
    /// the bindings changed, or the statement was reset (a plan
    /// holds execution-time state — the schema it resolved, the
    /// clock read of an expression DEFAULT — so every execution
    /// after the first plans afresh, from the parsed tree).
    stale: bool,
    /// `step` has run since prepare / the last `reset`.
    started: bool,
}

/// Whether a planning error of the all-NULL bindings is a value
/// error that another binding could avoid (v0.16, `params`): a
/// constraint, type or range failure of a constant row or operand
/// — errors the pin reports only when it runs the statement.
/// Only these are deferred to `step` (see
/// `Connection::prepare_params`); every other error (grammar, name
/// resolution, an existing table, storage, corruption) is what the
/// pin's `sqlite3_prepare` reports, so prepare reports it.
#[cfg(feature = "params")]
fn binding_dependent<E>(e: &Error<E>) -> bool {
    // The percentile family's runtime checks read the bound
    // values too (v0.16, `v16`).
    #[cfg(feature = "v16")]
    if matches!(
        e,
        Error::Schema {
            kind: SchemaKind::PercentileFraction
                | SchemaKind::PercentileFractionVaries
                | SchemaKind::PercentileNotNumeric
                | SchemaKind::PercentileInfinite
        }
    ) {
        return true;
    }
    matches!(
        e,
        Error::Schema {
            kind: SchemaKind::DuplicateRowid
                | SchemaKind::DatatypeMismatch
                | SchemaKind::MissingRowidValue
                | SchemaKind::UniqueViolation
                | SchemaKind::NotNullViolation
                | SchemaKind::IntegerOverflow
                | SchemaKind::CheckViolation
                | SchemaKind::StrictTypeViolation
                | SchemaKind::EscapeNotSingleChar
                | SchemaKind::PatternTooComplex
        }
    )
}

pub struct Statement<'c, S: Storage> {
    #[cfg(feature = "params")]
    binds: Binds,
    conn: &'c mut Connection<S>,
    arena: Arena,
    sql_text: crate::arena::ABytes,
    ast: Ast,
    plan: Plan,
    phase: Phase,
    cursor: Option<ScanCursor>,
    row_buf: AVec<u8>,
    current: AVec<OwnedValue>,
    current_rowid: Option<i64>,
    row_ckpt: usize,
    /// Secondary-index scan state (v0.3): the rowids an
    /// `Access::Index` lookup produced, consumed one per step.
    #[cfg(feature = "indexes")]
    index_rowids: AVec<i64>,
    #[cfg(feature = "indexes")]
    index_pos: usize,
    #[cfg(feature = "indexes")]
    index_loaded: bool,
    /// v0.4 query results, materialised on first step.
    #[cfg(feature = "v04")]
    query_rows: AVec<AVec<OwnedValue>>,
    #[cfg(feature = "v04")]
    query_rowids: AVec<Option<i64>>,
    #[cfg(feature = "v04")]
    query_pos: usize,
    #[cfg(feature = "v04")]
    query_loaded: bool,
    /// Hoisted uncorrelated-subquery results for this
    /// statement's execution (v0.13): `(plan node id, value)`
    /// pairs, populated on first use and cleared when a fresh
    /// execution begins (see `Statement::step`). The pinned
    /// oracle fixes an uncorrelated subquery's value for the
    /// whole statement, so one entry serves every outer row —
    /// including a statement that writes the subquery's own
    /// table, whose pre-statement snapshot the first use
    /// captures.
    #[cfg(feature = "v06")]
    hoist: AVec<((usize, usize), crate::query::SubValue)>,
    /// The uncorrelated subqueries of a RETURNING list that do not
    /// read the written table, cached for the statement (v0.16
    /// review fix; see `eval_returning`).
    #[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
    ret_hoist: AVec<((usize, usize), crate::query::SubValue)>,
    /// v0.7 RETURNING results of a write statement, materialised
    /// by the write's execution on first step and then served
    /// one row per step, like a query's.
    #[cfg(feature = "v07")]
    ret_rows: AVec<AVec<OwnedValue>>,
    #[cfg(feature = "v07")]
    ret_rowids: AVec<Option<i64>>,
    #[cfg(feature = "v07")]
    ret_pos: usize,
    #[cfg(feature = "v07")]
    ret_loaded: bool,
    /// An error to report after the rows already produced (v0.16,
    /// `v16`: an integrity check that met a corrupt page).
    #[cfg(feature = "v16")]
    deferred: Option<Error<S::Error>>,
    /// The rows recursive CTEs' queues gave up, innermost last
    /// (v0.16, `v16`): a recursive reference (`SubSource::rec`)
    /// reads the last entry with its id.
    #[cfg(feature = "v16")]
    rec_rows: AVec<(u32, AVec<OwnedValue>)>,
    /// Shared materializations of this execution (v0.16 review
    /// fix, `v16`; `SubPlan::mat`): (id, rows, complete). A
    /// recursive CTE's entry may hold only the prefix a bounded
    /// reader took (`complete` false).
    #[cfg(feature = "v16")]
    mats: AVec<(u32, AVec<AVec<OwnedValue>>, bool)>,
    /// The savepoint level of the statement savepoint this write
    /// runs under inside a transaction (v0.16, `v16`; see
    /// `abort_write`), while it is open.
    #[cfg(feature = "v16")]
    stmt_sp: Option<usize>,
    /// An upsert's DO UPDATE failed (v0.16, `v16`): its conflicts
    /// resolve by ABORT whatever the statement's OR clause says, as
    /// on the pin, so OR ROLLBACK does not end the transaction.
    #[cfg(feature = "v16")]
    upsert_failed: bool,
    /// The running INSERT's AUTOINCREMENT state (v0.16, `v16`).
    #[cfg(feature = "v16")]
    autoinc: Option<Autoinc>,
    /// The triggers this write may fire (v0.16, `triggers`), loaded
    /// when its execution starts; `None` when it fires none.
    #[cfg(feature = "triggers")]
    trig: Option<alloc::rc::Rc<trigger::Target>>,
    /// The foreign keys this write checks and acts on (v0.16,
    /// `foreign_keys`), loaded when its execution starts; `None`
    /// when `PRAGMA foreign_keys` is off or the table takes part in
    /// none.
    #[cfg(feature = "foreign_keys")]
    fk: Option<alloc::rc::Rc<fkey::FkTarget>>,
    /// An UPDATE OR REPLACE's keys for the rows its conflicts delete
    /// (all of the table's, as a DELETE's).
    #[cfg(feature = "foreign_keys")]
    fk_del: Option<alloc::rc::Rc<fkey::FkTarget>>,
    /// The keys whose new rows this write does not look up (the
    /// pin's `isSetNullAction`; see `fkey::Sim`), loaded with `fk`.
    #[cfg(feature = "foreign_keys")]
    fk_skip: fkey::Skip,
}

/// One INSERT's view of an AUTOINCREMENT table's `sqlite_sequence`
/// row (v0.16, `v16`), as the pin keeps it: read when the statement
/// starts (the first row whose `name` is the table's name, byte for
/// byte; `seq` converted as CAST AS INTEGER, NULL or no row reading
/// 0), raised by every rowid the statement inserts or generates,
/// and written back when it completes — only when there was no row
/// or the counter grew past what was read. A statement that fails
/// part way (OR FAIL keeps its earlier rows) writes nothing.
#[cfg(feature = "v16")]
struct Autoinc {
    name: AVec<u8>,
    seq_root: u32,
    seq_rowid: Option<i64>,
    counter: i64,
    orig: Option<i64>,
}

/// Refuse a new object name the pin reserves (`sqlite_` prefix,
/// ASCII case-insensitive): `SchemaKind::ReservedName`.
fn reserved_name<E>(name: &[u8]) -> Result<(), E> {
    match name.get(..7) {
        Some(p) if eq_ci(p, b"sqlite_") => Err(Error::Schema {
            kind: SchemaKind::ReservedName,
        }),
        _ => Ok(()),
    }
}

/// A column's default as a stored value: the literal with the
/// column's affinity applied (v0.16 fix). A record shorter than
/// the table (a column added by ALTER TABLE) reads this for the
/// missing field — `ADD COLUMN c INT DEFAULT '5'` reads as the
/// integer 5 on the pin, where hipp used to return the text.
fn read_default(arena: &Arena, c: &crate::schema::Column) -> OwnedValue {
    apply_affinity(literal_owned(arena, c.default), c.affinity)
}

/// Whether a column definition's text declares a REFERENCES
/// clause (v0.16).
#[cfg(feature = "v16")]
fn def_references<E>(text: &[u8], arena: &Arena) -> bool {
    use crate::sql::lexer::Tok;
    let Ok(toks) = crate::sql::lexer::lex::<E>(text, arena) else {
        return false;
    };
    toks.iter().any(
        |t| matches!(t.tok, Tok::Ident(h, false) if eq_ci(handle_bytes(arena, h), b"REFERENCES")),
    )
}

#[cfg(any(feature = "triggers", feature = "foreign_keys"))]
impl<S: Storage> Connection<S> {
    /// Whether a trigger or foreign-key action program (or DROP
    /// TABLE's inline DELETE) is running: its statements commit,
    /// roll back and check nothing themselves.
    pub(super) fn in_program(&self) -> bool {
        #[cfg(feature = "triggers")]
        if self.in_trigger() {
            return true;
        }
        #[cfg(all(feature = "foreign_keys", not(feature = "triggers")))]
        if self.fk.depth > 0 {
            return true;
        }
        #[cfg(feature = "foreign_keys")]
        if self.fk.nest > 0 {
            return true;
        }
        false
    }
}

/// An INSERT plan's column affinities (v0.16, `foreign_keys`).
#[cfg(feature = "foreign_keys")]
fn ins_affs(cols: &[InsCol]) -> AVec<crate::record::Affinity> {
    let mut out = AVec::new();
    for c in cols {
        out.push(c.affinity);
    }
    out
}

/// Whether `e` calls `last_insert_rowid()` (v0.16), whose value
/// moves with every row an INSERT inserts.
#[cfg(feature = "v16")]
fn reads_last_rowid(e: &RExpr) -> bool {
    crate::query::any_scalar(e, &|f| f == crate::query::ScalarFn::LastInsertRowid)
}

fn literal_owned(arena: &Arena, lit: Option<Literal>) -> OwnedValue {
    match lit {
        None | Some(Literal::Null) => OwnedValue::Null,
        Some(Literal::Integer(i)) => OwnedValue::Integer(i),
        Some(Literal::Real(f)) => OwnedValue::Real(f),
        Some(Literal::Text(h)) => OwnedValue::Text(copy_vec(handle_bytes(arena, h))),
        Some(Literal::Blob(h)) => OwnedValue::Blob(copy_vec(handle_bytes(arena, h))),
    }
}

/// A table's per-column collating sequences (v0.16), for its
/// resolution `SourceDesc`.
#[cfg(feature = "v04")]
fn table_colls(t: &crate::schema::TableDef) -> AVec<crate::record::Coll> {
    let mut out = AVec::new();
    for c in t.columns.iter() {
        out.push(c.coll);
    }
    out
}

fn copy_vec(b: &[u8]) -> AVec<u8> {
    let mut v = AVec::new();
    for x in b {
        v.push(*x);
    }
    v
}

/// The stored schema text for a CREATE statement: the statement
/// as issued, minus any trailing semicolon/whitespace (SQLite's
/// convention), rebuilt from the bare created name on after the
/// pin's own `CREATE TABLE ` / `CREATE VIEW ` / `CREATE [UNIQUE]
/// INDEX ` prefix (v0.16: a stored `CREATE TABLE main.t(...)` is a
/// schema the pin reports as corrupt, and `IF NOT EXISTS` or the
/// keywords' spelling are not kept by the pin either). `arena`
/// holds the re-lexed tokens. AUTOINCREMENT is refused
/// (`Error::Unsupported`: hipp keeps no `sqlite_sequence`).
fn trimmed_sql_text<E>(raw: &[u8], arena: &Arena) -> Result<AVec<u8>, E> {
    let mut end = raw.len();
    while end > 0 && (raw[end - 1] == b';' || (raw[end - 1] as char).is_whitespace()) {
        end -= 1;
    }
    let raw = &raw[..end];
    Ok(
        match crate::sql::parser::schema_text_start::<E>(raw, arena)? {
            Some((prefix, at)) if at <= raw.len() => {
                let mut v = copy_vec(prefix);
                v.extend_from_slice(&raw[at..]);
                v
            }
            _ => copy_vec(raw),
        },
    )
}

/// Where ALTER TABLE ADD COLUMN splices `, <column>` into a stored
/// CREATE TABLE text: after the last column definition — at the
/// comma that opens the table constraints, or the closing `)` — as
/// the pin's `addColOffset` (v0.16 fix, ungated: hipp appended the
/// column after the table constraints, a text that no longer parses,
/// so ADD COLUMN failed on any table with one).
#[cfg(feature = "v04")]
fn add_column_offset<E>(sql: &[u8], arena: &Arena) -> Result<usize, E> {
    use crate::sql::lexer::Tok;
    let toks = crate::sql::lexer::lex::<E>(sql, arena)?;
    let t = toks.as_slice();
    let kw = |i: usize| match t.get(i).map(|x| x.tok) {
        Some(Tok::Ident(h, false)) => {
            let w = handle_bytes(arena, h);
            [
                &b"CONSTRAINT"[..],
                b"PRIMARY",
                b"UNIQUE",
                b"CHECK",
                b"FOREIGN",
            ]
            .iter()
            .any(|k| eq_ci(w, k))
        }
        _ => false,
    };
    let mut depth = 0usize;
    for (i, x) in t.iter().enumerate() {
        match x.tok {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(x.offset);
                }
            }
            Tok::Comma if depth == 1 && kw(i + 1) => return Ok(x.offset),
            _ => {}
        }
    }
    Err(Error::Unsupported)
}

/// Whether a table is AUTOINCREMENT (v0.16): the catalog's flag
/// with `v16`; without it, whether its stored text carries the
/// clause (`schema_text_start` refuses that text there).
#[cfg(feature = "v05")]
#[cfg_attr(feature = "v16", allow(clippy::extra_unused_type_parameters))]
fn table_autoinc<E>(t: &crate::schema::TableDef, cat: &Arena, arena: &Arena) -> bool {
    #[cfg(feature = "v16")]
    let _ = (cat, arena);
    #[cfg(feature = "v16")]
    return t.autoinc;
    #[cfg(not(feature = "v16"))]
    crate::sql::parser::schema_text_start::<E>(handle_bytes(cat, t.sql), arena).is_err()
}

/// Whether a parsed CREATE TABLE has a rowid alias (v0.16, `v16`):
/// exactly one PRIMARY KEY column, declared exactly INTEGER and not
/// written `INTEGER PRIMARY KEY DESC` — the rule `schema.rs` applies
/// when it loads the table. AUTOINCREMENT needs one.
#[cfg(feature = "v16")]
fn create_has_rowid_alias(ast: &Ast, arena: &Arena) -> bool {
    let Ast::CreateTable {
        columns,
        pk_columns,
        ..
    } = ast
    else {
        return false;
    };
    pk_columns.len() == 1
        && columns.iter().any(|c| {
            eq_ci(
                handle_bytes(arena, c.name),
                handle_bytes(arena, pk_columns.as_slice()[0]),
            ) && !c.pk_desc
                && eq_ci(
                    crate::record::decl_token(handle_bytes(arena, c.decl)),
                    b"INTEGER",
                )
        })
}

/// Build an index key (the key columns' values, in index order)
/// for one table row (v0.3). `payload` is the row's table record;
/// short records materialise column defaults, as scans do.
#[cfg(feature = "indexes")]
fn index_key_for_row<E>(
    cols: &[IdxColPlan],
    rowid_alias: Option<usize>,
    defaults: &[OwnedValue],
    payload: &[u8],
    ncols: usize,
    rowid: i64,
    #[cfg(feature = "v04")] gcols: Option<&crate::rules::GenCols>,
) -> Result<AVec<OwnedValue>, E> {
    // v0.16: with generated columns, decode around the VIRTUAL
    // ones and compute them — an index may key on one.
    #[cfg(feature = "v04")]
    if let Some(g) = gcols {
        let mut rec =
            crate::rules::decode_rec::<E>(payload, ncols, defaults, Some(g.virt.as_slice()))?;
        g.fill::<E>(rec.as_mut_slice(), rowid, true)?;
        let mut key = AVec::new();
        for c in cols {
            key.push(match c.col {
                Some(i) if Some(i) != rowid_alias => {
                    rec.as_slice().get(i).cloned().unwrap_or(OwnedValue::Null)
                }
                _ => OwnedValue::Integer(rowid),
            });
        }
        return Ok(key);
    }
    let mut dec: AVec<Value<'_>> = AVec::new();
    crate::record::decode_record_into::<E>(payload, ncols, &mut dec)?;
    let mut key = AVec::new();
    for c in cols {
        key.push(match c.col {
            None => OwnedValue::Integer(rowid),
            Some(i) => {
                if Some(i) == rowid_alias {
                    OwnedValue::Integer(rowid)
                } else if i < dec.len() {
                    OwnedValue::from_value(dec.as_slice()[i])
                } else {
                    defaults[i].clone()
                }
            }
        });
    }
    Ok(key)
}

/// Encode an index key payload: the record of key values with
/// the rowid appended (SQFMT §2.6).
#[cfg(feature = "indexes")]
fn encode_index_payload(key: &[OwnedValue], rowid: i64) -> AVec<u8> {
    let mut vals: AVec<Value<'_>> = AVec::new();
    for v in key {
        vals.push(v.as_value());
    }
    vals.push(Value::Integer(rowid));
    let mut rec = AVec::new();
    encode_record(vals.as_slice(), &mut rec);
    rec
}

/// UNIQUE semantics: a key containing any NULL never conflicts
/// (SQLite treats NULLs as distinct for uniqueness).
#[cfg(feature = "indexes")]
fn key_has_no_null(key: &[OwnedValue]) -> bool {
    key.iter().all(|v| !matches!(v, OwnedValue::Null))
}

/// NOT NULL resolution for one fully-built row (v0.10; every
/// rule below probed on the pinned 3.54.1 oracle before
/// implementation). `row` holds one value per table column in
/// record form (affinities applied, defaults materialised);
/// the rowid-alias column is exempt — its record field is
/// NULL by construction and its value lives in the rowid.
/// Per statement conflict action:
/// - `Abort` (ABORT / FAIL / ROLLBACK / the default): any
///   NULL in a NOT NULL column is `NotNullViolation`.
/// - `Ignore`: returns `Ok(false)` — the caller skips this
///   row / combination and proceeds with the next.
/// - `Replace`: each NULL is replaced by the column's
///   default, affinity applied exactly as for a stored
///   value; a default that is itself NULL degrades to the
///   `Abort` error.
///
/// Callers run this BEFORE any uniqueness/rowid conflict
/// detection on the row: the pin reports the NOT NULL
/// violation when both are present, and an OR REPLACE
/// substitution is itself visible to conflict detection (a
/// substituted default that collides makes a victim).
/// Returns `Ok(true)` when the (possibly substituted) row
/// may proceed.
pub(crate) fn resolve_not_null<E>(
    row: &mut [OwnedValue],
    not_null: &[bool],
    defaults: &[OwnedValue],
    affs: &[crate::record::Affinity],
    rowid_alias: Option<usize>,
    action: ConflictAction,
) -> Result<bool, E> {
    for i in 0..row.len() {
        if !not_null[i] || Some(i) == rowid_alias || !matches!(row[i], OwnedValue::Null) {
            continue;
        }
        match action {
            ConflictAction::Ignore => return Ok(false),
            ConflictAction::Replace => {
                if matches!(defaults[i], OwnedValue::Null) {
                    return Err(Error::Schema {
                        kind: SchemaKind::NotNullViolation,
                    });
                }
                row[i] = apply_affinity(defaults[i].clone(), affs[i]);
            }
            // ABORT, FAIL, ROLLBACK: the statement fails (FAIL and
            // ROLLBACK differ only in what the failure undoes).
            _ => {
                return Err(Error::Schema {
                    kind: SchemaKind::NotNullViolation,
                });
            }
        }
    }
    Ok(true)
}

/// One row an UPDATE/DELETE will touch (v0.5), in record form:
/// decoded values with defaults materialised; the rowid-alias
/// column's slot holds its stored NULL — the rowid itself is in
/// `rowid`.
#[cfg(feature = "v05")]
struct WriteRow {
    rowid: i64,
    rec: AVec<OwnedValue>,
}

/// Build the evaluation row for one write-side row (v0.5): the
/// alias column reads as the rowid and REAL columns force reals,
/// exactly as the query executor builds its rows (§5.5).
#[cfg(feature = "v05")]
fn row_data_for(
    rowid_alias: Option<usize>,
    affs: &[crate::record::Affinity],
    rec: &[OwnedValue],
    rowid: i64,
) -> RowData {
    let mut vals = AVec::new();
    for i in 0..rec.len() {
        let mut v = if Some(i) == rowid_alias {
            OwnedValue::Integer(rowid)
        } else {
            rec[i].clone()
        };
        if affs[i] == crate::record::Affinity::Real
            && let OwnedValue::Integer(x) = v
        {
            v = OwnedValue::Real(x as f64);
        }
        vals.push(v);
    }
    let mut rowids = AVec::new();
    rowids.push(Some(rowid));
    RowData { vals, rowids }
}

/// An index key for one write-side row in record form (v0.5).
#[cfg(all(feature = "v05", feature = "indexes"))]
fn key_from_rec(
    cols: &[IdxColPlan],
    rowid_alias: Option<usize>,
    rec: &[OwnedValue],
    rowid: i64,
) -> AVec<OwnedValue> {
    let mut key = AVec::new();
    for c in cols {
        key.push(match c.col {
            None => OwnedValue::Integer(rowid),
            Some(i) => {
                if Some(i) == rowid_alias {
                    OwnedValue::Integer(rowid)
                } else {
                    rec[i].clone()
                }
            }
        });
    }
    key
}

/// An UPDATE's (or upsert DO UPDATE's) change set (v0.16): per
/// column, whether a SET names it, and whether the rowid (or its
/// alias) is assigned.
#[cfg(feature = "v05")]
fn assigned_columns(
    sets: &[(Target, RExpr)],
    ncols: usize,
    rowid_alias: Option<usize>,
) -> (AVec<bool>, bool) {
    let mut cols = AVec::new();
    for i in 0..ncols {
        cols.push(sets.iter().any(|(t, _)| *t == Target::Col(i)));
    }
    let rowid = sets.iter().any(|(t, _)| match t {
        Target::Rowid => true,
        Target::Col(i) => Some(*i) == rowid_alias,
    });
    (cols, rowid)
}

/// A new rowid assigned by UPDATE SET or an upsert DO UPDATE
/// (v0.5; v0.16 conversion): the pin's MustBeInt — NUMERIC
/// affinity, then a lossless integer (`'5'` and `7.0` assign 5
/// and 7). Unlike INSERT, NULL does not auto-assign: it is a
/// datatype mismatch, like any non-integer (oracle-probed).
#[cfg(feature = "v05")]
fn update_rowid<E>(v: &OwnedValue) -> Result<i64, E> {
    match apply_affinity(v.clone(), crate::record::Affinity::Integer) {
        OwnedValue::Integer(x) => Ok(x),
        _ => Err(Error::Schema {
            kind: SchemaKind::DatatypeMismatch,
        }),
    }
}

/// A unique index's canonical key columns with their
/// collations (v0.16), for upsert-target matching.
#[cfg(feature = "v07")]
type KeySet = AVec<(Target, crate::record::Coll)>;

/// Whether an upsert clause arbitrates one found conflict
/// (v0.7): no target arbitrates everything; a target arbitrates
/// a rowid conflict only when it is exactly `[Rowid]`, and an
/// index conflict only when the index's canonical key set is
/// exactly the target set.
#[cfg(feature = "v07")]
fn upsert_arbitrates(up: &UpsertPlan, by_rowid: bool, keyset: &Option<KeySet>) -> bool {
    let Some(target) = &up.target else {
        return true;
    };
    if by_rowid {
        return target.len() == 1 && target.as_slice()[0] == Target::Rowid;
    }
    match keyset {
        Some(ks) => keyset_matches(target.as_slice(), up.target_colls.as_slice(), ks.as_slice()),
        None => false,
    }
}

/// Whether an upsert target names exactly an index's key
/// columns (v0.7), each target column written with a `COLLATE`
/// matching that key column's collation (v0.16; the pin's
/// `sqlite3UpsertAnalyzeTarget` — a bare column matches any).
#[cfg(feature = "v07")]
fn keyset_matches(
    target: &[Target],
    colls: &[Option<crate::record::Coll>],
    keyset: &[(Target, crate::record::Coll)],
) -> bool {
    keyset.len() == target.len()
        && keyset.iter().all(|(k, kc)| {
            target
                .iter()
                .zip(colls.iter().chain(core::iter::repeat(&None)))
                .any(|(t, c)| t == k && c.is_none_or(|c| c == *kc))
        })
}

/// Coerce a `WHERE rowid = <literal>` literal to an access path,
/// following SQLite's comparison rules: the literal gets numeric
/// conversion, and a value that cannot equal any rowid yields an
/// empty scan — never an error.
fn literal_rowid(arena: &Arena, lit: Literal) -> Access {
    match lit {
        Literal::Integer(i) => Access::Rowid(i),
        Literal::Real(f) => match integral_f64(f) {
            Some(i) => Access::Rowid(i),
            None => Access::Empty,
        },
        Literal::Text(h) => {
            // The rowid's INTEGER affinity, as on the INSERT path:
            // only ASCII spaces trimmed (`'\u{a0}5'` is no rowid;
            // v0.16 fix, ungated — `trim` used to take Unicode ones).
            match crate::record::text_to_number(handle_bytes(arena, h)) {
                Some(crate::record::OwnedValue::Integer(i)) => Access::Rowid(i),
                Some(crate::record::OwnedValue::Real(f)) => match integral_f64(f) {
                    Some(i) => Access::Rowid(i),
                    None => Access::Empty,
                },
                _ => Access::Empty,
            }
        }
        _ => Access::Empty,
    }
}

/// `f` as an `i64` when it is exactly integral and in range.
/// Cast round-trip only — no float libm helpers, so it compiles
/// on every no_std target (2^63 itself is excluded: it has no
/// `i64` representation).
fn integral_f64(f: f64) -> Option<i64> {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if (-TWO_63..TWO_63).contains(&f) {
        let i = f as i64;
        if i as f64 == f {
            return Some(i);
        }
    }
    None
}

/// One row's deletion with its index entries (v0.7; see
/// `Statement::delete_row_completely`), written once for its two
/// names.
macro_rules! row_delete_fn {
    (#[$m:meta] $name:ident) => {
        /// Delete one row and every index entry it owns (v0.7): the
        /// shared tail of OR REPLACE and upsert conflict handling. A
        /// rowid that is already gone is a no-op.
        #[$m]
        async fn $name(
            &mut self,
            root: u32,
            rowid_alias: Option<usize>,
            ncols: usize,
            defaults: &[OwnedValue],
            rowid: i64,
        ) -> Result<(), S::Error> {
            #[cfg(feature = "indexes")]
            {
                let indexes: AVec<IndexPlan> = match &self.plan {
                    Plan::Insert { indexes, .. } => indexes.clone(),
                    Plan::Update { indexes, .. } => indexes.clone(),
                    Plan::Delete { indexes, .. } => indexes.clone(),
                    _ => AVec::new(),
                };
                let Some(payload) = btree::seek_rowid(&mut self.conn.pager, root, rowid).await?
                else {
                    return Ok(());
                };
                let gcols = self.plan_rules().and_then(|r| r.gcols);
                for ix in indexes.iter() {
                    let key = index_key_for_row(
                        ix.cols.as_slice(),
                        rowid_alias,
                        defaults,
                        payload.as_slice(),
                        ncols,
                        rowid,
                        gcols.as_ref(),
                    )?;
                    let enc = encode_index_payload(key.as_slice(), rowid);
                    btree::index_delete(
                        &mut self.conn.pager,
                        ix.root,
                        enc.as_slice(),
                        ix.desc.as_slice(),
                    )
                    .await?;
                }
            }
            #[cfg(not(feature = "indexes"))]
            {
                let _ = (rowid_alias, ncols, defaults);
            }
            btree::btree_delete(&mut self.conn.pager, root, rowid).await?;
            Ok(())
        }
    };
}

impl<'c, S: Storage> Statement<'c, S> {
    /// A statement over its parts (v0.16, `triggers`: the statements
    /// of a trigger program, `session::trigger`; the same fields as
    /// `Connection::prepare` sets, which builds its statement in
    /// place — a call here costs the tiny profile the struct's move).
    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
    fn assemble(
        conn: &'c mut Connection<S>,
        arena: Arena,
        sql_text: crate::arena::ABytes,
        ast: Ast,
        plan: Plan,
        #[cfg(feature = "params")] binds: Binds,
    ) -> Statement<'c, S> {
        let row_ckpt = arena.checkpoint();
        Statement {
            #[cfg(feature = "params")]
            binds,
            conn,
            arena,
            sql_text,
            ast,
            plan,
            phase: Phase::Fresh,
            cursor: None,
            row_buf: AVec::new(),
            current: AVec::new(),
            current_rowid: None,
            row_ckpt,
            #[cfg(feature = "indexes")]
            index_rowids: AVec::new(),
            #[cfg(feature = "indexes")]
            index_pos: 0,
            #[cfg(feature = "indexes")]
            index_loaded: false,
            #[cfg(feature = "v04")]
            query_rows: AVec::new(),
            #[cfg(feature = "v04")]
            query_rowids: AVec::new(),
            #[cfg(feature = "v04")]
            query_pos: 0,
            #[cfg(feature = "v04")]
            query_loaded: false,
            #[cfg(feature = "v06")]
            hoist: AVec::new(),
            #[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
            ret_hoist: AVec::new(),
            #[cfg(feature = "v07")]
            ret_rows: AVec::new(),
            #[cfg(feature = "v07")]
            ret_rowids: AVec::new(),
            #[cfg(feature = "v07")]
            ret_pos: 0,
            #[cfg(feature = "v07")]
            ret_loaded: false,
            #[cfg(feature = "v16")]
            deferred: None,
            #[cfg(feature = "v16")]
            rec_rows: AVec::new(),
            #[cfg(feature = "v16")]
            mats: AVec::new(),
            #[cfg(feature = "v16")]
            stmt_sp: None,
            #[cfg(feature = "v16")]
            upsert_failed: false,
            #[cfg(feature = "v16")]
            autoinc: None,
            #[cfg(feature = "triggers")]
            trig: None,
            #[cfg(feature = "foreign_keys")]
            fk: None,
            #[cfg(feature = "foreign_keys")]
            fk_del: None,
            #[cfg(feature = "foreign_keys")]
            fk_skip: [None; 2],
        }
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    /// The write plan's table rules (v0.16), cloned out of the
    /// plan; `None` for a table with none (or a plan without).
    #[cfg(feature = "v04")]
    #[cfg_attr(not(feature = "v05"), allow(dead_code))]
    fn plan_rules(&self) -> Option<Box<crate::rules::TableRules>> {
        match &self.plan {
            Plan::Insert { rules, .. } => rules.clone(),
            #[cfg(feature = "v05")]
            Plan::Update { rules, .. } | Plan::Delete { rules, .. } => rules.clone(),
            _ => None,
        }
    }

    /// Execute/advance the statement by one step.
    ///
    /// With feature `params`, the first step of an execution
    /// after `reset` or a binding change plans the statement
    /// again, so the plan reflects the values bound, the schema
    /// and the clock at the time.
    pub async fn step(&mut self) -> Result<Step, S::Error> {
        #[cfg(feature = "params")]
        if !self.binds.started {
            self.replan().await?;
            self.binds.started = true;
        }
        // A fresh execution starts with an empty hoist cache
        // (v0.13): cached uncorrelated-subquery values belong
        // to one statement execution, never across statements
        // or across a re-execution after an error.
        #[cfg(feature = "v06")]
        if self.phase == Phase::Fresh {
            self.hoist.clear();
            #[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
            self.ret_hoist.clear();
            #[cfg(feature = "v16")]
            self.mats.clear();
        }
        #[cfg(feature = "v16")]
        let was_txn = self.conn.explicit_txn;
        let ddl_or_txn = matches!(self.plan, Plan::CreateTable | Plan::Commit | Plan::Rollback)
            || {
                #[cfg(feature = "v16")]
                {
                    matches!(
                        self.plan,
                        Plan::CreateTableAs { .. } | Plan::Maint(_) | Plan::Alter16 { .. }
                    ) || {
                        #[cfg(feature = "triggers")]
                        {
                            matches!(
                                self.plan,
                                Plan::CreateTrigger { .. } | Plan::DropTrigger { .. }
                            )
                        }
                        #[cfg(not(feature = "triggers"))]
                        {
                            false
                        }
                    }
                }
                #[cfg(not(feature = "v16"))]
                {
                    false
                }
            }
            || {
                #[cfg(feature = "indexes")]
                {
                    matches!(self.plan, Plan::CreateIndex { .. } | Plan::DropIndex { .. })
                }
                #[cfg(not(feature = "indexes"))]
                {
                    false
                }
            }
            || {
                #[cfg(feature = "v04")]
                {
                    matches!(
                        self.plan,
                        Plan::AlterAdd { .. } | Plan::CreateView | Plan::DropView { .. }
                    )
                }
                #[cfg(not(feature = "v04"))]
                {
                    false
                }
            }
            || {
                #[cfg(feature = "v05")]
                {
                    matches!(
                        self.plan,
                        Plan::Update { .. } | Plan::Delete { .. } | Plan::DropTable { .. }
                    )
                }
                #[cfg(not(feature = "v05"))]
                {
                    false
                }
            };
        #[cfg(feature = "v16")]
        self.open_stmt();
        #[cfg(feature = "foreign_keys")]
        self.fk_stmt_begin();
        let r = match &self.plan {
            #[cfg(feature = "params")]
            Plan::Deferred => Err(Error::Unsupported),
            #[cfg(feature = "v16")]
            Plan::Maint(_) => self.step_maint().await,
            #[cfg(feature = "v16")]
            Plan::Alter16 { .. } => self.step_alter16().await,
            #[cfg(feature = "v16")]
            Plan::Temp { .. } => self.step_temp().await,
            Plan::CreateTable => self.step_create().await,
            #[cfg(feature = "v16")]
            Plan::CreateTableAs { .. } => self.step_create_as().await,
            #[cfg(feature = "v04")]
            Plan::Query(_) => self.step_query().await,
            #[cfg(feature = "v04")]
            Plan::AlterAdd { .. } => self.step_alter().await,
            #[cfg(feature = "v04")]
            Plan::CreateView => self.step_create_view().await,
            #[cfg(feature = "v04")]
            Plan::DropView { .. } => self.step_drop_view().await,
            #[cfg(feature = "v16")]
            Plan::Pragma(_) => self.step_pragma().await,
            #[cfg(feature = "indexes")]
            Plan::CreateIndex { .. } => self.step_create_index().await,
            #[cfg(feature = "indexes")]
            Plan::DropIndex { .. } => self.step_drop_index().await,
            Plan::Insert { .. } => self.step_insert().await,
            #[cfg(feature = "v05")]
            Plan::Update { .. } => self.step_update().await,
            #[cfg(feature = "v05")]
            Plan::Delete { .. } => self.step_delete().await,
            #[cfg(feature = "v05")]
            Plan::DropTable { .. } => self.step_drop_table().await,
            Plan::Scan { .. } => self.step_scan().await,
            Plan::Begin => self.step_begin().await,
            #[cfg(feature = "v16")]
            Plan::BeginImmediate => {
                let s = self.step_begin().await?;
                self.conn.pager.fix_layout();
                Ok(s)
            }
            Plan::Commit => self.step_commit().await,
            #[cfg(feature = "triggers")]
            Plan::CreateTrigger { .. } => self.step_create_trigger().await,
            #[cfg(feature = "triggers")]
            Plan::DropTrigger { .. } => self.step_drop_trigger().await,
            #[cfg(feature = "triggers")]
            Plan::Instead(_) => self.step_instead().await,
            Plan::Rollback => self.step_rollback().await,
        };
        #[cfg(feature = "v16")]
        self.close_stmt(r.is_err()).await;
        #[cfg(feature = "foreign_keys")]
        self.fk_stmt_end(r.is_err(), was_txn);
        // A failure that ended the transaction (OR ROLLBACK, an
        // I/O-class error) ends the TEMP database's too.
        #[cfg(feature = "v16")]
        if r.is_err()
            && was_txn
            && !self.conn.explicit_txn
            && !matches!(self.plan, Plan::Temp { .. })
        {
            self.conn.mirror_temp(b"ROLLBACK").await;
        }
        // A transaction statement applies to the TEMP database too.
        #[cfg(feature = "v16")]
        if r.is_ok()
            && matches!(
                self.plan,
                Plan::Begin
                    | Plan::BeginImmediate
                    | Plan::Commit
                    | Plan::Rollback
                    | Plan::Maint(maint::Maint::Savepoint(_))
                    | Plan::Maint(maint::Maint::Release(_))
                    | Plan::Maint(maint::Maint::RollbackTo(_))
            )
        {
            let sql = copy_vec(handle_bytes(&self.arena, self.sql_text));
            self.conn.mirror_temp(sql.as_slice()).await;
        }
        // Keep the connection's catalog in step with the schema
        // cookie after DDL / transaction boundaries, so metadata
        // readers (tables(), indexes()) see the new state without
        // waiting for the next prepare (§5.5).
        if ddl_or_txn && r.is_ok() && self.conn.pager.schema_cookie() != self.conn.catalog.cookie {
            self.conn.rebuild_catalog().await?;
        }
        r
    }

    /// Re-plan before an execution when the plan is stale (v0.16,
    /// `params`; see `Binds::stale`). The catalog is brought up
    /// to date first (a re-executed statement may have changed
    /// the schema itself, or rolled a change back), and the
    /// statement arena is rewound to its pre-plan mark, so
    /// repeated executions reuse the same arena space.
    #[cfg(feature = "params")]
    async fn replan(&mut self) -> Result<(), S::Error> {
        if !self.binds.stale {
            return Ok(());
        }
        // The TEMP database plans its own statements.
        #[cfg(feature = "v16")]
        if matches!(self.plan, Plan::Temp { .. }) {
            self.binds.stale = false;
            return Ok(());
        }
        if self.conn.pager.in_commit() {
            return Err(Error::OperationInFlight);
        }
        if self.conn.pager.schema_cookie() != self.conn.catalog.cookie {
            self.conn.rebuild_catalog().await?;
        }
        self.plan = Plan::Deferred;
        self.arena.rewind(self.binds.plan_ckpt);
        if let Some(src) = &self.binds.src {
            self.ast = crate::sql::bind::bind_ast::<S::Error>(
                src,
                self.binds.values.as_slice(),
                &self.arena,
            )?;
        }
        self.plan = self.conn.plan(&self.arena, &self.ast)?;
        self.binds.stale = false;
        self.row_ckpt = self.arena.checkpoint();
        Ok(())
    }

    /// The number of parameters (v0.16, `params`): the largest
    /// parameter index in the statement — `SELECT ?5` has 5 —
    /// as the pin's `sqlite3_bind_parameter_count`.
    #[cfg(feature = "params")]
    pub fn parameter_count(&self) -> usize {
        usize::from(self.binds.table.count)
    }

    /// The index of the parameter spelled exactly `name`,
    /// sigil included (`":a"`, `"$x"`, `"?3"`), as the pin's
    /// `sqlite3_bind_parameter_index`; `None` when no parameter
    /// has that spelling. Names are case-sensitive, and a
    /// numbered parameter answers only to its own spelling
    /// (`"?01"` is not `"?1"`).
    #[cfg(feature = "params")]
    pub fn parameter_index(&self, name: &str) -> Option<usize> {
        self.binds
            .table
            .index_of(&self.arena, name.as_bytes())
            .map(usize::from)
    }

    /// The name of parameter `i` (1-based) as written — `":a"`,
    /// `"?2"` for an explicitly numbered one — or `None` for a
    /// nameless `?` or an index the statement does not have (the
    /// pin's `sqlite3_bind_parameter_name`).
    #[cfg(feature = "params")]
    pub fn parameter_name(&self, i: usize) -> Option<&str> {
        let i = u16::try_from(i).ok()?;
        let h = self.binds.table.name_of(i)?;
        core::str::from_utf8(handle_bytes(&self.arena, h)).ok()
    }

    /// Bind parameter `i` (1-based) to `v` (v0.16, `params`).
    /// The value takes part in affinity exactly as a literal of
    /// the same storage class: no affinity of its own, the
    /// column's applied on store and comparison. Allowed only
    /// before the first `step` or after `reset`
    /// (`Error::ResetRequired`); `i` outside 1 ..=
    /// `parameter_count()` is `Error::BindIndexOutOfRange`.
    #[cfg(feature = "params")]
    pub fn bind_value(&mut self, i: usize, v: Value<'_>) -> Result<(), S::Error> {
        if self.binds.started {
            return Err(Error::ResetRequired);
        }
        let slot = i
            .checked_sub(1)
            .and_then(|k| self.binds.values.as_mut_slice().get_mut(k))
            .ok_or(Error::BindIndexOutOfRange { index: i })?;
        *slot = match v {
            // The pin stores a NaN double as NULL.
            Value::Real(f) if f.is_nan() => OwnedValue::Null,
            v => OwnedValue::from_value(v),
        };
        self.binds.stale = true;
        Ok(())
    }

    /// Bind parameter `i` to NULL (see `bind_value`).
    #[cfg(feature = "params")]
    pub fn bind_null(&mut self, i: usize) -> Result<(), S::Error> {
        self.bind_value(i, Value::Null)
    }

    /// Bind parameter `i` to an INTEGER (see `bind_value`).
    #[cfg(feature = "params")]
    pub fn bind_i64(&mut self, i: usize, v: i64) -> Result<(), S::Error> {
        self.bind_value(i, Value::Integer(v))
    }

    /// Bind parameter `i` to a REAL (see `bind_value`); a NaN
    /// binds NULL, as on the pin.
    #[cfg(feature = "params")]
    pub fn bind_f64(&mut self, i: usize, v: f64) -> Result<(), S::Error> {
        self.bind_value(i, Value::Real(v))
    }

    /// Bind parameter `i` to TEXT (see `bind_value`). The bytes
    /// are copied; an embedded NUL is kept, as on the pin. They
    /// count against the statement arena when the statement is
    /// planned, as the same literal in the SQL text would.
    #[cfg(feature = "params")]
    pub fn bind_text(&mut self, i: usize, v: &str) -> Result<(), S::Error> {
        self.bind_value(i, Value::Text(v.as_bytes()))
    }

    /// Bind parameter `i` to a BLOB (see `bind_text`).
    #[cfg(feature = "params")]
    pub fn bind_blob(&mut self, i: usize, v: &[u8]) -> Result<(), S::Error> {
        self.bind_value(i, Value::Blob(v))
    }

    /// Reset every parameter to NULL (v0.16, `params`; the pin's
    /// `sqlite3_clear_bindings`). Allowed at any time; a running
    /// execution keeps the values it started with, the next one
    /// (after `reset`) sees NULLs.
    #[cfg(feature = "params")]
    pub fn clear_bindings(&mut self) {
        for v in self.binds.values.as_mut_slice() {
            *v = OwnedValue::Null;
        }
        self.binds.stale = true;
    }

    /// Return the statement to its prepared state (v0.16,
    /// `params`; the pin's `sqlite3_reset`), so the next `step`
    /// executes it again from the start — with the bindings in
    /// force then, which `reset` keeps. Any unread rows are
    /// dropped; a write statement that already completed is not
    /// undone. Bindings may change only while reset.
    #[cfg(feature = "params")]
    pub fn reset(&mut self) {
        self.binds.started = false;
        self.binds.stale = true;
        self.phase = Phase::Fresh;
        self.cursor = None;
        self.row_buf.clear();
        self.current.clear();
        self.current_rowid = None;
        #[cfg(feature = "indexes")]
        {
            self.index_rowids.clear();
            self.index_pos = 0;
            self.index_loaded = false;
        }
        #[cfg(feature = "v04")]
        {
            self.query_rows.clear();
            self.query_rowids.clear();
            self.query_pos = 0;
            self.query_loaded = false;
        }
        #[cfg(feature = "v06")]
        self.hoist.clear();
        #[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
        self.ret_hoist.clear();
        #[cfg(feature = "v16")]
        {
            self.deferred = None;
            self.mats.clear();
        }
        #[cfg(feature = "v07")]
        {
            self.ret_rows.clear();
            self.ret_rowids.clear();
            self.ret_pos = 0;
            self.ret_loaded = false;
        }
        self.arena.rewind(self.row_ckpt);
    }

    /// `BEGIN` (v0.2): opens the explicit bracket only — the pager
    /// transaction itself opens lazily at the first write, so a
    /// read-only transaction never touches the journal (as SQLite).
    async fn step_begin(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if self.conn.explicit_txn {
            return Err(Error::TransactionState { nested: true });
        }
        self.conn.explicit_txn = true;
        self.phase = Phase::Done;
        Ok(Step::Done)
    }

    /// `COMMIT` (v0.2): drives the pager's resumable commit machine.
    /// A storage error leaves the bracket open and the stage
    /// resumable (`resume_txn` / a retry finishes it) — the flag is
    /// only cleared once the commit point has passed.
    async fn step_commit(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.conn.explicit_txn {
            return Err(Error::TransactionState { nested: false });
        }
        // Outstanding deferred foreign-key violations fail COMMIT,
        // the transaction staying open (v0.16, `foreign_keys`).
        #[cfg(feature = "foreign_keys")]
        self.conn.fk_commit_check()?;
        self.conn.pager.drive_commit().await?;
        // Every savepoint ends with the transaction (v0.16, `v16`).
        #[cfg(feature = "v16")]
        self.conn.pager.sp_clear();
        self.conn.explicit_txn = false;
        self.phase = Phase::Done;
        Ok(Step::Done)
    }

    /// `ROLLBACK` (v0.2): the pager's rollback restores the
    /// pre-image; a read-only bracket (pager still idle) is a no-op
    /// at the pager level.
    async fn step_rollback(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.conn.explicit_txn {
            return Err(Error::TransactionState { nested: false });
        }
        self.conn.pager.rollback().await?;
        self.conn.explicit_txn = false;
        self.phase = Phase::Done;
        Ok(Step::Done)
    }

    async fn step_create(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let Ast::CreateTable {
            name,
            if_not_exists,
            columns,
            ..
        } = &self.ast
        else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        // One namespace for tables, views and indexes (v0.16 fix,
        // ungated: hipp created a table over a view or an index of
        // its name). An index of the name errors even under IF NOT
        // EXISTS, as on the pin.
        let taken = self
            .conn
            .catalog
            .taken(&self.conn.catalog_arena, name_b.as_slice());
        if taken == Some(true) {
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateIndex,
            });
        }
        if taken.is_some() {
            if *if_not_exists {
                self.phase = Phase::Done;
                return Ok(Step::Done);
            }
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateTable,
            });
        }
        let _ = columns;
        // Stored schema text: the statement as issued, minus any
        // trailing semicolon/whitespace (SQLite's convention);
        // AUTOINCREMENT is refused there (v0.16).
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text::<S::Error>(raw, &self.arena)?;
        // AUTOINCREMENT (v0.16, `v16`): only on the rowid alias.
        #[cfg(feature = "v16")]
        let autoinc =
            crate::sql::parser::has_autoincrement::<S::Error>(sql_text.as_slice(), &self.arena);
        #[cfg(feature = "v16")]
        if autoinc && !create_has_rowid_alias(&self.ast, &self.arena) {
            return Err(Error::Schema {
                kind: SchemaKind::AutoincrementNotIntegerPk,
            });
        }
        #[cfg(feature = "v16")]
        let r = self
            .exec_create(name_b.as_slice(), sql_text.as_slice(), autoinc)
            .await;
        #[cfg(not(feature = "v16"))]
        let r = self
            .exec_create(name_b.as_slice(), sql_text.as_slice())
            .await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                // §7.5.4: a failed write leaves no trace (see
                // `abort_write`).
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    /// `CREATE TABLE ... AS SELECT` (v0.16, `v16`): an existing
    /// name is an error (a no-op under IF NOT EXISTS, the SELECT
    /// not run); otherwise the SELECT's rows are materialised,
    /// the table is created with the derived schema text, and
    /// the rows are stored in result order under rowids 1, 2,
    /// ... with each new column's affinity applied — one atomic
    /// statement.
    #[cfg(feature = "v16")]
    async fn step_create_as(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let Ast::CreateTable {
            name,
            if_not_exists,
            ..
        } = &self.ast
        else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        let taken = self
            .conn
            .catalog
            .taken(&self.conn.catalog_arena, name_b.as_slice());
        if taken == Some(true) {
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateIndex,
            });
        }
        if taken.is_some() {
            if *if_not_exists {
                self.phase = Phase::Done;
                return Ok(Step::Done);
            }
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateTable,
            });
        }
        let r = self.exec_create_as(name_b.as_slice()).await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "v16")]
    async fn exec_create_as(&mut self, name: &[u8]) -> Result<(), S::Error> {
        if matches!(&self.plan, Plan::CreateTableAs { select: None, .. }) {
            // The name was taken at plan time and is free now
            // (another connection's schema change): plan again,
            // as the pin re-prepares on a schema change.
            self.plan = self.conn.plan(&self.arena, &self.ast)?;
        }
        let (select, sql, affs) = match &self.plan {
            Plan::CreateTableAs {
                select: Some(select),
                sql,
                affs,
            } => (select.clone(), sql.clone(), affs.clone()),
            _ => return Err(Error::Unsupported),
        };
        let rows = self.exec_subplan(&select, &RowData::empty()).await?;
        let (root, _) = self.conn.pager.alloc_page().await?;
        btree::create_empty_root(&mut self.conn.pager, root).await?;
        self.insert_schema_row(b"table", name, name, root, Some(sql.as_slice()))
            .await?;
        let mut rowid = 0i64;
        for row in rows.iter() {
            rowid = rowid.checked_add(1).ok_or(Error::Schema {
                kind: SchemaKind::DatatypeMismatch,
            })?;
            let mut rec = AVec::new();
            for (v, a) in row.iter().zip(affs.iter()) {
                rec.push(apply_affinity(v.clone(), *a));
            }
            let bytes = crate::rules::encode_rec(rec.as_slice(), None, None);
            btree::btree_insert(&mut self.conn.pager, root, rowid, bytes.as_slice()).await?;
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// Append one row to `sqlite_schema` (root page 1) with the
    /// next schema rowid.
    async fn insert_schema_row(
        &mut self,
        ty: &[u8],
        name: &[u8],
        tbl_name: &[u8],
        root: u32,
        sql: Option<&[u8]>,
    ) -> Result<(), S::Error> {
        let pager = &mut self.conn.pager;
        // At the largest rowid: v16 takes an unused one, as the pin
        // does; without it the insert fails as a duplicate (v0.16
        // review fix: the next rowid was computed unchecked).
        #[cfg(feature = "v16")]
        let rowid = btree::new_rowid(pager, 1).await?;
        #[cfg(not(feature = "v16"))]
        let rowid = match btree::seek_max_rowid(pager, 1).await? {
            Some(m) => m.saturating_add(1),
            None => 1,
        };
        let mut vals: AVec<Value<'_>> = AVec::new();
        vals.push(Value::Text(ty));
        vals.push(Value::Text(name));
        vals.push(Value::Text(tbl_name));
        vals.push(Value::Integer(i64::from(root)));
        match sql {
            Some(s) => vals.push(Value::Text(s)),
            None => vals.push(Value::Null),
        }
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        btree::btree_insert(pager, 1, rowid, rec.as_slice()).await
    }

    async fn exec_create(
        &mut self,
        name: &[u8],
        sql_text: &[u8],
        #[cfg(feature = "v16")] autoinc: bool,
    ) -> Result<(), S::Error> {
        let (root, _) = self.conn.pager.alloc_page().await?;
        btree::create_empty_root(&mut self.conn.pager, root).await?;
        self.insert_schema_row(b"table", name, name, root, Some(sql_text))
            .await?;
        // Implicit indexes for the table's UNIQUE / PK constraint
        // sets (v0.3), in statement order, exactly as SQLite
        // creates them.
        #[cfg(feature = "indexes")]
        self.create_auto_indexes(name).await?;
        // The first AUTOINCREMENT table brings `sqlite_sequence`
        // (v0.16, `v16`): its schema row follows the table's
        // automatic indexes, with the pin's text, as on the pin.
        #[cfg(feature = "v16")]
        if autoinc
            && self
                .conn
                .catalog
                .find(&self.conn.catalog_arena, b"sqlite_sequence")
                .is_none()
        {
            let (sroot, _) = self.conn.pager.alloc_page().await?;
            btree::create_empty_root(&mut self.conn.pager, sroot).await?;
            self.insert_schema_row(
                b"table",
                b"sqlite_sequence",
                b"sqlite_sequence",
                sroot,
                Some(b"CREATE TABLE sqlite_sequence(name,seq)"),
            )
            .await?;
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// Create the implicit index B-trees + schema rows for a new
    /// table's constraint sets (v0.3). The rowid-alias PK set is
    /// skipped — it needs no index.
    #[cfg(feature = "indexes")]
    async fn create_auto_indexes(&mut self, table: &[u8]) -> Result<(), S::Error> {
        // Phase 1: compute the implicit index names from the AST
        // (borrows only the statement; the pager work follows).
        let names: AVec<AVec<u8>> = {
            let Ast::CreateTable {
                columns,
                pk_columns,
                constraint_sets,
                ..
            } = &self.ast
            else {
                unreachable!()
            };
            // Rowid alias determination, mirroring schema.rs.
            let mut alias = false;
            if pk_columns.len() == 1 {
                let target = handle_bytes(&self.arena, pk_columns.as_slice()[0]);
                for c in columns.iter() {
                    if eq_ci(handle_bytes(&self.arena, c.name), target) {
                        let decl = handle_bytes(&self.arena, c.decl);
                        alias = !c.pk_desc && eq_ci(crate::record::decl_token(decl), b"INTEGER");
                    }
                }
            }
            let mut out: AVec<AVec<u8>> = AVec::new();
            let mut n = 0usize;
            for set in constraint_sets.iter() {
                if set.primary_key && alias {
                    continue;
                }
                n += 1;
                let mut auto_name = AVec::new();
                for b in b"sqlite_autoindex_" {
                    auto_name.push(*b);
                }
                for b in table {
                    auto_name.push(*b);
                }
                auto_name.push(b'_');
                crate::record::render_int(n as i64, &mut auto_name);
                out.push(auto_name);
            }
            out
        };
        // Phase 2: one empty index B-tree + schema row per name.
        for auto_name in names.iter() {
            let (root, _) = self.conn.pager.alloc_page().await?;
            btree::create_empty_index_root(&mut self.conn.pager, root).await?;
            self.insert_schema_row(b"index", auto_name.as_slice(), table, root, None)
                .await?;
        }
        Ok(())
    }

    /// `CREATE [UNIQUE] INDEX` (v0.3): allocate the root, write
    /// the schema row, then populate the index from the table's
    /// existing rows. A UNIQUE violation during population fails
    /// the statement (and, per the engine's write-error rule,
    /// rolls the transaction back — the half-built index and its
    /// schema row vanish with it).
    #[cfg(feature = "indexes")]
    async fn step_create_index(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_create_index().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "indexes")]
    async fn exec_create_index(&mut self) -> Result<(), S::Error> {
        let Ast::CreateIndex {
            name,
            table,
            if_not_exists,
            ..
        } = &self.ast
        else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        let table_b = copy_vec(handle_bytes(&self.arena, *table));
        // Index names share the schema namespace with tables and
        // views: a table or view of the name errors even under IF
        // NOT EXISTS, as on the pin (v0.16 fix, ungated: a view's
        // name was free, a table's a silent no-op).
        let taken = self
            .conn
            .catalog
            .taken(&self.conn.catalog_arena, name_b.as_slice());
        if taken == Some(false) {
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateTable,
            });
        }
        if taken == Some(true) {
            if *if_not_exists {
                return Ok(());
            }
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateIndex,
            });
        }
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text::<S::Error>(raw, &self.arena)?;
        let (root, _) = self.conn.pager.alloc_page().await?;
        btree::create_empty_index_root(&mut self.conn.pager, root).await?;
        self.insert_schema_row(
            b"index",
            name_b.as_slice(),
            table_b.as_slice(),
            root,
            Some(sql_text.as_slice()),
        )
        .await?;
        // Populate from the table's current contents.
        let Plan::CreateIndex {
            table_root,
            rowid_alias,
            ncols,
            defaults,
            cols,
            unique,
            ..
        } = &self.plan
        else {
            unreachable!()
        };
        let (table_root, rowid_alias, ncols, unique) = (*table_root, *rowid_alias, *ncols, *unique);
        let defaults = defaults.clone();
        let cols = cols.clone();
        #[cfg(feature = "v04")]
        let gcols = match &self.plan {
            Plan::CreateIndex { gcols, .. } => gcols.clone(),
            _ => None,
        };
        let mut desc = AVec::new();
        for c in cols.iter() {
            desc.push(c.order());
        }
        let mut cursor = ScanCursor::new(table_root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let key = index_key_for_row(
                cols.as_slice(),
                rowid_alias,
                defaults.as_slice(),
                buf.as_slice(),
                ncols,
                rowid,
                #[cfg(feature = "v04")]
                gcols.as_deref(),
            )?;
            if unique && key_has_no_null(key.as_slice()) {
                let probe: AVec<OwnedValue> = key.clone();
                if btree::index_first_with_prefix(
                    &mut self.conn.pager,
                    root,
                    desc.as_slice(),
                    probe.as_slice(),
                )
                .await?
                .is_some()
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::UniqueViolation,
                    });
                }
            }
            let payload = encode_index_payload(key.as_slice(), rowid);
            btree::index_insert(
                &mut self.conn.pager,
                root,
                payload.as_slice(),
                desc.as_slice(),
            )
            .await?;
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// `DROP INDEX` (v0.3): free every page of the index B-tree
    /// (pages + overflow chains) to the freelist, then remove its
    /// schema row.
    #[cfg(feature = "indexes")]
    async fn step_drop_index(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_drop_index().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "indexes")]
    async fn exec_drop_index(&mut self) -> Result<(), S::Error> {
        let Plan::DropIndex { root } = &self.plan else {
            unreachable!()
        };
        let Some(root) = *root else {
            return Ok(()); // IF EXISTS no-op
        };
        let Ast::DropIndex { name, .. } = &self.ast else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        // Locate the schema row (its rowid keys the removal).
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        let mut schema_rowid = None;
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
            if values.len() >= 2
                && matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"index"))
                && let Value::Text(n) = &values.as_slice()[1]
                && eq_ci(n, name_b.as_slice())
            {
                schema_rowid = Some((rowid, copy_vec(n)));
                break;
            }
        }
        let Some((schema_rowid, stored)) = schema_rowid else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownIndex,
            });
        };
        self.clear_stat_rows(1, stored.as_slice()).await?;
        // Collect first, then free: freeing rewrites the freelist
        // linkage the traversal is not using, but the traversal
        // must see the intact tree.
        let pages = btree::collect_index_pages(&mut self.conn.pager, root).await?;
        for p in pages.iter() {
            self.conn.pager.free_page(*p).await?;
        }
        btree::btree_delete(&mut self.conn.pager, 1, schema_rowid).await?;
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// Whether this statement carries a v0.7 `RETURNING` clause
    /// with at least one result expression.
    #[cfg(feature = "v07")]
    fn has_returning(&self) -> bool {
        match &self.plan {
            Plan::Insert { returning, .. } => !returning.is_empty(),
            Plan::Update { returning, .. } => !returning.is_empty(),
            Plan::Delete { returning, .. } => !returning.is_empty(),
            #[cfg(feature = "triggers")]
            Plan::Instead(ip) => ip.has_returning(),
            _ => false,
        }
    }

    /// `serve_returning`, and after the last row an autocommit
    /// write's deferred foreign-key violations (v0.16,
    /// `foreign_keys`): the pin checks them when the statement
    /// halts, after its RETURNING rows (its immediate ones before).
    #[cfg(feature = "v07")]
    async fn serve_returning_fk(&mut self) -> Result<Step, S::Error> {
        let step = self.serve_returning();
        #[cfg(feature = "foreign_keys")]
        if step == Step::Done && core::mem::take(&mut self.conn.fk.late) {
            self.conn.env.finish(false);
            let _ = self.conn.pager.rollback().await;
            self.conn.explicit_txn = false;
            return Err(Error::Schema {
                kind: SchemaKind::ForeignKeyViolation,
            });
        }
        Ok(step)
    }

    /// Serve one buffered RETURNING row (v0.7), query-style.
    #[cfg(feature = "v07")]
    fn serve_returning(&mut self) -> Step {
        if self.ret_pos >= self.ret_rows.len() {
            self.phase = Phase::Done;
            self.current_rowid = None;
            return Step::Done;
        }
        self.current = self.ret_rows.as_slice()[self.ret_pos].clone();
        self.current_rowid = self.ret_rowids.as_slice()[self.ret_pos];
        self.ret_pos += 1;
        self.phase = Phase::Running;
        Step::Row
    }

    /// Evaluate RETURNING expressions for one affected row
    /// (v0.7): subqueries execute against the row at hand via
    /// the v0.6 substitution path, exactly as in a SELECT list.
    ///
    /// v0.16 review fix: the pin runs RETURNING after each row's
    /// change, as a trigger program, so its subqueries see the
    /// table as changed so far and are never hoisted across rows:
    /// each row evaluates them afresh (`hoist` is set aside
    /// meanwhile). A shared CTE materialization is still computed
    /// once per statement, as on the pin (its `OP_Once` holds
    /// across the rows). So is an uncorrelated subquery that does
    /// not read the written table (v0.16 review fix, `triggers` /
    /// `foreign_keys`: the pin computes it once, when a row first
    /// reaches it, so the later writes of triggers or foreign-key
    /// actions to its tables do not show — `ret_hoist`).
    #[cfg(feature = "v07")]
    async fn eval_returning(
        &mut self,
        exprs: &[RExpr],
        rd: &RowData,
    ) -> Result<AVec<OwnedValue>, S::Error> {
        let hoist = core::mem::take(&mut self.hoist);
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        {
            self.hoist = core::mem::take(&mut self.ret_hoist);
        }
        let mut out = AVec::new();
        let mut r = Ok(());
        for e in exprs {
            match self.eval_row_sub(e, rd).await {
                Ok(v) => out.push(v),
                Err(err) => {
                    r = Err(err);
                    break;
                }
            }
        }
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        {
            let root = match &self.plan {
                Plan::Insert { root, .. }
                | Plan::Update { root, .. }
                | Plan::Delete { root, .. } => Some(*root),
                _ => None,
            };
            let mut per_row = AVec::new();
            if let Some(root) = root {
                for e in exprs {
                    crate::query::subs_reading(e, root, &mut per_row);
                }
            }
            // (A view written through INSTEAD OF triggers has no
            // root: its RETURNING subqueries are all cached.)
            let cached = core::mem::take(&mut self.hoist);
            let mut kept = AVec::new();
            for (k, v) in cached.iter() {
                if !per_row.iter().any(|p| *p == k.0) {
                    kept.push((*k, v.clone()));
                }
            }
            self.ret_hoist = kept;
        }
        self.hoist = hoist;
        r.map(|()| out)
    }

    /// Delete one row and every index entry it owns (v0.7):
    /// the shared tail of OR REPLACE and upsert conflict
    /// handling. A rowid that is already gone is a no-op.
    #[cfg(all(feature = "v07", feature = "foreign_keys"))]
    async fn delete_row_completely(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: &[OwnedValue],
        rowid: i64,
    ) -> Result<(), S::Error> {
        // v0.16 (`foreign_keys`): a REPLACE victim's keys, as a
        // DELETE's (the pin deletes it through `GenerateRowDelete`).
        let fk_main = match self.fk_del.clone() {
            Some(d) => self.fk.replace(d),
            None => self.fk.clone(),
        };
        let r = self
            .delete_victim(root, rowid_alias, ncols, defaults, rowid)
            .await;
        self.fk = fk_main;
        r
    }

    /// `delete_row_completely` with the victim's foreign keys
    /// (v0.16, `foreign_keys`).
    #[cfg(feature = "foreign_keys")]
    async fn delete_victim(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: &[OwnedValue],
        rowid: i64,
    ) -> Result<(), S::Error> {
        let fk_old = if self.fk_active() {
            let Some(r) = self.reload_write_row(root, ncols, defaults, rowid).await? else {
                return Ok(());
            };
            let affs = self.plan_affs();
            let lrow = self.fk_row(r.rec.as_slice(), rowid, affs.as_slice());
            self.fk_old_row(lrow.as_slice(), rowid, None).await?;
            Some(lrow)
        } else {
            None
        };
        self.delete_row_raw(root, rowid_alias, ncols, defaults, rowid)
            .await?;
        if let Some(lrow) = &fk_old {
            self.fk_actions(lrow.as_slice(), None, None).await?;
        }
        Ok(())
    }

    // The row deletion itself: `delete_row_completely` without
    // `foreign_keys` (as it always was), its `delete_row_raw` with.
    row_delete_fn!(
        #[cfg(all(feature = "v07", feature = "foreign_keys"))]
        delete_row_raw
    );
    row_delete_fn!(
        #[cfg(all(feature = "v07", not(feature = "foreign_keys")))]
        delete_row_completely
    );

    async fn step_insert(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        #[cfg(feature = "v07")]
        if self.has_returning() && !self.ret_loaded {
            match self.exec_insert().await {
                Ok(()) => {
                    self.ret_loaded = true;
                    self.ret_pos = 0;
                }
                Err(e) => return Err(self.write_failed(e).await),
            }
        }
        #[cfg(feature = "v07")]
        if self.has_returning() {
            return self.serve_returning_fk().await;
        }
        let r = self.exec_insert().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            // Same abort rule as CREATE (see step_create), but for
            // OR FAIL (`write_failed`).
            #[cfg(feature = "v05")]
            Err(e) => Err(self.write_failed(e).await),
            // No OR FAIL without v0.5 / v0.7: the abort rule alone
            // (inline, as the tiny profile has always built it).
            #[cfg(not(feature = "v05"))]
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    /// The end of an INSERT / UPDATE that failed with `e`: the
    /// transaction is rolled back (the abort rule of
    /// `step_create`) — except that under OR FAIL a constraint
    /// violation keeps the rows the statement already changed, as
    /// on the pin (v0.16 review fix, ungated: hipp treated FAIL as
    /// ABORT, so `INSERT OR FAIL INTO t VALUES (2,'b'), (3,'a')`
    /// over a UNIQUE 'a' lost row 2). Conflicts are found before a
    /// row is written, so no partial row is kept. In autocommit the
    /// kept rows are committed; an explicit transaction stays open.
    /// `changes()` counts the kept rows.
    /// (OR FAIL parses only with UPDATE (v0.5) / INSERT OR (v0.7).)
    #[cfg(feature = "v05")]
    async fn write_failed(
        &mut self,
        #[allow(unused_mut)] mut e: Error<S::Error>,
    ) -> Error<S::Error> {
        #[cfg(feature = "triggers")]
        if self.trigger_failed(&mut e).await {
            return e;
        }
        let conflict = match &self.plan {
            Plan::Insert { conflict, .. } | Plan::Update { conflict, .. } => *conflict,
            _ => ConflictAction::Abort,
        };
        match self.fail_in_place(conflict, &e).await {
            Ok(true) => return e,
            Ok(false) => {}
            Err(ce) => return ce,
        }
        #[cfg(feature = "v16")]
        self.conn.env.finish(false);
        abort_write!(self, &e);
        e
    }

    /// The OR FAIL half of `write_failed` (v0.5+): whether the
    /// statement's changes were kept (committed in autocommit), or
    /// the commit's own error (the transaction then rolled back).
    #[cfg(feature = "v05")]
    async fn fail_in_place(
        &mut self,
        conflict: ConflictAction,
        e: &Error<S::Error>,
    ) -> core::result::Result<bool, Error<S::Error>> {
        let keep = conflict == ConflictAction::Fail
            && matches!(
                e,
                Error::Schema {
                    kind: SchemaKind::UniqueViolation
                        | SchemaKind::DuplicateRowid
                        | SchemaKind::NotNullViolation
                        | SchemaKind::CheckViolation
                }
            );
        if keep {
            // v0.16 (`foreign_keys`): an action program's failure
            // aborts the statement whatever its clause says; and the
            // pin's OR FAIL halt still checks the statement's
            // foreign-key violations (in autocommit the
            // transaction's too), undoing it when there are any.
            #[cfg(feature = "foreign_keys")]
            {
                if self.conn.fk.abort {
                    return Ok(false);
                }
                if let Err(fe) = self.fk_end() {
                    self.conn.env.finish(false);
                    abort_write!(self, &fe);
                    return Err(fe);
                }
            }
            let committed = if self.conn.explicit_txn {
                Ok(())
            } else {
                self.conn.pager.drive_commit().await
            };
            return match committed {
                Ok(()) => {
                    // A failed statement of a trigger program counts
                    // nothing (the pin's count is taken when a
                    // program's statement completes).
                    #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
                    let kept = !self.conn.in_program();
                    #[cfg(not(any(feature = "triggers", feature = "foreign_keys")))]
                    let kept = true;
                    #[cfg(feature = "v16")]
                    self.conn.env.finish(kept);
                    let _ = kept;
                    #[cfg(feature = "v16")]
                    self.keep_stmt();
                    Ok(true)
                }
                Err(ce) => {
                    #[cfg(feature = "v16")]
                    self.conn.env.finish(false);
                    abort_write!(self, &ce);
                    Err(ce)
                }
            };
        }
        Ok(false)
    }

    async fn exec_insert(&mut self) -> Result<(), S::Error> {
        #[cfg(feature = "v16")]
        self.conn.env.pending.set(0);
        #[cfg(feature = "triggers")]
        self.load_triggers()?;
        #[cfg(feature = "foreign_keys")]
        {
            let multi = self.insert_multi();
            self.fk_load(fkey::Event::Insert, multi)?;
        }
        #[cfg(feature = "v16")]
        self.autoinc_begin().await?;
        // v0.16: VALUES rows that read last_insert_rowid() evaluate
        // one at a time, each just before its own insert — on the
        // pin each row sees the rowid the row before it inserted.
        // (v0.16 review fix, `triggers`: not when the target has
        // INSERT triggers — the pin then computes every VALUES row
        // into a temporary table first, so a RAISE(FAIL) in a later
        // row of a trigger program's INSERT keeps none.)
        #[cfg(feature = "triggers")]
        let staged = self.fires(
            crate::sql::ast::TrigTime::Before,
            crate::sql::ast::TrigEvent::Insert,
        ) || self.fires(
            crate::sql::ast::TrigTime::After,
            crate::sql::ast::TrigEvent::Insert,
        );
        #[cfg(all(feature = "v16", not(feature = "triggers")))]
        let staged = false;
        #[cfg(feature = "v16")]
        let lazy = match &self.plan {
            Plan::Insert {
                rows_r: Some(rows_r),
                select: None,
                ..
            } if !staged
                && (rows_r.iter().any(|r| r.iter().any(reads_last_rowid))
                    || !rows_r.iter().any(|r| r.iter().any(crate::query::has_sub))) =>
            {
                Some(rows_r.clone())
            }
            _ => None,
        };
        #[cfg(feature = "v16")]
        if let Some(rows_r) = lazy {
            let empty = RowData::empty();
            for rr in rows_r.iter() {
                let mut rv = AVec::new();
                for e in rr.iter() {
                    rv.push(self.eval_row_sub(e, &empty).await?);
                }
                self.exec_insert_row(rv).await?;
            }
            self.autoinc_end().await?;
            #[cfg(feature = "foreign_keys")]
            self.fk_end_commit().await?;
            #[cfg(not(feature = "foreign_keys"))]
            if !self.conn.explicit_txn {
                self.conn.pager.drive_commit().await?;
            }
            self.conn.env.finish(true);
            return Ok(());
        }
        // The full row source is materialised BEFORE the first
        // row is inserted (v0.8): an INSERT ... SELECT reading
        // its own target terminates on the pre-statement rows,
        // and subqueries in VALUES observe the pre-statement
        // table for every row — both oracle-probed.
        #[cfg(feature = "triggers")]
        let (rows, failed) = match self.insert_source_rows().await {
            Ok(r) => (r, None),
            Err(e) => match self.insert_prefix().await? {
                Some((p, true)) => (p, Some(e)),
                Some((p, false)) => (p, None),
                None => return Err(e),
            },
        };
        #[cfg(not(feature = "triggers"))]
        let rows = self.insert_source_rows().await?;
        for values in rows.iter() {
            self.exec_insert_row(values.clone()).await?;
        }
        #[cfg(feature = "v16")]
        self.autoinc_end().await?;
        // The rows a trigger program's INSERT ... SELECT streamed
        // before its SELECT failed stay (`insert_prefix`).
        #[cfg(feature = "triggers")]
        if let Some(e) = failed {
            return Err(self.failed_part_way(e));
        }
        #[cfg(feature = "foreign_keys")]
        self.fk_end_commit().await?;
        #[cfg(not(feature = "foreign_keys"))]
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.finish(true);
        Ok(())
    }

    /// Read the target's `sqlite_sequence` row when the INSERT's
    /// table is AUTOINCREMENT (v0.16, `v16`; see `Autoinc`).
    #[cfg(feature = "v16")]
    async fn autoinc_begin(&mut self) -> Result<(), S::Error> {
        self.autoinc = None;
        let name = match &self.plan {
            Plan::Insert {
                autoinc: Some(n), ..
            } => n.clone(),
            _ => return Ok(()),
        };
        let seq_root = self
            .conn
            .catalog
            .find(&self.conn.catalog_arena, b"sqlite_sequence")
            .ok_or(Error::Schema {
                kind: SchemaKind::UnknownTable,
            })?
            .root;
        let mut ai = Autoinc {
            name,
            seq_root,
            seq_rowid: None,
            counter: 0,
            orig: None,
        };
        let mut cursor = ScanCursor::new(seq_root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 2)?;
            if !matches!(values.as_slice().first(), Some(Value::Text(n)) if *n == ai.name.as_slice())
            {
                continue;
            }
            let seq = values
                .as_slice()
                .get(1)
                .map(|v| OwnedValue::from_value(*v))
                .unwrap_or(OwnedValue::Null);
            ai.counter = match crate::query::cast_value(&seq, crate::record::Affinity::Integer) {
                OwnedValue::Integer(i) => i,
                _ => 0,
            };
            ai.seq_rowid = Some(rowid);
            ai.orig = Some(ai.counter);
            break;
        }
        self.autoinc = Some(ai);
        Ok(())
    }

    /// Write the AUTOINCREMENT counter back to `sqlite_sequence`
    /// at the end of a successful INSERT (v0.16, `v16`): a new row
    /// (next rowid) when the table had none, else its row rewritten
    /// in place when the counter grew.
    #[cfg(feature = "v16")]
    async fn autoinc_end(&mut self) -> Result<(), S::Error> {
        let Some(ai) = self.autoinc.take() else {
            return Ok(());
        };
        if ai.orig.is_some_and(|o| ai.counter <= o) {
            return Ok(());
        }
        let pager = &mut self.conn.pager;
        let rowid = match ai.seq_rowid {
            Some(r) => {
                btree::btree_delete(pager, ai.seq_root, r).await?;
                r
            }
            None => btree::new_rowid(pager, ai.seq_root).await?,
        };
        let mut vals: AVec<Value<'_>> = AVec::new();
        vals.push(Value::Text(ai.name.as_slice()));
        vals.push(Value::Integer(ai.counter));
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        btree::btree_insert(pager, ai.seq_root, rowid, rec.as_slice()).await
    }

    /// Produce every row an INSERT will insert, as raw value
    /// rows parallel to the plan's `targets` (v0.8). VALUES rows
    /// were folded at plan time, except rows containing
    /// subqueries, which evaluate here against the empty row;
    /// an INSERT ... SELECT source executes its subquery plan
    /// against the empty outer row.
    async fn insert_source_rows(&mut self) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        #[cfg(feature = "v08")]
        {
            let select = match &self.plan {
                Plan::Insert { select, .. } => select.clone(),
                _ => unreachable!(),
            };
            if let Some(plan) = select {
                let empty = RowData::empty();
                return self.exec_subplan(&plan, &empty).await;
            }
            let rows_r = match &self.plan {
                Plan::Insert { rows_r, .. } => rows_r.clone(),
                _ => unreachable!(),
            };
            if let Some(rows_r) = rows_r {
                let empty = RowData::empty();
                let mut out = AVec::new();
                for rr in rows_r.iter() {
                    let mut rv = AVec::new();
                    for e in rr.iter() {
                        rv.push(self.eval_row_sub(e, &empty).await?);
                    }
                    out.push(rv);
                }
                return Ok(out);
            }
        }
        match &self.plan {
            Plan::Insert { rows_v, .. } => Ok(rows_v.clone()),
            _ => unreachable!(),
        }
    }

    async fn exec_insert_row(&mut self, values: AVec<OwnedValue>) -> Result<(), S::Error> {
        let Plan::Insert {
            rowid_alias,
            cols,
            targets,
            ..
        } = &self.plan
        else {
            unreachable!()
        };
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        let ncols = cols.len();
        // This row's defaults: a connection-state DEFAULT is
        // evaluated now, for this row (v0.16 review fix, `v16`).
        #[cfg(feature = "v16")]
        let row_defaults: AVec<OwnedValue> = {
            let mut out = AVec::new();
            for c in cols.iter() {
                out.push(match &c.per_row {
                    Some((r, crow)) => crate::rules::eval_prepared_default::<S::Error>(
                        r,
                        crow,
                        Some(&self.conn.env),
                    )?,
                    None => c.default.clone(),
                });
            }
            out
        };
        let mut row: AVec<OwnedValue> = AVec::new();
        // An omitted column's default is a stored value like any
        // other: the column's affinity applies (v0.10; the pin
        // stores DEFAULT '7' in an INT column as the integer 7).
        #[cfg(feature = "v16")]
        for (c, d) in cols.iter().zip(row_defaults.iter()) {
            row.push(apply_affinity(d.clone(), c.affinity));
        }
        #[cfg(not(feature = "v16"))]
        for c in cols.iter() {
            row.push(apply_affinity(c.default.clone(), c.affinity));
        }
        let mut explicit_rowid: Option<i64> = None;
        for (target, v) in targets.iter().zip(values.iter()) {
            let v = v.clone();
            let i = match target {
                Target::Rowid => None,
                Target::Col(i) if Some(*i) == *rowid_alias => None,
                Target::Col(i) => Some(*i),
            };
            match i {
                // The rowid (or its alias, whose record field
                // stays NULL): the pin's MustBeInt — NUMERIC
                // affinity, then a lossless integer (v0.16: `'8'`,
                // `9.0` and `' 10 '` are rowids 8, 9, 10; they
                // used to be a datatype mismatch). NULL assigns.
                None => match apply_affinity(v, crate::record::Affinity::Integer) {
                    OwnedValue::Integer(x) => explicit_rowid = Some(x),
                    OwnedValue::Null => {}
                    _ => {
                        return Err(Error::Schema {
                            kind: SchemaKind::DatatypeMismatch,
                        });
                    }
                },
                Some(i) => row.as_mut_slice()[i] = apply_affinity(v, cols.as_slice()[i].affinity),
            }
        }
        // BEFORE INSERT triggers see the row with its generated
        // columns, and rowid -1 while it is still to be chosen; the
        // rowid is chosen (and the constraints checked) after them.
        #[cfg(feature = "triggers")]
        if self.fires(
            crate::sql::ast::TrigTime::Before,
            crate::sql::ast::TrigEvent::Insert,
        ) {
            let mut nrow = row.clone();
            let nrowid = explicit_rowid.unwrap_or(-1);
            if let Some(g) = self.plan_rules().and_then(|r| r.gcols) {
                g.fill::<S::Error>(nrow.as_mut_slice(), nrowid, false)?;
            }
            if !self
                .fire(
                    crate::sql::ast::TrigTime::Before,
                    crate::sql::ast::TrigEvent::Insert,
                    None,
                    None,
                    Some((nrow.as_slice(), nrowid)),
                )
                .await?
            {
                return Ok(());
            }
        }
        let Plan::Insert {
            root,
            rowid_alias,
            cols,
            ..
        } = &self.plan
        else {
            unreachable!()
        };
        let rowid = {
            let pager = &mut self.conn.pager;
            match explicit_rowid {
                Some(r) => r,
                #[cfg(feature = "v16")]
                None if self.autoinc.is_some() => {
                    // AUTOINCREMENT: one past both the largest rowid
                    // and the largest ever handed out; nothing past
                    // i64::MAX ("database or disk is full").
                    let max = btree::seek_max_rowid(pager, *root).await?;
                    let counter = self.autoinc.as_ref().map_or(0, |a| a.counter);
                    let full = Error::Schema {
                        kind: SchemaKind::DatabaseFull,
                    };
                    let next = match max {
                        Some(m) => m.checked_add(1).ok_or(full)?,
                        None => 1,
                    };
                    next.max(counter.checked_add(1).ok_or(Error::Schema {
                        kind: SchemaKind::DatabaseFull,
                    })?)
                }
                #[cfg(feature = "v16")]
                None => btree::new_rowid(pager, *root).await?,
                #[cfg(not(feature = "v16"))]
                None => match btree::seek_max_rowid(pager, *root).await? {
                    Some(m) => m.checked_add(1).ok_or(Error::Schema {
                        kind: SchemaKind::DatatypeMismatch,
                    })?,
                    None => 1,
                },
            }
        };
        #[cfg(feature = "v16")]
        if let Some(ai) = self.autoinc.as_mut() {
            ai.counter = ai.counter.max(rowid);
        }
        // v0.10 NOT NULL: the proposed row is resolved under
        // the statement's conflict action before any uniqueness
        // check (the pin's order) — and, for an upsert, before
        // arbitration: a violating proposal under OR IGNORE is
        // skipped wholesale (its DO UPDATE never runs), under OR
        // REPLACE it is substituted and then arbitrated. v0.16:
        // after the rowid is known (generated columns and CHECK
        // constraints read it), and with the table's rules —
        // generated columns, STRICT types, CHECK — in the pin's
        // order (see `rules`).
        {
            let action = match &self.plan {
                Plan::Insert { conflict, .. } => *conflict,
                _ => unreachable!(),
            };
            let mut nn_flags = AVec::new();
            let mut nn_defaults = AVec::new();
            let mut nn_affs = AVec::new();
            #[cfg(feature = "v16")]
            for (c, d) in cols.iter().zip(row_defaults.iter()) {
                nn_flags.push(c.not_null);
                nn_defaults.push(d.clone());
                nn_affs.push(c.affinity);
            }
            #[cfg(not(feature = "v16"))]
            for c in cols.iter() {
                nn_flags.push(c.not_null);
                nn_defaults.push(c.default.clone());
                nn_affs.push(c.affinity);
            }
            #[cfg(feature = "v04")]
            let proceed = match &self.plan {
                Plan::Insert { rules: Some(r), .. } => r.resolve::<S::Error>(
                    row.as_mut_slice(),
                    rowid,
                    nn_flags.as_slice(),
                    nn_defaults.as_slice(),
                    nn_affs.as_slice(),
                    *rowid_alias,
                    action,
                    None,
                    Some(&self.conn.env),
                )?,
                _ => resolve_not_null::<S::Error>(
                    row.as_mut_slice(),
                    nn_flags.as_slice(),
                    nn_defaults.as_slice(),
                    nn_affs.as_slice(),
                    *rowid_alias,
                    action,
                )?,
            };
            #[cfg(not(feature = "v04"))]
            let proceed = resolve_not_null::<S::Error>(
                row.as_mut_slice(),
                nn_flags.as_slice(),
                nn_defaults.as_slice(),
                nn_affs.as_slice(),
                *rowid_alias,
                action,
            )?;
            if !proceed {
                return Ok(()); // OR IGNORE: skip this row.
            }
        }
        // v0.7: a non-default conflict clause or an upsert clause
        // routes the row through the conflict-aware path, which
        // finds the conflicting rows first and then replaces,
        // skips, or updates instead of failing.
        #[cfg(feature = "v07")]
        {
            let routed = match &self.plan {
                Plan::Insert {
                    conflict, upsert, ..
                } => !conflict.aborts() || upsert.is_some(),
                _ => unreachable!(),
            };
            if routed {
                return self.exec_insert_row_v07(row, rowid).await;
            }
        }
        #[cfg(feature = "v04")]
        let virt: Option<AVec<bool>> = match &self.plan {
            Plan::Insert { rules: Some(r), .. } => r.virt().map(|m| {
                let mut v = AVec::new();
                v.extend_from_slice(m);
                v
            }),
            _ => None,
        };
        #[cfg(feature = "foreign_keys")]
        let fk_affs = ins_affs(cols.as_slice());
        let pager = &mut self.conn.pager;
        let mut vals: AVec<Value<'_>> = AVec::new();
        for (i, v) in row.iter().enumerate() {
            // A VIRTUAL generated column is not stored (v0.16).
            #[cfg(feature = "v04")]
            if virt
                .as_ref()
                .is_some_and(|m| m.as_slice().get(i).copied().unwrap_or(false))
            {
                continue;
            }
            if Some(i) == *rowid_alias {
                vals.push(Value::Null);
            } else {
                vals.push(v.as_value());
            }
        }
        let _ = ncols;
        // v0.3 index work, part 1: UNIQUE pre-checks against the
        // row's keys (a key containing NULL never conflicts).
        #[cfg(feature = "indexes")]
        let mut pending_keys: AVec<(u32, AVec<crate::record::KeyOrder>, AVec<OwnedValue>)> =
            AVec::new();
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            let mut key = AVec::new();
            for c in ix.cols.iter() {
                key.push(match c.col {
                    None => OwnedValue::Integer(rowid),
                    Some(i) => row.as_slice()[i].clone(),
                });
            }
            if ix.unique
                && key_has_no_null(key.as_slice())
                && btree::index_first_with_prefix(
                    pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                )
                .await?
                .is_some()
            {
                return Err(Error::Schema {
                    kind: SchemaKind::UniqueViolation,
                });
            }
            pending_keys.push((ix.root, ix.desc.clone(), key));
        }
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        // v0.16 (`foreign_keys`): the row's keys, after its other
        // constraints and before it is stored (the pin's order).
        #[cfg(feature = "foreign_keys")]
        let (pager, root) = {
            let root = *root;
            drop(vals);
            if self.fk_active() {
                // The pin finds a rowid conflict among the row's
                // constraints, before its keys: a single-row INSERT
                // halts on a violated key at once, so the conflict
                // must win (v0.16 review fix — OR ROLLBACK then ends
                // the transaction).
                if btree::seek_rowid(&mut self.conn.pager, root, rowid)
                    .await?
                    .is_some()
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::DuplicateRowid,
                    });
                }
                let lrow = self.fk_row(row.as_slice(), rowid, fk_affs.as_slice());
                self.fk_new_row(lrow.as_slice(), rowid, None, None).await?;
            }
            (&mut self.conn.pager, root)
        };
        #[cfg(feature = "foreign_keys")]
        btree::btree_insert(pager, root, rowid, rec.as_slice()).await?;
        #[cfg(not(feature = "foreign_keys"))]
        btree::btree_insert(pager, *root, rowid, rec.as_slice()).await?;
        // v0.3 index work, part 2: maintain every index.
        #[cfg(feature = "indexes")]
        for (ix_root, desc, key) in pending_keys.iter() {
            let payload = encode_index_payload(key.as_slice(), rowid);
            btree::index_insert(pager, *ix_root, payload.as_slice(), desc.as_slice()).await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.note_insert(rowid);
        #[cfg(feature = "v07")]
        self.push_insert_returning(row.as_slice(), rowid).await?;
        #[cfg(feature = "triggers")]
        self.after_insert(row.as_slice(), rowid).await?;
        Ok(())
    }

    /// AFTER INSERT triggers of one inserted row (v0.16,
    /// `triggers`): they see the row as stored, generated columns
    /// computed.
    #[cfg(feature = "triggers")]
    async fn after_insert(&mut self, row: &[OwnedValue], rowid: i64) -> Result<(), S::Error> {
        use crate::sql::ast::{TrigEvent, TrigTime};
        if !self.fires(TrigTime::After, TrigEvent::Insert) {
            return Ok(());
        }
        let mut nrow = AVec::new();
        nrow.extend_from_slice(row);
        if let Some(g) = self.plan_rules().and_then(|r| r.gcols) {
            g.fill::<S::Error>(nrow.as_mut_slice(), rowid, true)?;
        }
        self.fire(
            TrigTime::After,
            TrigEvent::Insert,
            None,
            None,
            Some((nrow.as_slice(), rowid)),
        )
        .await?;
        Ok(())
    }

    /// Buffer one inserted (or upsert-updated) row's RETURNING
    /// values (v0.7): the expressions read the row as stored,
    /// through the same row shape a scan would produce.
    #[cfg(feature = "v07")]
    async fn push_insert_returning(
        &mut self,
        rec: &[OwnedValue],
        rowid: i64,
    ) -> Result<(), S::Error> {
        let (returning, rowid_alias, affs) = match &self.plan {
            Plan::Insert {
                returning,
                rowid_alias,
                cols,
                ..
            } => {
                let mut affs = AVec::new();
                for c in cols.iter() {
                    affs.push(c.affinity);
                }
                (returning.clone(), *rowid_alias, affs)
            }
            _ => unreachable!(),
        };
        if returning.is_empty() {
            return Ok(());
        }
        let rd = row_data_for(rowid_alias, affs.as_slice(), rec, rowid);
        let vals = self.eval_returning(returning.as_slice(), &rd).await?;
        self.ret_rows.push(vals);
        self.ret_rowids.push(Some(rowid));
        Ok(())
    }

    /// Insert one fully-built row with no conflict handling
    /// (v0.7): record encode, B-tree insert, index maintenance.
    /// Callers have already resolved every conflict.
    #[cfg(feature = "v07")]
    async fn insert_row_raw(&mut self, row: &[OwnedValue], rowid: i64) -> Result<(), S::Error> {
        let (root, rowid_alias) = match &self.plan {
            Plan::Insert {
                root, rowid_alias, ..
            } => (*root, *rowid_alias),
            _ => unreachable!(),
        };
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        let rules = self.plan_rules();
        let rec = crate::rules::encode_rec(row, rowid_alias, rules.as_ref().and_then(|r| r.virt()));
        btree::btree_insert(&mut self.conn.pager, root, rowid, rec.as_slice()).await?;
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            let mut key = AVec::new();
            for c in ix.cols.iter() {
                key.push(match c.col {
                    None => OwnedValue::Integer(rowid),
                    Some(i) => row[i].clone(),
                });
            }
            let payload = encode_index_payload(key.as_slice(), rowid);
            btree::index_insert(
                &mut self.conn.pager,
                ix.root,
                payload.as_slice(),
                ix.desc.as_slice(),
            )
            .await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.note_insert(rowid);
        Ok(())
    }

    /// Rewrite one stored row in place: old index entries out,
    /// old row out, new row in, new index entries in — the apply
    /// tail shared by UPDATE (v0.5) and upsert DO UPDATE (v0.7).
    #[cfg(feature = "v05")]
    async fn rewrite_stored_row(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        old_rec: &[OwnedValue],
        old_rowid: i64,
        new_rec: &[OwnedValue],
        new_rowid: i64,
    ) -> Result<(), S::Error> {
        #[cfg(not(feature = "indexes"))]
        let _ = old_rec;
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } => indexes.clone(),
            Plan::Update { indexes, .. } => indexes.clone(),
            Plan::Delete { indexes, .. } => indexes.clone(),
            _ => AVec::new(),
        };
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            let old_key = key_from_rec(ix.cols.as_slice(), rowid_alias, old_rec, old_rowid);
            let payload = encode_index_payload(old_key.as_slice(), old_rowid);
            btree::index_delete(
                &mut self.conn.pager,
                ix.root,
                payload.as_slice(),
                ix.desc.as_slice(),
            )
            .await?;
        }
        btree::btree_delete(&mut self.conn.pager, root, old_rowid).await?;
        let rules = self.plan_rules();
        let rec_bytes =
            crate::rules::encode_rec(new_rec, rowid_alias, rules.as_ref().and_then(|r| r.virt()));
        btree::btree_insert(&mut self.conn.pager, root, new_rowid, rec_bytes.as_slice()).await?;
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            let key = key_from_rec(ix.cols.as_slice(), rowid_alias, new_rec, new_rowid);
            let payload = encode_index_payload(key.as_slice(), new_rowid);
            btree::index_insert(
                &mut self.conn.pager,
                ix.root,
                payload.as_slice(),
                ix.desc.as_slice(),
            )
            .await?;
        }
        Ok(())
    }

    /// One INSERT row under a non-default conflict clause or an
    /// upsert clause (v0.7). `row` is the proposed record
    /// (defaults applied, column affinities applied, the alias
    /// slot holding its default) and `rowid` its proposed rowid.
    #[cfg(feature = "v07")]
    async fn exec_insert_row_v07(
        &mut self,
        row: AVec<OwnedValue>,
        rowid: i64,
    ) -> Result<(), S::Error> {
        let (root, rowid_alias, defaults, affs, not_nulls, conflict_action, upsert) =
            match &self.plan {
                Plan::Insert {
                    root,
                    rowid_alias,
                    cols,
                    conflict,
                    upsert,
                    ..
                } => {
                    let mut defaults = AVec::new();
                    let mut affs = AVec::new();
                    let mut not_nulls = AVec::new();
                    for c in cols.iter() {
                        defaults.push(c.default.clone());
                        affs.push(c.affinity);
                        not_nulls.push(c.not_null);
                    }
                    (
                        *root,
                        *rowid_alias,
                        defaults,
                        affs,
                        not_nulls,
                        *conflict,
                        upsert.clone(),
                    )
                }
                _ => unreachable!(),
            };
        let ncols = defaults.len();
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        // Find every conflict: (conflicting rowid, conflict is on
        // the rowid itself, the index's canonical key-target set
        // when the conflict is index-borne).
        let mut conflicts: AVec<(i64, bool, Option<KeySet>)> = AVec::new();
        if btree::seek_rowid(&mut self.conn.pager, root, rowid)
            .await?
            .is_some()
        {
            conflicts.push((rowid, true, None));
        }
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            if !ix.unique {
                continue;
            }
            let mut key = AVec::new();
            for c in ix.cols.iter() {
                key.push(match c.col {
                    None => OwnedValue::Integer(rowid),
                    Some(i) => row.as_slice()[i].clone(),
                });
            }
            if key_has_no_null(key.as_slice())
                && let Some(other) = btree::index_first_with_prefix(
                    &mut self.conn.pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                )
                .await?
            {
                let mut keyset = AVec::new();
                for c in ix.cols.iter() {
                    keyset.push((
                        match c.col {
                            None => Target::Rowid,
                            Some(i) if Some(i) == rowid_alias => Target::Rowid,
                            Some(i) => Target::Col(i),
                        },
                        c.coll,
                    ));
                }
                conflicts.push((other, false, Some(keyset)));
            }
        }
        if conflicts.is_empty() {
            #[cfg(feature = "foreign_keys")]
            if self.fk_active() {
                let lrow = self.fk_row(row.as_slice(), rowid, affs.as_slice());
                self.fk_new_row(lrow.as_slice(), rowid, None, None).await?;
            }
            self.insert_row_raw(row.as_slice(), rowid).await?;
            self.push_insert_returning(row.as_slice(), rowid).await?;
            #[cfg(feature = "triggers")]
            self.after_insert(row.as_slice(), rowid).await?;
            return Ok(());
        }
        // Upsert arbitration: the first conflict the clause
        // arbitrates, if any.
        if let Some(up) = &upsert {
            let arb = conflicts
                .iter()
                .position(|c| upsert_arbitrates(up, c.1, &c.2));
            if let Some(pos) = arb {
                let crowid = conflicts.as_slice()[pos].0;
                if !up.do_update {
                    return Ok(()); // DO NOTHING: skip the row.
                }
                let r = self
                    .exec_upsert_update(
                        up,
                        row.as_slice(),
                        rowid,
                        crowid,
                        root,
                        rowid_alias,
                        defaults.as_slice(),
                        affs.as_slice(),
                        not_nulls.as_slice(),
                    )
                    .await;
                #[cfg(feature = "v16")]
                if r.is_err() {
                    self.upsert_failed = true;
                }
                return r;
            }
            // A conflict the clause does not arbitrate falls back
            // to the statement's own conflict action (Abort by
            // default — the oracle's behaviour, probed in v0.7).
        }
        match conflict_action {
            ConflictAction::Ignore => Ok(()),
            ConflictAction::Replace => {
                let mut victims: AVec<i64> = AVec::new();
                for c in conflicts.iter() {
                    if !victims.iter().any(|v| *v == c.0) {
                        victims.push(c.0);
                    }
                }
                #[cfg(feature = "triggers")]
                let mut triggered = false;
                for v in victims.iter() {
                    #[cfg(feature = "triggers")]
                    {
                        let kind = if conflicts.iter().any(|c| c.0 == *v && c.1) {
                            SchemaKind::DuplicateRowid
                        } else {
                            SchemaKind::UniqueViolation
                        };
                        if self.replace_victim(*v, kind).await? {
                            triggered = true;
                            continue;
                        }
                    }
                    self.delete_row_completely(root, rowid_alias, ncols, defaults.as_slice(), *v)
                        .await?;
                }
                // DELETE triggers may have written the table: the pin
                // checks the row again, under ABORT.
                #[cfg(feature = "triggers")]
                if triggered
                    && let Some(kind) = self
                        .insert_conflict(root, row.as_slice(), rowid, None)
                        .await?
                {
                    return Err(Error::Schema { kind });
                }
                #[cfg(feature = "foreign_keys")]
                if self.fk_active() {
                    let lrow = self.fk_row(row.as_slice(), rowid, affs.as_slice());
                    self.fk_new_row(lrow.as_slice(), rowid, None, None).await?;
                }
                self.insert_row_raw(row.as_slice(), rowid).await?;
                self.push_insert_returning(row.as_slice(), rowid).await?;
                #[cfg(feature = "triggers")]
                self.after_insert(row.as_slice(), rowid).await?;
                Ok(())
            }
            // ABORT, FAIL, ROLLBACK: the statement fails.
            _ => Err(Error::Schema {
                kind: if conflicts.as_slice()[0].1 {
                    SchemaKind::DuplicateRowid
                } else {
                    SchemaKind::UniqueViolation
                },
            }),
        }
    }

    /// Whether inserting `row` at `rowid` would conflict (v0.16,
    /// `triggers`): the kind of the first conflict (the rowid, then
    /// each UNIQUE index in order).
    /// `own`: the rowid the row is stored at now (an UPDATE's), which
    /// is no conflict.
    #[cfg(feature = "triggers")]
    async fn insert_conflict(
        &mut self,
        root: u32,
        row: &[OwnedValue],
        rowid: i64,
        own: Option<i64>,
    ) -> Result<Option<SchemaKind>, S::Error> {
        if own != Some(rowid)
            && btree::seek_rowid(&mut self.conn.pager, root, rowid)
                .await?
                .is_some()
        {
            return Ok(Some(SchemaKind::DuplicateRowid));
        }
        #[cfg(not(feature = "indexes"))]
        let _ = row;
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } | Plan::Update { indexes, .. } => indexes.clone(),
            _ => AVec::new(),
        };
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            if !ix.unique {
                continue;
            }
            let mut key = AVec::new();
            for c in ix.cols.iter() {
                key.push(match c.col {
                    None => OwnedValue::Integer(rowid),
                    Some(i) => row.get(i).cloned().unwrap_or(OwnedValue::Null),
                });
            }
            if key_has_no_null(key.as_slice())
                && let Some(other) = btree::index_first_with_prefix(
                    &mut self.conn.pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                )
                .await?
                && own != Some(other)
            {
                return Ok(Some(SchemaKind::UniqueViolation));
            }
        }
        Ok(None)
    }

    /// The DO UPDATE half of an upsert (v0.7): transform the
    /// insert into an update of the conflicting row `crowid`.
    /// SET expressions evaluate against (original ++ excluded);
    /// a false WHERE skips the row entirely.
    #[cfg(feature = "v07")]
    #[allow(clippy::too_many_arguments)]
    async fn exec_upsert_update(
        &mut self,
        up: &UpsertPlan,
        row: &[OwnedValue],
        rowid: i64,
        crowid: i64,
        root: u32,
        rowid_alias: Option<usize>,
        defaults: &[OwnedValue],
        affs: &[crate::record::Affinity],
        not_nulls: &[bool],
    ) -> Result<(), S::Error> {
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Insert { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        // Load the conflicting row in record form.
        let Some(payload) = btree::seek_rowid(&mut self.conn.pager, root, crowid).await? else {
            // The conflicting row vanished mid-statement (it was
            // an earlier victim of this same statement's REPLACE
            // handling); nothing to update.
            return Ok(());
        };
        let rules = self.plan_rules();
        let gcols = rules.as_ref().and_then(|r| r.gcols.as_ref());
        let mut orec = crate::rules::decode_rec::<S::Error>(
            payload.as_slice(),
            defaults.len(),
            defaults,
            gcols.map(|g| g.virt.as_slice()),
        )?;
        if let Some(g) = gcols {
            g.fill::<S::Error>(orec.as_mut_slice(), crowid, true)?;
        }
        // The row as stored when it is rewritten (read again after
        // BEFORE UPDATE triggers); `orec` stays the OLD row.
        #[allow(unused_mut)]
        let mut stored = orec.clone();
        // Combined evaluation row: original ++ excluded.
        let orig_rd = row_data_for(rowid_alias, affs, orec.as_slice(), crowid);
        let prop_rd = row_data_for(rowid_alias, affs, row, rowid);
        let mut combined = orig_rd.clone();
        for v in prop_rd.vals.iter() {
            combined.vals.push(v.clone());
        }
        for r in prop_rd.rowids.iter() {
            combined.rowids.push(*r);
        }
        if let Some(f) = &up.filter
            && crate::query::truthy(&self.eval_row_sub(f, &combined).await?) != Some(true)
        {
            return Ok(()); // WHERE false: neither insert nor update.
        }
        let mut evaluated: AVec<(Target, OwnedValue)> = AVec::new();
        for (target, e) in up.sets.iter() {
            let v = self.eval_row_sub(e, &combined).await?;
            evaluated.push((*target, v));
        }
        let mut new_rec: AVec<OwnedValue> = orec.clone();
        let mut new_rowid = crowid;
        for (target, v) in evaluated.iter() {
            match target {
                Target::Col(i) if Some(*i) != rowid_alias => {
                    new_rec.as_mut_slice()[*i] = apply_affinity(v.clone(), affs[*i]);
                }
                _ => new_rowid = update_rowid::<S::Error>(v)?,
            }
        }
        // v0.16 (`triggers`): DO UPDATE fires the UPDATE triggers.
        #[cfg(feature = "triggers")]
        let (up_assigned, up_rowid_set) =
            assigned_columns(up.sets.as_slice(), defaults.len(), rowid_alias);
        #[cfg(feature = "triggers")]
        if self.fires(
            crate::sql::ast::TrigTime::Before,
            crate::sql::ast::TrigEvent::Update,
        ) {
            let mut nrec = new_rec.clone();
            if let Some(g) = gcols {
                for (i, v) in nrec.as_mut_slice().iter_mut().enumerate() {
                    if g.is_gen(i) {
                        *v = OwnedValue::Null;
                    }
                }
            }
            if !self
                .fire_as(
                    crate::sql::ast::TrigTime::Before,
                    crate::sql::ast::TrigEvent::Update,
                    Some((up_assigned.as_slice(), up_rowid_set)),
                    Some((orec.as_slice(), crowid)),
                    Some((nrec.as_slice(), new_rowid)),
                    Some(ConflictAction::Abort),
                )
                .await?
            {
                return Ok(());
            }
            // Then the row is read again, as an UPDATE's (v0.16 review
            // fix: the upsert used the row from before its triggers):
            // gone — deleted by a trigger, or by a foreign-key cascade
            // one ran — it is skipped; the columns DO UPDATE does not
            // assign take their current values.
            let Some(now) = self
                .reload_write_row(root, defaults.len(), defaults, crowid)
                .await?
            else {
                return Ok(());
            };
            for (i, v) in new_rec.as_mut_slice().iter_mut().enumerate() {
                if !up_assigned.as_slice().get(i).copied().unwrap_or(false) {
                    *v = now
                        .rec
                        .as_slice()
                        .get(i)
                        .cloned()
                        .unwrap_or(OwnedValue::Null);
                }
            }
            stored = now.rec;
        }
        // v0.10 NOT NULL: the DO UPDATE result row is checked
        // with ABORT semantics unconditionally (probed on the
        // pin: no default substitution and no skip, whatever
        // the statement's own conflict action — even OR
        // REPLACE), and before its uniqueness checks, matching
        // the pin's error precedence. A WHERE that skipped the
        // update returned above, before any assignment ran.
        // v0.16: with the table's rules (generated columns
        // recomputed, STRICT, CHECK) — also ABORT unconditionally
        // (probed: a failing CHECK errors even under OR IGNORE).
        // v0.16: like an UPDATE, only the assigned columns are
        // checked for NOT NULL / CHECK (see `TableRules::resolve`).
        let (assigned, rowid_set) =
            assigned_columns(up.sets.as_slice(), defaults.len(), rowid_alias);
        match &rules {
            Some(r) => {
                r.resolve::<S::Error>(
                    new_rec.as_mut_slice(),
                    new_rowid,
                    not_nulls,
                    defaults,
                    affs,
                    rowid_alias,
                    ConflictAction::Abort,
                    Some((assigned.as_slice(), rowid_set)),
                    Some(&self.conn.env),
                )?;
            }
            None => {
                let mut nn = AVec::new();
                for (i, f) in not_nulls.iter().enumerate() {
                    nn.push(*f && assigned.as_slice().get(i).copied().unwrap_or(false));
                }
                resolve_not_null::<S::Error>(
                    new_rec.as_mut_slice(),
                    nn.as_slice(),
                    defaults,
                    affs,
                    rowid_alias,
                    ConflictAction::Abort,
                )?;
            }
        }
        // The update itself must not collide with a third row.
        if new_rowid != crowid
            && btree::seek_rowid(&mut self.conn.pager, root, new_rowid)
                .await?
                .is_some()
        {
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateRowid,
            });
        }
        #[cfg(feature = "indexes")]
        for ix in indexes.iter() {
            if !ix.unique {
                continue;
            }
            let key = key_from_rec(
                ix.cols.as_slice(),
                rowid_alias,
                new_rec.as_slice(),
                new_rowid,
            );
            if key_has_no_null(key.as_slice())
                && let Some(other) = btree::index_first_with_prefix(
                    &mut self.conn.pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                )
                .await?
                && other != crowid
            {
                return Err(Error::Schema {
                    kind: SchemaKind::UniqueViolation,
                });
            }
        }
        // v0.16 (`foreign_keys`): the update's keys, as an UPDATE's.
        #[cfg(feature = "foreign_keys")]
        let fk_rows = if self.fk_active() {
            let lold = self.fk_row(orec.as_slice(), crowid, affs);
            let lnew = self.fk_row(new_rec.as_slice(), new_rowid, affs);
            let ch = Some((assigned.as_slice(), rowid_set));
            self.fk_old_row(lold.as_slice(), crowid, ch).await?;
            self.fk_new_row(lnew.as_slice(), new_rowid, Some(crowid), ch)
                .await?;
            Some((lold, lnew))
        } else {
            None
        };
        self.rewrite_stored_row(
            root,
            rowid_alias,
            stored.as_slice(),
            crowid,
            new_rec.as_slice(),
            new_rowid,
        )
        .await?;
        #[cfg(feature = "foreign_keys")]
        if let Some((lold, lnew)) = &fk_rows {
            self.fk_actions(
                lold.as_slice(),
                Some(lnew.as_slice()),
                Some((assigned.as_slice(), rowid_set)),
            )
            .await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.note_change();
        self.push_insert_returning(new_rec.as_slice(), new_rowid)
            .await?;
        #[cfg(feature = "triggers")]
        if self.fires(
            crate::sql::ast::TrigTime::After,
            crate::sql::ast::TrigEvent::Update,
        ) {
            self.fire_as(
                crate::sql::ast::TrigTime::After,
                crate::sql::ast::TrigEvent::Update,
                Some((up_assigned.as_slice(), up_rowid_set)),
                Some((orec.as_slice(), crowid)),
                Some((new_rec.as_slice(), new_rowid)),
                Some(ConflictAction::Abort),
            )
            .await?;
        }
        Ok(())
    }

    // ---------------- v0.5 write-side execution ----------------

    #[cfg(feature = "v05")]
    async fn step_update(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        #[cfg(feature = "v07")]
        if self.has_returning() {
            if !self.ret_loaded {
                match self.exec_update().await {
                    Ok(()) => {
                        self.ret_loaded = true;
                        self.ret_pos = 0;
                    }
                    Err(e) => return Err(self.write_failed(e).await),
                }
            }
            return self.serve_returning_fk().await;
        }
        let r = self.exec_update().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            // Same abort rule as INSERT (see `write_failed`).
            Err(e) => Err(self.write_failed(e).await),
        }
    }

    #[cfg(feature = "v05")]
    async fn step_delete(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        #[cfg(feature = "v07")]
        if self.has_returning() {
            if !self.ret_loaded {
                match self.exec_delete().await {
                    Ok(()) => {
                        self.ret_loaded = true;
                        self.ret_pos = 0;
                    }
                    #[allow(unused_mut)]
                    Err(mut e) => {
                        #[cfg(feature = "triggers")]
                        if self.trigger_failed(&mut e).await {
                            return Err(e);
                        }
                        #[cfg(feature = "v16")]
                        self.conn.env.finish(false);
                        abort_write!(self, &e);
                        return Err(e);
                    }
                }
            }
            return self.serve_returning_fk().await;
        }
        let r = self.exec_delete().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            #[allow(unused_mut)]
            Err(mut e) => {
                #[cfg(feature = "triggers")]
                if self.trigger_failed(&mut e).await {
                    return Err(e);
                }
                #[cfg(feature = "v16")]
                self.conn.env.finish(false);
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "v05")]
    async fn step_drop_table(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_drop_table().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                #[cfg(feature = "foreign_keys")]
                if core::mem::take(&mut self.conn.fk.drop_counted) {
                    self.conn.env.finish(false);
                }
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    /// Materialise the rows an UPDATE/DELETE touches (v0.5):
    /// full scan, decode each record (short records materialise
    /// defaults), keep the rows the filter accepts. Collecting
    /// before mutating is what keeps the statement's row set
    /// stable while it rewrites the B-tree underneath itself —
    /// and what makes statement atomicity a rollback away. On a
    /// failure, the rows the scan passed before it are in `out`
    /// (v0.16 review fix, `triggers`: a trigger program's one-pass
    /// write keeps them — `one_pass`).
    #[cfg(feature = "v05")]
    #[allow(clippy::too_many_arguments)]
    async fn scan_write_rows_into(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: &[OwnedValue],
        affs: &[crate::record::Affinity],
        filter: &Option<RExpr>,
        out: &mut AVec<WriteRow>,
    ) -> Result<(), S::Error> {
        let mut cursor = ScanCursor::new(root);
        let mut buf = AVec::new();
        // v0.16: VIRTUAL generated columns are computed per row.
        let rules = self.plan_rules();
        let gcols = rules.as_ref().and_then(|r| r.gcols.as_ref());
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let mut rec = crate::rules::decode_rec::<S::Error>(
                buf.as_slice(),
                ncols,
                defaults,
                gcols.map(|g| g.virt.as_slice()),
            )?;
            if let Some(g) = gcols {
                g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
            }
            if let Some(f) = filter {
                let rd = row_data_for(rowid_alias, affs, rec.as_slice(), rowid);
                #[cfg(feature = "v06")]
                let keep = crate::query::truthy(&self.eval_row_sub(f, &rd).await?) == Some(true);
                #[cfg(not(feature = "v06"))]
                let keep = crate::query::truthy(&crate::query::eval::<S::Error>(
                    f,
                    &Ctx::of(&rd).with(&self.conn.env),
                )?) == Some(true);
                if !keep {
                    continue;
                }
            }
            out.push(WriteRow { rowid, rec });
        }
        Ok(())
    }

    /// Re-read one row of an UPDATE's target by rowid (v0.16),
    /// decoded as `scan_write_rows_into` decodes it; `None` when the
    /// row is gone (an OR REPLACE earlier in the statement
    /// deleted it).
    #[cfg(feature = "v05")]
    async fn reload_write_row(
        &mut self,
        root: u32,
        ncols: usize,
        defaults: &[OwnedValue],
        rowid: i64,
    ) -> Result<Option<WriteRow>, S::Error> {
        let Some(payload) = btree::seek_rowid(&mut self.conn.pager, root, rowid).await? else {
            return Ok(None);
        };
        let rules = self.plan_rules();
        let gcols = rules.as_ref().and_then(|r| r.gcols.as_ref());
        let mut rec = crate::rules::decode_rec::<S::Error>(
            payload.as_slice(),
            ncols,
            defaults,
            gcols.map(|g| g.virt.as_slice()),
        )?;
        if let Some(g) = gcols {
            g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
        }
        Ok(Some(WriteRow { rowid, rec }))
    }

    #[cfg(feature = "v05")]
    async fn exec_update(&mut self) -> Result<(), S::Error> {
        #[cfg(feature = "v16")]
        self.conn.env.pending.set(0);
        #[cfg(feature = "triggers")]
        self.load_triggers()?;
        let (root, rowid_alias, ncols, defaults, affs, not_null, sets, filter, conflict_action) =
            match &self.plan {
                Plan::Update {
                    root,
                    rowid_alias,
                    ncols,
                    defaults,
                    affs,
                    not_null,
                    sets,
                    filter,
                    conflict,
                    ..
                } => (
                    *root,
                    *rowid_alias,
                    *ncols,
                    defaults.clone(),
                    affs.clone(),
                    not_null.clone(),
                    sets.clone(),
                    filter.clone(),
                    *conflict,
                ),
                _ => unreachable!(),
            };
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Update { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        // v0.7 FROM: materialise the FROM sources once; the
        // filter then applies per (target row, FROM combination)
        // pair instead of per target row, so the scan runs
        // unfiltered.
        #[cfg(feature = "v07")]
        let (from_sources, returning) = match &self.plan {
            Plan::Update {
                from_sources,
                returning,
                ..
            } => (from_sources.clone(), returning.clone()),
            _ => unreachable!(),
        };
        #[cfg(feature = "v07")]
        let from_rows = if from_sources.is_empty() {
            None
        } else {
            Some(
                self.materialise_sources_sub(from_sources.as_slice(), &RowData::empty(), None)
                    .await?,
            )
        };
        #[cfg(feature = "v07")]
        let scan_filter = if from_rows.is_some() { &None } else { &filter };
        #[cfg(not(feature = "v07"))]
        let scan_filter = &filter;
        let mut rows = AVec::new();
        let scanned = self
            .scan_write_rows_into(
                root,
                rowid_alias,
                ncols,
                defaults.as_slice(),
                affs.as_slice(),
                scan_filter,
                &mut rows,
            )
            .await;
        #[cfg(not(feature = "triggers"))]
        scanned?;
        let rules = self.plan_rules();
        // v0.16: the statement's change set — NOT NULL and CHECK
        // apply to assigned columns only, as on the pin.
        let (assigned, rowid_set) = assigned_columns(sets.as_slice(), ncols, rowid_alias);
        #[cfg(feature = "foreign_keys")]
        self.fk_load(fkey::Event::Update(assigned.as_slice(), rowid_set), true)?;
        // A trigger program's one-pass UPDATE keeps the rows its
        // WHERE passed before failing (`one_pass`).
        #[cfg(feature = "triggers")]
        let failed = match scanned {
            Ok(()) => None,
            Err(e)
                if from_rows.is_none()
                    && self.one_pass(
                        crate::sql::ast::TrigEvent::Update,
                        Some((assigned.as_slice(), rowid_set)),
                        filter.as_ref(),
                        conflict_action == ConflictAction::Replace,
                    ) =>
            {
                Some(e)
            }
            Err(e) => return Err(e),
        };
        // (The pin deletes REPLACE victims only where the update can
        // conflict: a new rowid, or a UNIQUE index it changes.)
        #[cfg(feature = "foreign_keys")]
        {
            let replace = conflict_action == ConflictAction::Replace
                && self.fk_update_replace(assigned.as_slice(), rowid_set);
            if replace {
                self.fk_load_victims()?;
            }
        }
        let mut nn_assigned = AVec::new();
        for (i, f) in not_null.iter().enumerate() {
            nn_assigned.push(*f && assigned.as_slice().get(i).copied().unwrap_or(false));
        }
        // Set once an OR REPLACE deletion or a rowid change has
        // rewritten rows other than the one being updated: from
        // then on each collected rowid is re-read before it is
        // updated. The pin collects rowids first and re-reads
        // each one, so a row an earlier REPLACE deleted is never
        // visited, and a rowid another row moved into is updated
        // from its current content (probed: UPDATE OR REPLACE t
        // SET id=id+1, v=v||'x' over ids 1,2 leaves 3|axx).
        let mut stale = false;
        // v0.16 (`triggers`): with triggers each row is read again
        // when its turn comes, as on the pin (a trigger may have
        // changed or deleted it).
        #[cfg(feature = "triggers")]
        let triggered = self.trig.is_some();
        #[cfg(not(feature = "triggers"))]
        let triggered = false;
        // v0.16 (`foreign_keys`): an action may change rows too.
        #[cfg(feature = "foreign_keys")]
        let triggered = triggered || self.fk_active();
        for snap_row in rows.iter() {
            let reloaded;
            let wrow = if stale || triggered {
                match self
                    .reload_write_row(root, ncols, defaults.as_slice(), snap_row.rowid)
                    .await?
                {
                    Some(r) => {
                        reloaded = r;
                        &reloaded
                    }
                    None => continue,
                }
            } else {
                snap_row
            };
            let base_rd = row_data_for(
                rowid_alias,
                affs.as_slice(),
                wrow.rec.as_slice(),
                wrow.rowid,
            );
            // The (target, FROM) combinations this row updates
            // through (v0.7): exactly one without FROM; with FROM,
            // one per joined combination the filter accepts.
            let mut combos: AVec<RowData> = AVec::new();
            #[cfg(feature = "v07")]
            if let Some(fr) = &from_rows {
                let mut joined: AVec<RowData> = AVec::new();
                self.join_sources(
                    from_sources.as_slice(),
                    None,
                    fr.as_slice(),
                    &base_rd,
                    &mut joined,
                )
                .await?;
                // v0.16: only the LAST matching combination
                // updates the row. The pin materialises the
                // join keyed by target rowid, so a later
                // combination overwrites an earlier one before
                // any is applied — probed: with SET v = s.x over
                // s = (5, 50, 6) and CHECK (v < 10), the row
                // becomes 6 and no CHECK fires; a NULL in an
                // earlier combination of a NOT NULL column
                // likewise never errors. (hipp used to apply
                // every combination in turn: the same final
                // value, but constraint checks on the earlier
                // ones.)
                let mut last: Option<RowData> = None;
                for c in joined.iter() {
                    let keep = match &filter {
                        None => true,
                        Some(f) => {
                            crate::query::truthy(&self.eval_row_sub(f, c).await?) == Some(true)
                        }
                    };
                    if keep {
                        last = Some(c.clone());
                    }
                }
                if let Some(c) = last {
                    combos.push(c);
                }
            } else {
                combos.push(base_rd);
            }
            #[cfg(not(feature = "v07"))]
            combos.push(base_rd);
            // The row as currently stored. `combos` holds at most
            // one combination (v0.16: the last matching one —
            // the v0.7 probes saw the last combination's value,
            // computed from the ORIGINAL v, remain), so the row
            // is rewritten at most once.
            let mut cur_rec: AVec<OwnedValue> = wrow.rec.clone();
            let mut cur_rowid = wrow.rowid;
            let mut applied = false;
            for rd in combos.iter() {
                // SET expressions evaluate against the ORIGINAL
                // row (oracle-verified: `SET b=1, b=b+100` adds
                // 100 to the old b); applying them in order makes
                // the last assignment to a column win.
                let mut evaluated: AVec<(Target, OwnedValue)> = AVec::new();
                for (target, e) in sets.iter() {
                    #[cfg(feature = "v06")]
                    let v = self.eval_row_sub(e, rd).await?;
                    #[cfg(not(feature = "v06"))]
                    let v = crate::query::eval::<S::Error>(e, &Ctx::of(rd).with(&self.conn.env))?;
                    evaluated.push((*target, v));
                }
                let mut new_rec: AVec<OwnedValue> = wrow.rec.clone();
                let mut new_rowid = wrow.rowid;
                for (target, v) in evaluated.iter() {
                    match target {
                        Target::Col(i) if Some(*i) != rowid_alias => {
                            new_rec.as_mut_slice()[*i] =
                                apply_affinity(v.clone(), affs.as_slice()[*i]);
                        }
                        // The rowid / its alias (whose record field
                        // stays NULL — the rowid carries it).
                        _ => new_rowid = update_rowid::<S::Error>(v)?,
                    }
                }
                // v0.16 (`triggers`): BEFORE UPDATE triggers (NEW's
                // generated columns computed as on the pin); then the row
                // is read again — gone, it is skipped; the columns the
                // statement does not assign take their current values.
                #[cfg(feature = "triggers")]
                if self.fires(
                    crate::sql::ast::TrigTime::Before,
                    crate::sql::ast::TrigEvent::Update,
                ) {
                    let mut nrec = new_rec.clone();
                    if let Some(g) = rules.as_ref().and_then(|r| r.gcols.as_ref()) {
                        // v0.16 review fix: the pin computes them from
                        // the assigned columns and those a trigger
                        // reads as NEW.x; the rest count as NULL.
                        let loaded = self.before_update_loaded(assigned.as_slice(), rowid_set);
                        for (i, v) in nrec.as_mut_slice().iter_mut().enumerate() {
                            if Some(i) != rowid_alias
                                && !g.is_gen(i)
                                && !loaded.as_slice().get(i).copied().unwrap_or(true)
                            {
                                *v = OwnedValue::Null;
                            }
                        }
                        g.fill::<S::Error>(nrec.as_mut_slice(), new_rowid, false)?;
                        for (i, v) in nrec.as_mut_slice().iter_mut().enumerate() {
                            if !g.is_gen(i) {
                                *v = new_rec
                                    .as_slice()
                                    .get(i)
                                    .cloned()
                                    .unwrap_or(OwnedValue::Null);
                            }
                        }
                    }
                    if !self
                        .fire(
                            crate::sql::ast::TrigTime::Before,
                            crate::sql::ast::TrigEvent::Update,
                            Some((assigned.as_slice(), rowid_set)),
                            Some((wrow.rec.as_slice(), wrow.rowid)),
                            Some((nrec.as_slice(), new_rowid)),
                        )
                        .await?
                    {
                        continue;
                    }
                    let Some(now) = self
                        .reload_write_row(root, ncols, defaults.as_slice(), cur_rowid)
                        .await?
                    else {
                        continue;
                    };
                    for (i, v) in new_rec.as_mut_slice().iter_mut().enumerate() {
                        if !assigned.as_slice().get(i).copied().unwrap_or(false) {
                            *v = now
                                .rec
                                .as_slice()
                                .get(i)
                                .cloned()
                                .unwrap_or(OwnedValue::Null);
                        }
                    }
                    cur_rec = now.rec;
                }
                // v0.10 NOT NULL: the combination's result row
                // is resolved under the statement's conflict
                // action before uniqueness is considered (the
                // pin's order — NOT NULL wins precedence, and
                // an OR REPLACE substitution is what the
                // uniqueness check then sees). OR IGNORE skips
                // this combination; later combinations of the
                // same row still get their turn.
                // v0.16: with the table's rules — generated
                // columns recomputed from the new values, STRICT
                // types, CHECK (OR IGNORE skips the row).
                let proceed = match &rules {
                    Some(r) => r.resolve::<S::Error>(
                        new_rec.as_mut_slice(),
                        new_rowid,
                        not_null.as_slice(),
                        defaults.as_slice(),
                        affs.as_slice(),
                        rowid_alias,
                        conflict_action,
                        Some((assigned.as_slice(), rowid_set)),
                        Some(&self.conn.env),
                    )?,
                    None => resolve_not_null::<S::Error>(
                        new_rec.as_mut_slice(),
                        nn_assigned.as_slice(),
                        defaults.as_slice(),
                        affs.as_slice(),
                        rowid_alias,
                        conflict_action,
                    )?,
                };
                if !proceed {
                    continue;
                }
                // Conflicts of the new values against OTHER rows
                // (the row as currently stored is `cur_rowid`):
                // collect every conflicting rowid, then let the
                // statement's conflict action decide.
                let mut conflict_rows: AVec<i64> = AVec::new();
                let mut first_err: Option<SchemaKind> = None;
                if new_rowid != cur_rowid
                    && btree::seek_rowid(&mut self.conn.pager, root, new_rowid)
                        .await?
                        .is_some()
                {
                    conflict_rows.push(new_rowid);
                    first_err = Some(SchemaKind::DuplicateRowid);
                }
                #[cfg(feature = "indexes")]
                for ix in indexes.iter() {
                    let key = key_from_rec(
                        ix.cols.as_slice(),
                        rowid_alias,
                        new_rec.as_slice(),
                        new_rowid,
                    );
                    if ix.unique
                        && key_has_no_null(key.as_slice())
                        && let Some(other) = btree::index_first_with_prefix(
                            &mut self.conn.pager,
                            ix.root,
                            ix.desc.as_slice(),
                            key.as_slice(),
                        )
                        .await?
                        && other != cur_rowid
                        && !conflict_rows.iter().any(|r| *r == other)
                    {
                        conflict_rows.push(other);
                        if first_err.is_none() {
                            first_err = Some(SchemaKind::UniqueViolation);
                        }
                    }
                }
                if !conflict_rows.is_empty() {
                    match conflict_action {
                        ConflictAction::Ignore => continue,
                        ConflictAction::Replace => {
                            #[cfg(feature = "v07")]
                            {
                                stale = true;
                                #[cfg(feature = "triggers")]
                                let mut triggered = false;
                                for v in conflict_rows.iter() {
                                    #[cfg(feature = "triggers")]
                                    if self
                                        .replace_victim(
                                            *v,
                                            first_err.unwrap_or(SchemaKind::UniqueViolation),
                                        )
                                        .await?
                                    {
                                        triggered = true;
                                        continue;
                                    }
                                    self.delete_row_completely(
                                        root,
                                        rowid_alias,
                                        ncols,
                                        defaults.as_slice(),
                                        *v,
                                    )
                                    .await?;
                                }
                                // DELETE triggers may have written the
                                // table: checked again, under ABORT.
                                #[cfg(feature = "triggers")]
                                if triggered
                                    && let Some(kind) = self
                                        .insert_conflict(
                                            root,
                                            new_rec.as_slice(),
                                            new_rowid,
                                            Some(cur_rowid),
                                        )
                                        .await?
                                {
                                    return Err(Error::Schema { kind });
                                }
                            }
                            #[cfg(not(feature = "v07"))]
                            {
                                return Err(Error::Schema {
                                    kind: first_err.unwrap_or(SchemaKind::UniqueViolation),
                                });
                            }
                        }
                        // ABORT, FAIL, ROLLBACK: the statement fails.
                        _ => {
                            return Err(Error::Schema {
                                kind: first_err.unwrap_or(SchemaKind::UniqueViolation),
                            });
                        }
                    }
                }
                // v0.16 (`foreign_keys`): the old row's keys while it is
                // stored, the new row's as if it were gone (the pin
                // deletes it in between).
                #[cfg(feature = "foreign_keys")]
                let fk_rows = if self.fk_active() {
                    let lold = self.fk_row(wrow.rec.as_slice(), wrow.rowid, affs.as_slice());
                    let lnew = self.fk_row(new_rec.as_slice(), new_rowid, affs.as_slice());
                    let ch = Some((assigned.as_slice(), rowid_set));
                    self.fk_old_row(lold.as_slice(), wrow.rowid, ch).await?;
                    self.fk_new_row(lnew.as_slice(), new_rowid, Some(cur_rowid), ch)
                        .await?;
                    Some((lold, lnew))
                } else {
                    None
                };
                // Apply against the row as currently stored.
                self.rewrite_stored_row(
                    root,
                    rowid_alias,
                    cur_rec.as_slice(),
                    cur_rowid,
                    new_rec.as_slice(),
                    new_rowid,
                )
                .await?;
                stale |= new_rowid != cur_rowid;
                #[cfg(feature = "foreign_keys")]
                if let Some((lold, lnew)) = &fk_rows {
                    self.fk_actions(
                        lold.as_slice(),
                        Some(lnew.as_slice()),
                        Some((assigned.as_slice(), rowid_set)),
                    )
                    .await?;
                    stale = true;
                }
                // RETURNING reads the row before its AFTER triggers
                // run (v0.16 review fix: the pin's RETURNING program
                // is the first of them, as for INSERT and DELETE).
                #[cfg(feature = "v07")]
                if !returning.is_empty() {
                    let rd =
                        row_data_for(rowid_alias, affs.as_slice(), new_rec.as_slice(), new_rowid);
                    let vals = self.eval_returning(returning.as_slice(), &rd).await?;
                    self.ret_rows.push(vals);
                    self.ret_rowids.push(Some(new_rowid));
                }
                #[cfg(feature = "triggers")]
                if self.fires(
                    crate::sql::ast::TrigTime::After,
                    crate::sql::ast::TrigEvent::Update,
                ) {
                    self.fire(
                        crate::sql::ast::TrigTime::After,
                        crate::sql::ast::TrigEvent::Update,
                        Some((assigned.as_slice(), rowid_set)),
                        Some((wrow.rec.as_slice(), wrow.rowid)),
                        Some((new_rec.as_slice(), new_rowid)),
                    )
                    .await?;
                }
                cur_rec = new_rec;
                cur_rowid = new_rowid;
                applied = true;
            }
            #[cfg(feature = "v16")]
            if applied {
                self.conn.env.note_change();
            }
            let _ = applied;
        }
        #[cfg(feature = "triggers")]
        if let Some(e) = failed {
            return Err(self.failed_part_way(e));
        }
        #[cfg(feature = "foreign_keys")]
        self.fk_end_commit().await?;
        #[cfg(not(feature = "foreign_keys"))]
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.finish(true);
        Ok(())
    }

    #[cfg(feature = "v05")]
    async fn exec_delete(&mut self) -> Result<(), S::Error> {
        #[cfg(feature = "v16")]
        self.conn.env.pending.set(0);
        #[cfg(feature = "triggers")]
        self.load_triggers()?;
        let (root, rowid_alias, ncols, defaults, affs, filter) = match &self.plan {
            Plan::Delete {
                root,
                rowid_alias,
                ncols,
                defaults,
                affs,
                filter,
                ..
            } => (
                *root,
                *rowid_alias,
                *ncols,
                defaults.clone(),
                affs.clone(),
                filter.clone(),
            ),
            _ => unreachable!(),
        };
        #[cfg(feature = "indexes")]
        let indexes: AVec<IndexPlan> = match &self.plan {
            Plan::Delete { indexes, .. } => indexes.clone(),
            _ => unreachable!(),
        };
        let mut rows = AVec::new();
        let scanned = self
            .scan_write_rows_into(
                root,
                rowid_alias,
                ncols,
                defaults.as_slice(),
                affs.as_slice(),
                &filter,
                &mut rows,
            )
            .await;
        #[cfg(not(feature = "triggers"))]
        scanned?;
        #[cfg(feature = "v07")]
        let returning = match &self.plan {
            Plan::Delete { returning, .. } => returning.clone(),
            _ => unreachable!(),
        };
        #[cfg(feature = "foreign_keys")]
        self.fk_load(fkey::Event::Delete, true)?;
        // A trigger program's one-pass DELETE keeps the rows its
        // WHERE passed before failing (`one_pass`).
        #[cfg(feature = "triggers")]
        let failed = match scanned {
            Ok(()) => None,
            Err(e)
                if self.one_pass(
                    crate::sql::ast::TrigEvent::Delete,
                    None,
                    filter.as_ref(),
                    false,
                ) =>
            {
                Some(e)
            }
            Err(e) => return Err(e),
        };
        for snap_row in rows.iter() {
            // v0.16 (`triggers`): with triggers each row is read again
            // when its turn comes (gone: skipped), BEFORE DELETE
            // triggers see it, and it is read once more after them;
            // AFTER DELETE triggers see the row as first read.
            #[cfg(feature = "triggers")]
            let (old_row, cur_row);
            #[cfg(feature = "triggers")]
            let wrow = if self.trig.is_some() {
                let Some(r) = self
                    .reload_write_row(root, ncols, defaults.as_slice(), snap_row.rowid)
                    .await?
                else {
                    continue;
                };
                old_row = Some(r);
                let Some(old_row) = &old_row else {
                    continue;
                };
                if self.fires(
                    crate::sql::ast::TrigTime::Before,
                    crate::sql::ast::TrigEvent::Delete,
                ) {
                    if !self
                        .fire(
                            crate::sql::ast::TrigTime::Before,
                            crate::sql::ast::TrigEvent::Delete,
                            None,
                            Some((old_row.rec.as_slice(), old_row.rowid)),
                            None,
                        )
                        .await?
                    {
                        continue;
                    }
                    let Some(r) = self
                        .reload_write_row(root, ncols, defaults.as_slice(), snap_row.rowid)
                        .await?
                    else {
                        continue;
                    };
                    cur_row = r;
                    &cur_row
                } else {
                    old_row
                }
            } else {
                old_row = None;
                snap_row
            };
            #[cfg(not(feature = "triggers"))]
            let wrow = snap_row;
            // v0.16 (`foreign_keys`): an earlier row's actions may have
            // deleted or changed this one; its keys are checked while
            // it is stored, its actions run once it is gone.
            #[cfg(feature = "foreign_keys")]
            let fk_now;
            #[cfg(feature = "foreign_keys")]
            let (wrow, fk_old) = if self.fk_active() {
                let Some(r) = self
                    .reload_write_row(root, ncols, defaults.as_slice(), wrow.rowid)
                    .await?
                else {
                    continue;
                };
                fk_now = r;
                let lrow = self.fk_row(fk_now.rec.as_slice(), fk_now.rowid, affs.as_slice());
                self.fk_old_row(lrow.as_slice(), fk_now.rowid, None).await?;
                (&fk_now, Some(lrow))
            } else {
                (wrow, None)
            };
            #[cfg(feature = "indexes")]
            for ix in indexes.iter() {
                let key = key_from_rec(
                    ix.cols.as_slice(),
                    rowid_alias,
                    wrow.rec.as_slice(),
                    wrow.rowid,
                );
                let payload = encode_index_payload(key.as_slice(), wrow.rowid);
                btree::index_delete(
                    &mut self.conn.pager,
                    ix.root,
                    payload.as_slice(),
                    ix.desc.as_slice(),
                )
                .await?;
            }
            btree::btree_delete(&mut self.conn.pager, root, wrow.rowid).await?;
            #[cfg(feature = "v16")]
            self.conn.env.note_change();
            #[cfg(feature = "foreign_keys")]
            if let Some(lrow) = &fk_old {
                self.fk_actions(lrow.as_slice(), None, None).await?;
            }
            // RETURNING reads the row's pre-delete values, its
            // subqueries the table with this row (and every
            // earlier one) gone — the pin evaluates it after the
            // row's delete (v0.16 review fix: it ran before).
            #[cfg(feature = "v07")]
            if !returning.is_empty() {
                let rd = row_data_for(
                    rowid_alias,
                    affs.as_slice(),
                    wrow.rec.as_slice(),
                    wrow.rowid,
                );
                let vals = self.eval_returning(returning.as_slice(), &rd).await?;
                self.ret_rows.push(vals);
                self.ret_rowids.push(Some(wrow.rowid));
            }
            #[cfg(feature = "triggers")]
            if let Some(o) = &old_row
                && self.fires(
                    crate::sql::ast::TrigTime::After,
                    crate::sql::ast::TrigEvent::Delete,
                )
            {
                self.fire(
                    crate::sql::ast::TrigTime::After,
                    crate::sql::ast::TrigEvent::Delete,
                    None,
                    Some((o.rec.as_slice(), o.rowid)),
                    None,
                )
                .await?;
            }
        }
        #[cfg(feature = "triggers")]
        if let Some(e) = failed {
            return Err(self.failed_part_way(e));
        }
        #[cfg(feature = "foreign_keys")]
        self.fk_end_commit().await?;
        #[cfg(not(feature = "foreign_keys"))]
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        #[cfg(feature = "v16")]
        self.conn.env.finish(true);
        Ok(())
    }

    /// Delete the statistics rows of a dropped table (`col` 0, the
    /// `tbl` column) or index (`col` 1, `idx`) from every
    /// `sqlite_stat1` .. `sqlite_stat4` table the database has, as
    /// the pin's DROP TABLE / DROP INDEX do (v0.16 review fix,
    /// ungated: hipp left them, so a later table of the same name
    /// inherited made-up statistics). `name` is the name as stored
    /// in the schema, compared exactly, as the pin's `tbl = name`.
    #[cfg(any(feature = "v05", feature = "indexes"))]
    async fn clear_stat_rows(&mut self, col: usize, name: &[u8]) -> Result<(), S::Error> {
        for stat in [
            b"sqlite_stat1",
            b"sqlite_stat2",
            b"sqlite_stat3",
            b"sqlite_stat4",
        ] {
            let Some(t) = self.conn.catalog.find(&self.conn.catalog_arena, stat) else {
                continue;
            };
            let root = t.root;
            let mut gone = AVec::new();
            let mut cursor = ScanCursor::new(root);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), col + 1)?;
                if matches!(values.as_slice().get(col), Some(Value::Text(n)) if *n == name) {
                    gone.push(rowid);
                }
            }
            for r in gone.iter() {
                btree::btree_delete(&mut self.conn.pager, root, *r).await?;
            }
        }
        Ok(())
    }

    /// `DROP TABLE` (v0.5): free every page of the table B-tree
    /// and of each of its indexes (B-tree pages + overflow
    /// chains) to the freelist, then remove the schema rows.
    /// Collection happens before any freeing, so the traversals
    /// see intact trees.
    #[cfg(feature = "v05")]
    async fn exec_drop_table(&mut self) -> Result<(), S::Error> {
        let (root, index_roots, seq) = match &self.plan {
            Plan::DropTable {
                root,
                index_roots,
                seq,
            } => (*root, index_roots.clone(), seq.clone()),
            _ => unreachable!(),
        };
        let Some(root) = root else {
            return Ok(()); // IF EXISTS no-op
        };
        let Ast::DropTable { name, .. } = &self.ast else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        // v0.16 (`foreign_keys`): the pin's implicit DELETE first.
        #[cfg(feature = "foreign_keys")]
        self.fk_drop(name_b.as_slice()).await?;
        // Schema rowids: the table's own row plus every index
        // row it owns (explicit and implicit alike).
        let mut schema_rowids = AVec::new();
        let mut stored: Option<AVec<u8>> = None;
        {
            let mut cursor = ScanCursor::new(1);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
                if values.len() >= 3 {
                    let is_table_row = matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"table"))
                        && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, name_b.as_slice()));
                    if is_table_row && let Value::Text(n) = &values.as_slice()[1] {
                        stored = Some(copy_vec(n));
                    }
                    // Its indexes and (v0.16, every profile) its
                    // triggers go with it, as on the pin.
                    let is_index_row = matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"index") || eq_ci(t, b"trigger"))
                        && matches!(&values.as_slice()[2], Value::Text(n) if eq_ci(n, name_b.as_slice()));
                    if is_table_row || is_index_row {
                        schema_rowids.push(rowid);
                    }
                }
            }
        }
        // Before any page is freed: the table may be a statistics
        // table itself.
        if let Some(n) = &stored {
            self.clear_stat_rows(0, n.as_slice()).await?;
        }
        let mut pages = btree::collect_table_pages(&mut self.conn.pager, root).await?;
        for ir in index_roots.iter() {
            let ipages = btree::collect_index_pages(&mut self.conn.pager, *ir).await?;
            for p in ipages.iter() {
                pages.push(*p);
            }
        }
        for p in pages.iter() {
            self.conn.pager.free_page(*p).await?;
        }
        for r in schema_rowids.iter() {
            btree::btree_delete(&mut self.conn.pager, 1, *r).await?;
        }
        if let Some((sroot, tname)) = seq {
            let mut gone = AVec::new();
            let mut cursor = ScanCursor::new(sroot);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 1)?;
                if matches!(values.as_slice().first(), Some(Value::Text(n)) if *n == tname.as_slice())
                {
                    gone.push(rowid);
                }
            }
            for r in gone.iter() {
                btree::btree_delete(&mut self.conn.pager, sroot, *r).await?;
            }
        }
        self.conn.pager.bump_schema_cookie();
        #[cfg(feature = "foreign_keys")]
        {
            self.fk_end_commit().await?;
            if core::mem::take(&mut self.conn.fk.drop_counted) {
                self.conn.env.finish(true);
            }
        }
        #[cfg(not(feature = "foreign_keys"))]
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// The v0.3 secondary-index scan: resolve the lookup's
    /// rowids once, then fetch each row by rowid.
    #[cfg(feature = "indexes")]
    async fn step_scan_index(&mut self) -> Result<Step, S::Error> {
        let (table_root, idx_root, desc, probe) = {
            let Plan::Scan { root, access, .. } = &self.plan else {
                unreachable!()
            };
            match access {
                Access::Index {
                    root: ir,
                    desc,
                    probe,
                } => (*root, *ir, desc.clone(), probe.clone()),
                _ => unreachable!(),
            }
        };
        if !self.index_loaded {
            let probe1 = [probe];
            self.index_rowids = btree::index_prefix_rowids(
                &mut self.conn.pager,
                idx_root,
                desc.as_slice(),
                &probe1,
            )
            .await?;
            self.index_pos = 0;
            self.index_loaded = true;
        }
        loop {
            if self.index_pos >= self.index_rowids.len() {
                self.phase = Phase::Done;
                self.current_rowid = None;
                return Ok(Step::Done);
            }
            let rowid = self.index_rowids.as_slice()[self.index_pos];
            self.index_pos += 1;
            let found = btree::seek_rowid(&mut self.conn.pager, table_root, rowid).await?;
            if let Some(payload) = found {
                self.row_buf = payload;
                self.project_current(rowid)?;
                self.phase = Phase::Running;
                return Ok(Step::Row);
            }
            // An index entry without a table row cannot occur in a
            // consistent file; skip it rather than tear.
        }
    }

    async fn step_scan(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        // Per-row checkpoint rewind (§7.5.2): the previous row's
        // arena temporaries die before the next row is decoded.
        self.arena.rewind(self.row_ckpt);
        #[cfg(feature = "indexes")]
        {
            let is_index = matches!(
                &self.plan,
                Plan::Scan {
                    access: Access::Index { .. },
                    ..
                }
            );
            if is_index {
                return self.step_scan_index().await;
            }
        }
        let Plan::Scan { root, access, .. } = &self.plan else {
            unreachable!()
        };
        let root = *root;
        enum SimpleAccess {
            FullScan,
            Rowid(i64),
            Empty,
        }
        let access = match access {
            Access::FullScan => SimpleAccess::FullScan,
            Access::Rowid(r) => SimpleAccess::Rowid(*r),
            Access::Empty => SimpleAccess::Empty,
            #[cfg(feature = "indexes")]
            Access::Index { .. } => unreachable!(),
        };
        match access {
            SimpleAccess::Empty => {
                self.phase = Phase::Done;
                self.current_rowid = None;
                Ok(Step::Done)
            }
            SimpleAccess::Rowid(want) => {
                if self.phase == Phase::Running {
                    // The single matching row was already produced.
                    self.phase = Phase::Done;
                    self.current_rowid = None;
                    return Ok(Step::Done);
                }
                let found = btree::seek_rowid(&mut self.conn.pager, root, want).await?;
                match found {
                    None => {
                        self.phase = Phase::Done;
                        self.current_rowid = None;
                        Ok(Step::Done)
                    }
                    Some(payload) => {
                        self.row_buf = payload;
                        self.project_current(want)?;
                        self.phase = Phase::Running;
                        Ok(Step::Row)
                    }
                }
            }
            SimpleAccess::FullScan => {
                if self.cursor.is_none() {
                    self.cursor = Some(ScanCursor::new(root));
                }
                let cursor = self.cursor.as_mut().expect("just set");
                let next = cursor
                    .next_row_into(&mut self.conn.pager, &mut self.row_buf)
                    .await?;
                let Some(rowid) = next else {
                    self.phase = Phase::Done;
                    self.current_rowid = None;
                    return Ok(Step::Done);
                };
                self.project_current(rowid)?;
                self.phase = Phase::Running;
                Ok(Step::Row)
            }
        }
    }

    /// Decode `row_buf` (the payload of the row at `rowid`) into
    /// the current-row values, applying the plan's projection and
    /// read-side affinity rules. Shared by the scan and rowid-seek
    /// access paths.
    fn project_current(&mut self, rowid: i64) -> Result<(), S::Error> {
        let Plan::Scan {
            rowid_alias,
            ncols,
            out,
            aff,
            defaults,
            ..
        } = &self.plan
        else {
            unreachable!()
        };
        let mut dec: AVec<Value<'_>> = AVec::new();
        crate::record::decode_record_into::<S::Error>(self.row_buf.as_slice(), *ncols, &mut dec)?;
        self.current.clear();
        for (j, idx) in out.iter().enumerate() {
            if *idx == ROWID_COL || Some(*idx) == *rowid_alias {
                self.current.push(OwnedValue::Integer(rowid));
                continue;
            }
            let mut v = if *idx < dec.len() {
                OwnedValue::from_value(dec.as_slice()[*idx])
            } else {
                defaults.as_slice()[*idx].clone()
            };
            // Read-side affinity forcing (§5.5): REAL columns
            // yield reals even when the record stores an integer.
            if aff.as_slice()[j] == crate::record::Affinity::Real
                && let OwnedValue::Integer(i) = v
            {
                v = OwnedValue::Real(i as f64);
            }
            self.current.push(v);
        }
        self.current_rowid = Some(rowid);
        Ok(())
    }

    // ---------------- v0.4 query execution ----------------

    #[cfg(feature = "v04")]
    async fn step_query(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.query_loaded {
            self.run_query().await?;
            self.query_loaded = true;
            self.query_pos = 0;
        }
        if self.query_pos >= self.query_rows.len() {
            self.phase = Phase::Done;
            self.current_rowid = None;
            return Ok(Step::Done);
        }
        self.current = self.query_rows.as_slice()[self.query_pos].clone();
        self.current_rowid = self.query_rowids.as_slice()[self.query_pos];
        self.query_pos += 1;
        self.phase = Phase::Running;
        Ok(Step::Row)
    }

    /// Materialise a v0.4 query end to end (SPEC §5.7's
    /// tree-walking executor, materialising flavour): scan every
    /// source, nested-loop join, filter, group/project, distinct,
    /// sort, limit. The plan's fast paths keep the common
    /// single-table scans off this code entirely.
    #[cfg(feature = "v04")]
    async fn run_query(&mut self) -> Result<(), S::Error> {
        let Plan::Query(qp) = &self.plan else {
            unreachable!()
        };
        let qp = qp.clone();
        #[cfg(feature = "v09")]
        if !qp.compound.is_empty() || !qp.values.is_empty() {
            return self.run_query_compound(qp).await;
        }
        #[cfg(feature = "v06")]
        if query_plan_has_sub(&qp) {
            return self.run_query_sub(qp).await;
        }
        // LIMIT / OFFSET first: the pin computes them before
        // the scan, so a bad one errors even over no rows.
        let window = self.limit_window(&qp.limit, &qp.offset).await?;
        let src_rows = self
            .materialise_query_sources(qp.sources.as_slice())
            .await?;
        // Nested-loop join + WHERE, then group / project: the
        // synchronous v0.4 executor (no subqueries in the plan).
        let mut rows: AVec<RowData> = AVec::new();
        join_all::<S::Error>(&self.conn.env, &qp, src_rows.as_slice(), &mut rows)?;
        let produced = project_rows::<S::Error>(
            &self.conn.env,
            &qp,
            rows.as_slice(),
            group_cap(
                qp.group_by.as_slice(),
                qp.order_by.as_slice(),
                qp.distinct,
                window,
            ),
        )?;
        self.finish_produced(&qp, produced, window);
        Ok(())
    }

    /// Evaluate a plan's LIMIT / OFFSET (v0.16) into the row
    /// window `(skip, take)`: each expression evaluates once,
    /// against the empty row (it was planned against an empty
    /// scope), and must pass the pin's `MustBeInt` conversion
    /// (`query::limit_int`). A negative LIMIT means no limit and
    /// a negative OFFSET means none; counts beyond the address
    /// space saturate rather than wrap on 32-bit targets.
    #[cfg(feature = "v04")]
    async fn limit_window(
        &mut self,
        limit: &Option<RExpr>,
        offset: &Option<RExpr>,
    ) -> Result<(usize, usize), S::Error> {
        let mut vals = [None, None];
        for (slot, e) in vals.iter_mut().zip([limit, offset]) {
            let Some(e) = e else { continue };
            #[cfg(feature = "v06")]
            let v = self.eval_row_sub(e, &RowData::empty()).await?;
            #[cfg(not(feature = "v06"))]
            let v = crate::query::eval::<S::Error>(
                e,
                &Ctx::of(&RowData::empty()).with(&self.conn.env),
            )?;
            *slot = Some(crate::query::limit_int::<S::Error>(v)?);
        }
        let [lim, off] = vals;
        let take = match lim {
            Some(l) if l >= 0 => usize::try_from(l).unwrap_or(usize::MAX),
            _ => usize::MAX,
        };
        let skip = match off {
            Some(o) if o > 0 => usize::try_from(o).unwrap_or(usize::MAX),
            _ => 0,
        };
        Ok((skip, take))
    }

    /// Scan one query source's rows into memory (v0.4 executor
    /// step 1, shared by the subquery path): decoded values with
    /// defaults materialised and read-side REAL forcing (§5.5).
    #[cfg(feature = "v04")]
    async fn materialise_query_sources(
        &mut self,
        sources: &[SourcePlan],
    ) -> Result<AVec<AVec<(AVec<OwnedValue>, Option<i64>)>>, S::Error> {
        let mut src_rows: AVec<AVec<(AVec<OwnedValue>, Option<i64>)>> = AVec::new();
        for sp in sources.iter() {
            // Derived sources execute through the subquery
            // path (materialise_sources_sub); reaching one here
            // is a dispatch bug, refused rather than misread.
            if sp.derived.is_some() {
                return Err(Error::Unsupported);
            }
            #[cfg(feature = "v16")]
            if sp.rec != 0 {
                return Err(Error::Unsupported);
            }
            let mut rows = AVec::new();
            let mut cursor = ScanCursor::new(sp.root);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                if let Some(g) = &sp.gcols {
                    // v0.16: a table with generated columns —
                    // decode around the VIRTUAL ones, compute
                    // them, then read the row as any other.
                    let mut rec = crate::rules::decode_rec::<S::Error>(
                        buf.as_slice(),
                        sp.ncols,
                        sp.defaults.as_slice(),
                        Some(g.virt.as_slice()),
                    )?;
                    g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
                    let rd = crate::rules::eval_row(
                        sp.rowid_alias,
                        sp.affs.as_slice(),
                        rec.as_slice(),
                        rowid,
                    );
                    rows.push((rd.vals, Some(rowid)));
                    continue;
                }
                let mut dec: AVec<Value<'_>> = AVec::new();
                crate::record::decode_record_into::<S::Error>(buf.as_slice(), sp.ncols, &mut dec)?;
                let mut vals = AVec::new();
                for i in 0..sp.ncols {
                    let mut v = if Some(i) == sp.rowid_alias {
                        OwnedValue::Integer(rowid)
                    } else if i < dec.len() {
                        OwnedValue::from_value(dec.as_slice()[i])
                    } else {
                        sp.defaults.as_slice()[i].clone()
                    };
                    // Read-side REAL forcing (§5.5).
                    if sp.affs.as_slice()[i] == crate::record::Affinity::Real
                        && let OwnedValue::Integer(x) = v
                    {
                        v = OwnedValue::Real(x as f64);
                    }
                    vals.push(v);
                }
                rows.push((vals, Some(rowid)));
            }
            src_rows.push(rows);
        }
        Ok(src_rows)
    }

    /// Materialise query sources for the async paths (v0.9):
    /// like `materialise_query_sources`, but a derived source
    /// executes its subplan against `outer` (the empty row at
    /// statement level) and contributes rowid-less rows.
    #[cfg(feature = "v06")]
    async fn materialise_sources_sub(
        &mut self,
        sources: &[SourcePlan],
        outer: &RowData,
        need: Option<usize>,
    ) -> Result<AVec<AVec<(AVec<OwnedValue>, Option<i64>)>>, S::Error> {
        #[cfg(not(feature = "v09"))]
        let _ = (outer, need);
        let mut src_rows: AVec<AVec<(AVec<OwnedValue>, Option<i64>)>> = AVec::new();
        for sp in sources.iter() {
            // v0.16: a recursive reference's one row.
            #[cfg(feature = "v16")]
            if sp.rec != 0 {
                let mut rows = AVec::new();
                if let Some((_, r)) = self.rec_rows.iter().rev().find(|(id, _)| *id == sp.rec) {
                    rows.push((r.clone(), None));
                }
                src_rows.push(rows);
                continue;
            }
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &sp.derived {
                    // `need` bounds the first source only (the outer
                    // loop the reader stops early).
                    let need = if src_rows.is_empty() { need } else { None };
                    #[cfg(feature = "v16")]
                    let inner = if d.mat != 0 {
                        self.shared_rows(d, outer, need).await?
                    } else {
                        self.exec_subplan_need(d, outer, need).await?
                    };
                    #[cfg(not(feature = "v16"))]
                    let inner = self.exec_subplan_need(d, outer, need).await?;
                    let mut rows = AVec::new();
                    for r in inner.iter() {
                        rows.push((r.clone(), None));
                    }
                    src_rows.push(rows);
                    continue;
                }
            }
            let mut rows = AVec::new();
            let mut cursor = ScanCursor::new(sp.root);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                if let Some(g) = &sp.gcols {
                    // v0.16: a table with generated columns —
                    // decode around the VIRTUAL ones, compute
                    // them, then read the row as any other.
                    let mut rec = crate::rules::decode_rec::<S::Error>(
                        buf.as_slice(),
                        sp.ncols,
                        sp.defaults.as_slice(),
                        Some(g.virt.as_slice()),
                    )?;
                    g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
                    let rd = crate::rules::eval_row(
                        sp.rowid_alias,
                        sp.affs.as_slice(),
                        rec.as_slice(),
                        rowid,
                    );
                    rows.push((rd.vals, Some(rowid)));
                    continue;
                }
                let mut dec: AVec<Value<'_>> = AVec::new();
                crate::record::decode_record_into::<S::Error>(buf.as_slice(), sp.ncols, &mut dec)?;
                let mut vals = AVec::new();
                for i in 0..sp.ncols {
                    let mut v = if Some(i) == sp.rowid_alias {
                        OwnedValue::Integer(rowid)
                    } else if i < dec.len() {
                        OwnedValue::from_value(dec.as_slice()[i])
                    } else {
                        sp.defaults.as_slice()[i].clone()
                    };
                    if sp.affs.as_slice()[i] == crate::record::Affinity::Real
                        && let OwnedValue::Integer(x) = v
                    {
                        v = OwnedValue::Real(x as f64);
                    }
                    vals.push(v);
                }
                rows.push((vals, Some(rowid)));
            }
            src_rows.push(rows);
        }
        Ok(src_rows)
    }

    /// Execute a compound / VALUES-core top-level query (v0.9):
    /// each core's rows through the async core path, combined
    /// left to right, then the compound ORDER BY / LIMIT.
    #[cfg(feature = "v09")]
    async fn run_query_compound(&mut self, qp: QueryPlan) -> Result<(), S::Error> {
        let window = self.limit_window(&qp.limit, &qp.offset).await?;
        let head = self.exec_qp_core_values(&qp).await?;
        let mut tails = AVec::new();
        let mut cores = AVec::new();
        cores.push(core_colls(qp.items.as_slice(), qp.values.as_slice()));
        for (op, tail) in qp.compound.iter() {
            tails.push((*op, self.exec_qp_core_values(tail).await?));
            cores.push(core_colls(tail.items.as_slice(), tail.values.as_slice()));
        }
        let rows = if union_first_row(
            &qp.limit,
            &qp.offset,
            qp.comp_order.as_slice(),
            tails.as_slice(),
        ) {
            first_core_row(head, tails.as_slice())
        } else {
            run_compound(
                head,
                tails.as_slice(),
                cores.as_slice(),
                qp.comp_order.as_slice(),
            )
        };
        let rows = apply_window(rows, window);
        let mut ids: AVec<Option<i64>> = AVec::new();
        for _ in rows.iter() {
            ids.push(None);
        }
        self.query_rows = rows;
        self.query_rowids = ids;
        Ok(())
    }

    /// One core's output rows for a compound top-level query
    /// (v0.9): a VALUES core evaluates its rows against the
    /// empty row; a SELECT core runs the async join/project
    /// path. The core's own DISTINCT applies; ORDER BY / LIMIT
    /// belong to the compound and are the caller's job.
    #[cfg(feature = "v09")]
    async fn exec_qp_core_values(
        &mut self,
        qp: &QueryPlan,
    ) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        if !qp.values.is_empty() {
            let empty = RowData::empty();
            let mut out = AVec::new();
            for r in qp.values.iter() {
                #[cfg(feature = "window")]
                let rv = self.eval_values_row(r.as_slice(), &empty).await?;
                #[cfg(not(feature = "window"))]
                let rv = {
                    let mut rv = AVec::new();
                    for e in r.iter() {
                        rv.push(self.eval_row_sub(e, &empty).await?);
                    }
                    rv
                };
                out.push(rv);
            }
            return Ok(out);
        }
        let src_rows = self
            .materialise_sources_sub(qp.sources.as_slice(), &RowData::empty(), None)
            .await?;
        let mut rows: AVec<RowData> = AVec::new();
        self.join_sources(
            qp.sources.as_slice(),
            qp.filter.as_ref(),
            src_rows.as_slice(),
            &RowData::empty(),
            &mut rows,
        )
        .await?;
        let produced = self.project_qp_async(qp, rows.as_slice(), None).await?;
        let mut out: AVec<AVec<OwnedValue>> = AVec::new();
        for p in produced.iter() {
            out.push(p.out.clone());
        }
        if qp.distinct {
            let colls = crate::query::exprs_colls(qp.items.as_slice());
            out = dedup_compound_rows(out, colls.as_slice());
        }
        Ok(out)
    }

    /// DISTINCT, ORDER BY, LIMIT/OFFSET over produced rows
    /// (v0.4 executor steps 4-6, shared by the subquery path;
    /// v0.16: NULLS FIRST / LAST honoured, and the row window
    /// comes evaluated from `limit_window`).
    #[cfg(feature = "v04")]
    fn finish_produced(
        &mut self,
        qp: &QueryPlan,
        mut produced: AVec<Produced>,
        (skip, take): (usize, usize),
    ) {
        if qp.distinct {
            produced = dedup_produced(
                produced,
                crate::query::exprs_colls(qp.items.as_slice()).as_slice(),
            );
        }
        produced.as_mut_slice().sort_by(|x, y| {
            crate::query::order_keys_cmp(
                x.keys.as_slice(),
                y.keys.as_slice(),
                qp.order_by.iter().map(|(_, o)| *o),
            )
        });
        let mut kept_rows: AVec<AVec<OwnedValue>> = AVec::new();
        let mut kept_ids: AVec<Option<i64>> = AVec::new();
        for p in produced.iter().skip(skip).take(take) {
            kept_rows.push(p.out.clone());
            kept_ids.push(p.rowid);
        }
        self.query_rows = kept_rows;
        self.query_rowids = kept_ids;
    }

    // ---------------- v0.6 subquery execution ----------------

    /// The v0.6 query path: like `run_query`, but ON / WHERE /
    /// projection evaluate through `eval_row_sub`, which
    /// executes each subquery node against the pager for the
    /// row at hand and substitutes its value before the
    /// synchronous evaluator runs.
    #[cfg(feature = "v06")]
    async fn run_query_sub(&mut self, qp: QueryPlan) -> Result<(), S::Error> {
        let window = self.limit_window(&qp.limit, &qp.offset).await?;
        // Window calls (v0.16, `window`) read every input row: no
        // bound passes into the sources.
        #[cfg(feature = "window")]
        let wins = !crate::window::calls(qp.items.as_slice(), qp.order_by.as_slice()).is_empty();
        #[cfg(not(feature = "window"))]
        #[cfg_attr(not(feature = "v09"), allow(unused_variables))]
        let wins = false;
        #[cfg(feature = "v09")]
        let need = derived_need(
            qp.sources.as_slice(),
            wins || qp.filter.is_some() || !qp.group_by.is_empty() || qp.has_agg,
            !qp.order_by.is_empty() || qp.distinct,
            window,
        );
        #[cfg(not(feature = "v09"))]
        let need = None;
        // v0.16 review fix: an endless recursive CTE read as the
        // outer loop through a WHERE or a join stops at the
        // reader's LIMIT, as the pin's co-routine does.
        #[cfg(feature = "v16")]
        let grow = !wins
            && streams_first(
                qp.sources.as_slice(),
                qp.group_by.is_empty() && !qp.has_agg && qp.order_by.is_empty() && !qp.distinct,
            )
            && (qp.filter.is_some() || qp.sources.len() > 1)
            && window.1 != usize::MAX;
        #[cfg(not(feature = "v16"))]
        let grow = false;
        let target = window.0.saturating_add(window.1);
        let mut n = if grow { target.max(1) } else { 0 };
        let mut rows: AVec<RowData> = AVec::new();
        loop {
            let src_rows = self
                .materialise_sources_sub(
                    qp.sources.as_slice(),
                    &RowData::empty(),
                    if grow { Some(n) } else { need },
                )
                .await?;
            rows.clear();
            self.join_sources(
                qp.sources.as_slice(),
                qp.filter.as_ref(),
                src_rows.as_slice(),
                &RowData::empty(),
                &mut rows,
            )
            .await?;
            let first = src_rows.as_slice().first().map_or(0, |r| r.len());
            if !grow || rows.len() >= target || first < n {
                break;
            }
            n = n.saturating_mul(2);
        }
        let produced = self
            .project_qp_async(
                &qp,
                rows.as_slice(),
                group_cap(
                    qp.group_by.as_slice(),
                    qp.order_by.as_slice(),
                    qp.distinct,
                    window,
                ),
            )
            .await?;
        self.finish_produced(&qp, produced, window);
        Ok(())
    }

    /// One VALUES row against `outer` (v0.16, `window`): its window
    /// calls run over the row alone, as the pin's one-row SELECT
    /// core it is.
    #[cfg(feature = "window")]
    async fn eval_values_row(
        &mut self,
        r: &[RExpr],
        outer: &RowData,
    ) -> Result<AVec<OwnedValue>, S::Error> {
        let mut rv = AVec::new();
        {
            let wins = crate::window::calls(r, &[]);
            if !wins.is_empty() {
                let mut ins = AVec::new();
                for w in wins.iter() {
                    let mut v = AVec::new();
                    for k in crate::window::children(w) {
                        v.push(self.eval_row_sub(k, outer).await?);
                    }
                    ins.push(v);
                }
                let mut units = AVec::new();
                units.push(ins);
                let (_, vals) = crate::window::compute(wins.as_slice(), units.as_slice())
                    .map_err(|kind| Error::Schema { kind })?;
                let empty = AVec::new();
                let v = vals.as_slice().first().unwrap_or(&empty);
                for e in r {
                    let mut t = crate::window::strip(e);
                    crate::window::fill(&mut t, v.as_slice());
                    rv.push(self.eval_row_sub(&t, outer).await?);
                }
                return Ok(rv);
            }
        }
        for e in r {
            rv.push(self.eval_row_sub(e, outer).await?);
        }
        Ok(rv)
    }

    /// Evaluate one expression for one row, executing any
    /// subquery nodes it contains first (v0.6). Expressions
    /// without subqueries take the plain synchronous path.
    #[cfg(feature = "v06")]
    async fn eval_row_sub(&mut self, e: &RExpr, row: &RowData) -> Result<OwnedValue, S::Error> {
        if !crate::query::has_sub(e) {
            return crate::query::eval::<S::Error>(e, &Ctx::of(row).with(&self.conn.env));
        }
        let values = self.compute_sub_values(e, row).await?;
        let mut pos = 0;
        let e2 = crate::query::substitute(e, values.as_slice(), &mut pos);
        crate::query::eval::<S::Error>(&e2, &Ctx::of(row).with(&self.conn.env))
    }

    /// Evaluate one expression for an aggregate group (v0.6):
    /// subqueries resolve against the group's representative
    /// row, exactly like bare columns.
    ///
    /// v0.11: a subquery inside an aggregate's arguments
    /// resolves per group row instead — the aggregate's
    /// arguments are evaluated for every row of the group
    /// (each subquery correlated to that row), the fold runs
    /// over those values, and the folded literal replaces the
    /// aggregate node before the rest of the expression
    /// evaluates against the representative row.
    #[cfg(feature = "v06")]
    async fn eval_group_sub(
        &mut self,
        e: &RExpr,
        rows: &[RowData],
        rep: &RowData,
    ) -> Result<OwnedValue, S::Error> {
        if !crate::query::has_sub(e) {
            return crate::query::eval::<S::Error>(e, &Ctx::group(rows, rep).with(&self.conn.env));
        }
        // This query's aggregates inside its subqueries (v0.16
        // fix: `(SELECT max(t.a) FROM u)` over t): fold each over
        // the group and substitute the value into the subquery
        // plan before anything executes it.
        if crate::query::has_owned_sub_agg(e) {
            let mut e2 = e.clone();
            crate::query::subst_owned_aggs::<S::Error>(&mut e2, 0, &mut |a| {
                let RExpr::Agg { args, .. } = a else {
                    return Err(Error::Unsupported);
                };
                if args.iter().any(crate::query::has_sub) {
                    return Err(Error::Unsupported);
                }
                let Some(p) = crate::query::agg_parts(a) else {
                    return Err(Error::Unsupported);
                };
                let mut arg_rows: AVec<AVec<OwnedValue>> = AVec::new();
                for r in rows {
                    let ctx = Ctx::of(r).with(&self.conn.env);
                    if let Some(vs) = crate::query::agg_row::<S::Error>(&p, &mut |x| {
                        crate::query::eval::<S::Error>(x, &ctx)
                    })? {
                        arg_rows.push(vs);
                    }
                }
                crate::agg::fold::<S::Error>(&p, arg_rows)
            })?;
            return Box::pin(self.eval_group_sub(&e2, rows, rep)).await;
        }
        if !crate::query::sub_in_agg_args(e) {
            let values = self.compute_sub_values(e, rep).await?;
            let mut pos = 0;
            let e2 = crate::query::substitute(e, values.as_slice(), &mut pos);
            return crate::query::eval::<S::Error>(
                &e2,
                &Ctx::group(rows, rep).with(&self.conn.env),
            );
        }
        // Per-group-row aggregate path (v0.11): fold every
        // outermost aggregate over per-row argument values,
        // substitute the outside-aggregate subqueries against
        // the representative row, then evaluate synchronously.
        let mut agg_exprs: AVec<&RExpr> = AVec::new();
        crate::query::collect_outer_aggs(e, &mut agg_exprs);
        let mut agg_values: AVec<OwnedValue> = AVec::new();
        for a in agg_exprs.iter() {
            agg_values.push(self.eval_agg_async(a, rows).await?);
        }
        let values = self.compute_sub_values_outside_agg(e, rep).await?;
        let mut pos = 0;
        let mut agg_pos = 0;
        let e2 = crate::query::substitute_group(
            e,
            values.as_slice(),
            &mut pos,
            agg_values.as_slice(),
            &mut agg_pos,
        );
        crate::query::eval::<S::Error>(&e2, &Ctx::group(rows, rep).with(&self.conn.env))
    }

    /// Fold one aggregate node over a group's rows, evaluating
    /// its arguments per row through `eval_row_sub` so any
    /// subquery in an argument is executed against that row
    /// (v0.11). The fold itself is `query::eval_agg_values`,
    /// shared with the synchronous evaluator.
    #[cfg(feature = "v06")]
    async fn eval_agg_async(
        &mut self,
        agg: &RExpr,
        rows: &[RowData],
    ) -> Result<OwnedValue, S::Error> {
        let Some(p) = crate::query::agg_parts(agg) else {
            return Err(Error::Unsupported);
        };
        let mut arg_rows: AVec<AVec<OwnedValue>> = AVec::new();
        for r in rows {
            // The FILTER first: a row it rejects is not stepped,
            // and nothing else of it is evaluated.
            if let Some(f) = p.filter
                && crate::query::truthy(&self.eval_row_sub(f, r).await?) != Some(true)
            {
                continue;
            }
            let mut vs: AVec<OwnedValue> = AVec::new();
            for a in p.inputs() {
                vs.push(self.eval_row_sub(a, r).await?);
            }
            arg_rows.push(vs);
        }
        crate::agg::fold::<S::Error>(&p, arg_rows)
    }

    /// The representative row for a top-level query's group
    /// (v0.11 wrapper over `pick_rep`): when the lone min/max
    /// aggregate's argument contains a subquery, the extremum
    /// search must execute that subquery per row, which the
    /// synchronous `pick_rep` cannot do.
    #[cfg(feature = "v06")]
    async fn pick_rep_qp_async(
        &mut self,
        qp: &QueryPlan,
        grows: &[RowData],
        empty_rep: &RowData,
    ) -> Result<RowData, S::Error> {
        if grows.is_empty() {
            return Ok(empty_rep.clone());
        }
        if let Some(m) = rep_minmax(
            qp.items.as_slice(),
            qp.having.as_ref(),
            qp.order_by.as_slice(),
        ) && m.has_sub()
        {
            return self.extremum_rep(m, grows, empty_rep).await;
        }
        pick_rep::<S::Error>(&self.conn.env, qp, grows, empty_rep)
    }

    /// The `pick_rep_qp_async` mirror for a subquery's group.
    #[cfg(feature = "v06")]
    async fn pick_rep_sub_async(
        &mut self,
        plan: &crate::query::SubPlan,
        grows: &[RowData],
        empty_rep: &RowData,
    ) -> Result<RowData, S::Error> {
        if grows.is_empty() {
            return Ok(empty_rep.clone());
        }
        if let Some(m) = rep_minmax(
            plan.items.as_slice(),
            plan.having.as_ref(),
            plan.order_by.as_slice(),
        ) && m.has_sub()
        {
            return self.extremum_rep(m, grows, empty_rep).await;
        }
        pick_rep_sub::<S::Error>(&self.conn.env, plan, grows, empty_rep)
    }

    /// The extremum row of a group under the min/max aggregate
    /// `rep_minmax` chose, whose argument is evaluated per row
    /// through `eval_row_sub` (v0.11): the synchronous
    /// `pick_rep_from` rule (`extremum_step`), with per-row
    /// subquery execution.
    #[cfg(feature = "v06")]
    async fn extremum_rep(
        &mut self,
        m: MinMaxRep<'_>,
        grows: &[RowData],
        empty_rep: &RowData,
    ) -> Result<RowData, S::Error> {
        let Some(first) = grows.first() else {
            return Ok(empty_rep.clone());
        };
        let coll = crate::query::expr_coll(m.arg).unwrap_or_default();
        let mut best: Option<OwnedValue> = None;
        let mut rep = first;
        for r in grows {
            if let Some(f) = m.filter
                && crate::query::truthy(&self.eval_row_sub(f, r).await?) != Some(true)
            {
                continue;
            }
            let v = self.eval_row_sub(m.arg, r).await?;
            if extremum_step(m.kind, coll, &mut best, v) {
                rep = r;
            }
        }
        Ok(rep.clone())
    }

    /// Execute every current-scope subquery node of `e` for the
    /// outer row `outer`, in `collect_subs` traversal order.
    #[cfg(feature = "v06")]
    async fn compute_sub_values(
        &mut self,
        e: &RExpr,
        outer: &RowData,
    ) -> Result<AVec<crate::query::SubValue>, S::Error> {
        use crate::query::{SubKind, SubValue, collect_subs};
        let mut kinds: AVec<SubKind<'_>> = AVec::new();
        collect_subs(e, &mut kinds);
        let mut out: AVec<SubValue> = AVec::new();
        for k in kinds.iter() {
            out.push(self.sub_value_for(k, outer).await?);
        }
        Ok(out)
    }

    /// Execute the subquery nodes of `e` that lie OUTSIDE any
    /// aggregate's arguments, for the outer row `outer`, in
    /// `collect_subs_outside_agg` traversal order (v0.11).
    #[cfg(feature = "v06")]
    async fn compute_sub_values_outside_agg(
        &mut self,
        e: &RExpr,
        outer: &RowData,
    ) -> Result<AVec<crate::query::SubValue>, S::Error> {
        use crate::query::{SubKind, SubValue, collect_subs_outside_agg};
        let mut kinds: AVec<SubKind<'_>> = AVec::new();
        collect_subs_outside_agg(e, &mut kinds);
        let mut out: AVec<SubValue> = AVec::new();
        for k in kinds.iter() {
            out.push(self.sub_value_for(k, outer).await?);
        }
        Ok(out)
    }

    /// Compute one subquery node's value for `outer` (v0.13):
    /// an uncorrelated plan is executed once per statement
    /// execution and the result is cached under its plan node
    /// id — every later row (or group row, for aggregate
    /// arguments) reuses it. The first use computes against
    /// the live database at that moment, which for a statement
    /// that writes the subquery's own table is its pre-write
    /// snapshot, matching the pin's per-statement behaviour.
    /// Correlated plans always take the per-row path.
    #[cfg(feature = "v06")]
    async fn sub_value_for(
        &mut self,
        kind: &crate::query::SubKind<'_>,
        outer: &RowData,
    ) -> Result<crate::query::SubValue, S::Error> {
        use crate::query::SubKind;
        let plan = match kind {
            SubKind::Scalar(p) => *p,
            SubKind::Exists(p) => *p,
            SubKind::In(p) => *p,
        };
        if plan.uncorrelated && plan.id != 0 {
            // Keyed by node id and result column: the parts of a
            // multi-column scalar subquery share their node id.
            let key = (plan.id, plan.scalar_col);
            for (k, v) in self.hoist.iter() {
                if *k == key {
                    return Ok(v.clone());
                }
            }
            let v = self.exec_sub_kind_deferred(kind, outer).await?;
            self.hoist.push((key, v.clone()));
            return Ok(v);
        }
        self.exec_sub_kind_deferred(kind, outer).await
    }

    /// `exec_sub_kind` with an evaluation error (`Error::Schema`)
    /// kept as the node's value (`SubValue::Fail`, v0.16 review
    /// fix): it is raised only if evaluation reaches the node, as
    /// on the pin, which runs a subquery only when it is read
    /// (`coalesce(1, (SELECT abs(-9223372036854775808)))` is 1).
    /// I/O, corruption and resource errors still fail at once.
    #[cfg(feature = "v06")]
    async fn exec_sub_kind_deferred(
        &mut self,
        kind: &crate::query::SubKind<'_>,
        outer: &RowData,
    ) -> Result<crate::query::SubValue, S::Error> {
        match self.exec_sub_kind(kind, outer).await {
            Err(Error::Schema { kind }) => Ok(crate::query::SubValue::Fail(kind)),
            r => r,
        }
    }

    /// Execute one subquery node for one outer row and shape
    /// its `SubValue` (v0.6; factored out in v0.13).
    #[cfg(feature = "v06")]
    async fn exec_sub_kind(
        &mut self,
        kind: &crate::query::SubKind<'_>,
        outer: &RowData,
    ) -> Result<crate::query::SubValue, S::Error> {
        use crate::query::{SubKind, SubValue};
        Ok(match kind {
            SubKind::Scalar(plan) => {
                // Only the first row is read (the pin's implicit
                // LIMIT 1, v0.16 review fix: an endless recursive
                // CTE under a scalar subquery answers).
                let rows = self.exec_subplan_need(plan, outer, Some(1)).await?;
                SubValue::One(
                    match rows.as_slice().first() {
                        Some(r) => r
                            .as_slice()
                            .get(plan.scalar_col)
                            .cloned()
                            .unwrap_or(OwnedValue::Null),
                        None => OwnedValue::Null,
                    },
                    crate::query::result_affinity(plan),
                )
            }
            SubKind::Exists(plan) => {
                let rows = self.exec_subplan_need(plan, outer, Some(1)).await?;
                SubValue::One(
                    OwnedValue::Integer(if rows.is_empty() { 0 } else { 1 }),
                    None,
                )
            }
            SubKind::In(plan) => {
                let rows = self.exec_subplan(plan, outer).await?;
                // Row-major: every column of every row for a
                // row-value IN (v0.16), else the one column.
                let w = plan.width().max(1);
                let mut vs = AVec::new();
                for r in rows.iter() {
                    for c in 0..w {
                        vs.push(r.as_slice().get(c).cloned().unwrap_or(OwnedValue::Null));
                    }
                }
                SubValue::Many(vs, crate::query::result_affinity(plan))
            }
        })
    }

    /// Execute one planned subquery for one outer row (v0.6):
    /// scan its sources, nested-loop join them onto the outer
    /// row, group/project, then DISTINCT / ORDER BY / LIMIT.
    /// Returns the result rows' output values. The future is
    /// boxed: subquery execution is recursive (a subquery's
    /// expressions can contain further subqueries), and boxing
    /// this link keeps the async layout finite.
    #[cfg(feature = "v06")]
    fn exec_subplan<'x>(
        &'x mut self,
        plan: &'x crate::query::SubPlan,
        outer: &'x RowData,
    ) -> SubplanFuture<'x, S::Error> {
        self.exec_subplan_need(plan, outer, None)
    }

    /// `exec_subplan` for a derived table whose reader needs only
    /// its first `need` rows (v0.16 review fix; None: all of
    /// them). The pin runs such a FROM subquery as a coroutine
    /// (or flattens it) and stops at the reader's LIMIT, so its
    /// later groups are never finalized and their aggregates
    /// cannot error: `SELECT x FROM (SELECT k x, sum(v) FROM g
    /// GROUP BY k) LIMIT 1` answers although a later group's sum
    /// overflows.
    #[cfg(feature = "v06")]
    fn exec_subplan_need<'x>(
        &'x mut self,
        plan: &'x crate::query::SubPlan,
        outer: &'x RowData,
        need: Option<usize>,
    ) -> SubplanFuture<'x, S::Error> {
        self.conn.sub_execs += 1;
        alloc::boxed::Box::pin(self.exec_subplan_inner(plan, outer, need))
    }

    #[cfg(feature = "v06")]
    async fn exec_subplan_inner(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
        need: Option<usize>,
    ) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        #[cfg(feature = "v16")]
        if let Some(rec) = plan.rec {
            return self.exec_recursive(plan, rec, outer, need).await;
        }
        let plan = plan.clone();
        let window = self.limit_window(&plan.limit, &plan.offset).await?;
        // The rows its reader takes bound the rows it must make.
        let (skip, take) = window;
        let streamed = (skip, need.map_or(take, |n| take.min(n)));
        // Only a lone core streams its groups up to the LIMIT —
        // and not over window calls (v0.16, `window`), which read
        // every input row.
        #[cfg(feature = "v09")]
        let lone = plan.compound.is_empty();
        #[cfg(not(feature = "v09"))]
        let lone = true;
        #[cfg(feature = "window")]
        let lone = lone
            && crate::window::calls(plan.items.as_slice(), plan.order_by.as_slice()).is_empty();
        let cap = if lone {
            group_cap(
                plan.group_by.as_slice(),
                plan.order_by.as_slice(),
                plan.distinct,
                streamed,
            )
        } else {
            None
        };
        // A lone core reading one derived table passes the bound
        // on (`derived_need`).
        #[cfg(feature = "v09")]
        let src_need = if lone {
            derived_need(
                plan.sources.as_slice(),
                plan.filter.is_some() || !plan.group_by.is_empty() || plan.has_agg,
                !plan.order_by.is_empty() || plan.distinct,
                streamed,
            )
        } else {
            None
        };
        #[cfg(not(feature = "v09"))]
        let src_need = None;
        // v0.16 review fix: an endless recursive CTE read through
        // a WHERE or a join stops at the reader's bound (see
        // `run_query_sub`).
        #[cfg(feature = "v16")]
        let grow = lone
            && plan.values.is_empty()
            && streams_first(
                plan.sources.as_slice(),
                plan.group_by.is_empty()
                    && !plan.has_agg
                    && plan.order_by.is_empty()
                    && !plan.distinct,
            )
            && (plan.filter.is_some() || plan.sources.len() > 1)
            && streamed.1 != usize::MAX;
        #[cfg(not(feature = "v16"))]
        let grow = false;
        let target = streamed.0.saturating_add(streamed.1);
        let mut n = if grow { target.max(1) } else { 0 };
        let mut produced = loop {
            let (p, first) = self
                .exec_subplan_core_n(&plan, outer, cap, if grow { Some(n) } else { src_need })
                .await?;
            if !grow || p.len() >= target || first < n {
                break p;
            }
            n = n.saturating_mul(2);
        };
        // Compound finish (v0.9): combine the core rows with
        // each tail core's rows, left to right, then the
        // compound ORDER BY / LIMIT.
        #[cfg(feature = "v09")]
        if !plan.compound.is_empty() {
            let mut head: AVec<AVec<OwnedValue>> = AVec::new();
            for p in produced.iter() {
                head.push(p.out.clone());
            }
            let mut tails = AVec::new();
            let mut cores = AVec::new();
            cores.push(core_colls(plan.items.as_slice(), plan.values.as_slice()));
            for (op, tail) in plan.compound.iter() {
                let tp = self.exec_subplan_core(tail, outer, None, None).await?;
                let mut right: AVec<AVec<OwnedValue>> = AVec::new();
                for p in tp.iter() {
                    right.push(p.out.clone());
                }
                tails.push((*op, right));
                cores.push(core_colls(tail.items.as_slice(), tail.values.as_slice()));
            }
            let rows = if union_first_row(
                &plan.limit,
                &plan.offset,
                plan.comp_order.as_slice(),
                tails.as_slice(),
            ) {
                first_core_row(head, tails.as_slice())
            } else {
                run_compound(
                    head,
                    tails.as_slice(),
                    cores.as_slice(),
                    plan.comp_order.as_slice(),
                )
            };
            return Ok(apply_window(rows, window));
        }
        produced.as_mut_slice().sort_by(|x, y| {
            crate::query::order_keys_cmp(
                x.keys.as_slice(),
                y.keys.as_slice(),
                plan.order_by.iter().map(|(_, o)| *o),
            )
        });
        let mut rows: AVec<AVec<OwnedValue>> = AVec::new();
        for p in produced.iter().skip(window.0).take(window.1) {
            rows.push(p.out.clone());
        }
        Ok(rows)
    }

    /// Execute one subquery CORE (v0.6; v0.9 split): materialise
    /// sources (derived tables execute their subplan against
    /// the outer row), join onto the outer row, project, and
    /// apply the core's own DISTINCT. ORDER BY / LIMIT are the
    /// caller's job (they belong to the whole statement, or to
    /// the compound).
    #[cfg(feature = "v09")]
    async fn exec_subplan_core(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
        cap: Option<usize>,
        need: Option<usize>,
    ) -> Result<AVec<Produced>, S::Error> {
        Ok(self.exec_subplan_core_n(plan, outer, cap, need).await?.0)
    }

    /// `exec_subplan_core`, also returning how many rows its first
    /// source gave (v0.16 review fix: whether a bounded first
    /// derived table ran out).
    #[cfg(feature = "v06")]
    async fn exec_subplan_core_n(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
        cap: Option<usize>,
        need: Option<usize>,
    ) -> Result<(AVec<Produced>, usize), S::Error> {
        // A VALUES core (v0.9): the rows evaluate against the
        // outer row directly.
        #[cfg(feature = "v09")]
        if !plan.values.is_empty() {
            let mut out = AVec::new();
            for r in plan.values.iter() {
                #[cfg(feature = "window")]
                let rv = self.eval_values_row(r.as_slice(), outer).await?;
                #[cfg(not(feature = "window"))]
                let rv = {
                    let mut rv = AVec::new();
                    for e in r.iter() {
                        rv.push(self.eval_row_sub(e, outer).await?);
                    }
                    rv
                };
                out.push(Produced {
                    out: rv,
                    keys: AVec::new(),
                    rowid: None,
                });
            }
            let n = out.len();
            return Ok((out, n));
        }
        // Materialise the subquery's own sources (a derived
        // table's rows are its subplan's result for this outer
        // row; they carry no rowid).
        let src_rows = self
            .materialise_sources_sub(plan.sources.as_slice(), outer, need)
            .await?;
        let first = src_rows.as_slice().first().map_or(0, |r| r.len());
        let mut joined: AVec<RowData> = AVec::new();
        self.join_sources(
            plan.sources.as_slice(),
            plan.filter.as_ref(),
            src_rows.as_slice(),
            outer,
            &mut joined,
        )
        .await?;
        let mut produced = self
            .sub_project(plan, outer, joined.as_slice(), cap)
            .await?;
        if plan.distinct {
            produced = dedup_produced(
                produced,
                crate::query::exprs_colls(plan.items.as_slice()).as_slice(),
            );
        }
        Ok((produced, first))
    }

    /// The nested-loop join of `sources` onto `outer` (v0.6
    /// subquery joins, v0.7 UPDATE ... FROM, top-level queries
    /// with subqueries; one executor since v0.16): ON and the
    /// optional `filter` evaluate through `eval_row_sub`, so a
    /// nested subquery may be correlated to the combined row.
    /// LEFT / FULL sources NULL-pad an unmatched left
    /// combination in place. RIGHT / FULL sources (v0.16) then
    /// emit, after the whole main loop, each of their rows that
    /// no combination matched, NULL-padded on the left and
    /// joined onward through the later sources — earlier
    /// sources first, each in its own row order, exactly the
    /// pin's output order (its right-join loop runs once the
    /// left operand is exhausted).
    #[cfg(feature = "v06")]
    async fn join_sources(
        &mut self,
        sources: &[SourcePlan],
        filter: Option<&RExpr>,
        src_rows: &[AVec<(AVec<OwnedValue>, Option<i64>)>],
        outer: &RowData,
        out: &mut AVec<RowData>,
    ) -> Result<(), S::Error> {
        let mut matched = right_match_flags(sources, src_rows);
        self.join_step(
            sources,
            filter,
            src_rows,
            0,
            outer.clone(),
            &mut matched,
            out,
        )
        .await?;
        for (i, (sp, rows)) in sources.iter().zip(src_rows.iter()).enumerate() {
            if !sp.kind.pads_left() {
                continue;
            }
            for (ri, (vals, rowid)) in rows.iter().enumerate() {
                if row_matched(&matched, i, ri) {
                    continue;
                }
                let acc = left_padded(outer, sources, i, vals, *rowid);
                self.join_step(sources, filter, src_rows, i + 1, acc, &mut matched, out)
                    .await?;
            }
        }
        Ok(())
    }

    /// One level of `join_sources`' main loop: extend `acc`
    /// with source `idx`'s rows that pass its ON, recording the
    /// RIGHT / FULL matches in `matched`, and recurse; a
    /// complete row passes `filter` into `out`. Recursive, so
    /// the future is boxed.
    #[cfg(feature = "v06")]
    #[allow(clippy::too_many_arguments)]
    fn join_step<'x>(
        &'x mut self,
        sources: &'x [SourcePlan],
        filter: Option<&'x RExpr>,
        src_rows: &'x [AVec<(AVec<OwnedValue>, Option<i64>)>],
        idx: usize,
        acc: RowData,
        matched: &'x mut AVec<AVec<bool>>,
        out: &'x mut AVec<RowData>,
    ) -> JoinFuture<'x, S::Error> {
        alloc::boxed::Box::pin(async move {
            let (Some(sp), Some(rows)) = (sources.get(idx), src_rows.get(idx)) else {
                let keep = match filter {
                    None => true,
                    Some(f) => {
                        crate::query::truthy(&self.eval_row_sub(f, &acc).await?) == Some(true)
                    }
                };
                if keep {
                    out.push(acc);
                }
                return Ok(());
            };
            let mut any = false;
            for (ri, (vals, rowid)) in rows.iter().enumerate() {
                let mut acc2 = acc.clone();
                for v in vals.iter() {
                    acc2.vals.push(v.clone());
                }
                acc2.rowids.push(*rowid);
                let on_ok = match &sp.on {
                    None => true,
                    // A subquery in this ON was planned with its
                    // own slots after EVERY source of the FROM
                    // clause, but the row holds only sources
                    // 0..=idx here: evaluate it on a copy padded
                    // with the later sources as NULLs (an ON may
                    // not read them — see `plan_on`), so the
                    // subquery's rows land where it reads them.
                    // v0.16 regression: hipp read misaligned
                    // slots (wrong rows) for such an ON.
                    Some(on) if crate::query::has_sub(on) => {
                        let mut padded = acc2.clone();
                        pad_sources(&mut padded, sources.get(idx + 1..).unwrap_or(&[]));
                        crate::query::truthy(&self.eval_row_sub(on, &padded).await?) == Some(true)
                    }
                    Some(on) => {
                        crate::query::truthy(&self.eval_row_sub(on, &acc2).await?) == Some(true)
                    }
                };
                if on_ok {
                    any = true;
                    mark_matched(matched, idx, ri);
                    self.join_step(
                        sources,
                        filter,
                        src_rows,
                        idx + 1,
                        acc2,
                        &mut *matched,
                        &mut *out,
                    )
                    .await?;
                }
            }
            if sp.kind.pads_right() && !any {
                let mut acc2 = acc;
                pad_sources(&mut acc2, core::slice::from_ref(sp));
                self.join_step(
                    sources,
                    filter,
                    src_rows,
                    idx + 1,
                    acc2,
                    &mut *matched,
                    &mut *out,
                )
                .await?;
            }
            Ok(())
        })
    }

    /// Group / project for a top-level query whose expressions
    /// contain subqueries (v0.6): `project_rows` with per-row /
    /// per-group subquery execution.
    #[cfg(feature = "v06")]
    async fn project_qp_async(
        &mut self,
        qp: &QueryPlan,
        rows: &[RowData],
        cap: Option<usize>,
    ) -> Result<AVec<Produced>, S::Error> {
        let mut out = AVec::new();
        // Window calls (v0.16, `window`; see `project_rows`).
        #[cfg(feature = "window")]
        let wins = crate::window::calls(qp.items.as_slice(), qp.order_by.as_slice());
        #[cfg(feature = "window")]
        let cap = if wins.is_empty() { cap } else { None };
        #[cfg(feature = "window")]
        let mut pend: AVec<WinUnit> = AVec::new();
        if !qp.has_agg {
            #[cfg(feature = "window")]
            let mut row_at = 0usize;
            for r in rows {
                #[cfg(feature = "window")]
                if !wins.is_empty() {
                    let mut ins = AVec::new();
                    for w in wins.iter() {
                        let mut v = AVec::new();
                        for k in crate::window::children(w) {
                            v.push(self.eval_row_sub(k, r).await?);
                        }
                        ins.push(v);
                    }
                    pend.push(WinUnit {
                        at: row_at,
                        rep: None,
                        ins,
                    });
                    row_at += 1;
                    continue;
                }
                let mut vals = AVec::new();
                for it in qp.items.iter() {
                    vals.push(self.eval_row_sub(it, r).await?);
                }
                let mut keys = AVec::new();
                for (e, _) in qp.order_by.iter() {
                    keys.push(self.eval_row_sub(e, r).await?);
                }
                let rowid = if qp.sources.len() == 1 {
                    r.rowids.as_slice()[0]
                } else {
                    None
                };
                out.push(Produced {
                    out: vals,
                    keys,
                    rowid,
                });
            }
            #[cfg(feature = "window")]
            if !wins.is_empty() {
                return self
                    .win_project_async(
                        qp.items.as_slice(),
                        qp.order_by.as_slice(),
                        qp.sources.len() == 1,
                        wins.as_slice(),
                        pend,
                        rows,
                        &[],
                    )
                    .await;
            }
            return Ok(out);
        }
        // Grouping, mirroring project_rows but with per-row
        // subquery execution for the keys.
        let total_width: usize = qp.sources.iter().map(|s| s.ncols).sum();
        let empty_rep = RowData {
            vals: {
                let mut v = AVec::new();
                for _ in 0..total_width {
                    v.push(OwnedValue::Null);
                }
                v
            },
            rowids: {
                let mut v = AVec::new();
                for _ in qp.sources.iter() {
                    v.push(None);
                }
                v
            },
        };
        let mut groups: AVec<(AVec<OwnedValue>, AVec<RowData>)> = AVec::new();
        if qp.group_by.is_empty() {
            let mut all = AVec::new();
            for r in rows {
                all.push(r.clone());
            }
            groups.push((AVec::new(), all));
        } else {
            // v0.16: groups form and sort under each term's
            // collation (a NOCASE column groups 'a' with 'A').
            let gcolls = crate::query::exprs_colls(qp.group_by.as_slice());
            let gdesc = group_desc(
                qp.group_by.as_slice(),
                qp.items.as_slice(),
                qp.order_by.as_slice(),
            );
            for r in rows {
                let mut key = AVec::new();
                for g in qp.group_by.iter() {
                    key.push(self.eval_row_sub(g, r).await?);
                }
                let mut found = None;
                for (gi, (k, _)) in groups.iter().enumerate() {
                    if k.len() == key.len()
                        && k.iter()
                            .zip(key.iter())
                            .zip(gcolls.iter())
                            .all(|((a, b), c)| crate::query::values_equal_coll(a, b, *c))
                    {
                        found = Some(gi);
                        break;
                    }
                }
                match found {
                    Some(gi) => groups.as_mut_slice()[gi].1.push(r.clone()),
                    None => {
                        let mut g = AVec::new();
                        g.push(r.clone());
                        groups.push((key, g));
                    }
                }
            }
            groups.as_mut_slice().sort_by(|a, b| {
                for (((av, bv), c), d) in
                    a.0.iter()
                        .zip(b.0.iter())
                        .zip(gcolls.iter())
                        .zip(gdesc.iter())
                {
                    let o = crate::record::compare_coll(&av.as_value(), &bv.as_value(), *c);
                    let o = if *d { o.reverse() } else { o };
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        #[cfg(feature = "window")]
        let mut group_at = usize::MAX;
        for (_, grows) in groups.iter() {
            #[cfg(feature = "window")]
            {
                group_at = group_at.wrapping_add(1);
            }
            if cap.is_some_and(|c| out.len() >= c) {
                break;
            }
            let rep = self
                .pick_rep_qp_async(qp, grows.as_slice(), &empty_rep)
                .await?;
            if let Some(h) = &qp.having {
                let mut early = AVec::new();
                having_early(h, qp.group_by.as_slice(), &mut early);
                let mut pass = true;
                for c in early.iter() {
                    pass &= crate::query::truthy(
                        &self.eval_group_sub(c, grows.as_slice(), &rep).await?,
                    ) == Some(true);
                }
                if !pass {
                    continue;
                }
                for a in group_aggs(qp.items.as_slice(), qp.order_by.as_slice()).iter() {
                    self.eval_group_sub(a, grows.as_slice(), &rep).await?;
                }
                if crate::query::truthy(&self.eval_group_sub(h, grows.as_slice(), &rep).await?)
                    != Some(true)
                {
                    continue;
                }
            }
            #[cfg(feature = "window")]
            if !wins.is_empty() {
                let mut ins = AVec::new();
                for w in wins.iter() {
                    let mut v = AVec::new();
                    for k in crate::window::children(w) {
                        v.push(self.eval_group_sub(k, grows.as_slice(), &rep).await?);
                    }
                    ins.push(v);
                }
                pend.push(WinUnit {
                    at: group_at,
                    rep: Some(rep),
                    ins,
                });
                continue;
            }
            let mut vals = AVec::new();
            for it in qp.items.iter() {
                vals.push(self.eval_group_sub(it, grows.as_slice(), &rep).await?);
            }
            let mut keys = AVec::new();
            for (e, _) in qp.order_by.iter() {
                keys.push(self.eval_group_sub(e, grows.as_slice(), &rep).await?);
            }
            out.push(Produced {
                out: vals,
                keys,
                rowid: None,
            });
        }
        #[cfg(feature = "window")]
        if !wins.is_empty() {
            return self
                .win_project_async(
                    qp.items.as_slice(),
                    qp.order_by.as_slice(),
                    false,
                    wins.as_slice(),
                    pend,
                    rows,
                    groups.as_slice(),
                )
                .await;
        }
        Ok(out)
    }

    /// Group / project for a subquery (v0.6): `project_rows`
    /// over combined (outer ++ inner) rows; the empty-group
    /// representative keeps the outer row's values and pads the
    /// subquery's own columns with NULLs.
    #[cfg(feature = "v06")]
    async fn sub_project(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
        rows: &[RowData],
        cap: Option<usize>,
    ) -> Result<AVec<Produced>, S::Error> {
        let mut out = AVec::new();
        // Window calls (v0.16, `window`; see `project_rows`).
        #[cfg(feature = "window")]
        let wins = crate::window::calls(plan.items.as_slice(), plan.order_by.as_slice());
        #[cfg(feature = "window")]
        let cap = if wins.is_empty() { cap } else { None };
        #[cfg(feature = "window")]
        let mut pend: AVec<WinUnit> = AVec::new();
        if !plan.has_agg {
            #[cfg(feature = "window")]
            let mut row_at = 0usize;
            for r in rows {
                #[cfg(feature = "window")]
                if !wins.is_empty() {
                    let mut ins = AVec::new();
                    for w in wins.iter() {
                        let mut v = AVec::new();
                        for k in crate::window::children(w) {
                            v.push(self.eval_row_sub(k, r).await?);
                        }
                        ins.push(v);
                    }
                    pend.push(WinUnit {
                        at: row_at,
                        rep: None,
                        ins,
                    });
                    row_at += 1;
                    continue;
                }
                let mut vals = AVec::new();
                for it in plan.items.iter() {
                    vals.push(self.eval_row_sub(it, r).await?);
                }
                let mut keys = AVec::new();
                for (e, _) in plan.order_by.iter() {
                    keys.push(self.eval_row_sub(e, r).await?);
                }
                out.push(Produced {
                    out: vals,
                    keys,
                    rowid: None,
                });
            }
            #[cfg(feature = "window")]
            if !wins.is_empty() {
                return self
                    .win_project_async(
                        plan.items.as_slice(),
                        plan.order_by.as_slice(),
                        false,
                        wins.as_slice(),
                        pend,
                        rows,
                        &[],
                    )
                    .await;
            }
            return Ok(out);
        }
        // Grouping: keys evaluate per combined row.
        let inner_width: usize = plan.sources.iter().map(|s| s.ncols).sum();
        let empty_rep = RowData {
            vals: {
                let mut v = outer.vals.clone();
                for _ in 0..inner_width {
                    v.push(OwnedValue::Null);
                }
                v
            },
            rowids: {
                let mut v = outer.rowids.clone();
                for _ in plan.sources.iter() {
                    v.push(None);
                }
                v
            },
        };
        let mut groups: AVec<(AVec<OwnedValue>, AVec<RowData>)> = AVec::new();
        if plan.group_by.is_empty() {
            let mut all = AVec::new();
            for r in rows {
                all.push(r.clone());
            }
            groups.push((AVec::new(), all));
        } else {
            // v0.16: groups form and sort under each term's
            // collation (a NOCASE column groups 'a' with 'A').
            let gcolls = crate::query::exprs_colls(plan.group_by.as_slice());
            let gdesc = group_desc(
                plan.group_by.as_slice(),
                plan.items.as_slice(),
                plan.order_by.as_slice(),
            );
            for r in rows {
                let mut key = AVec::new();
                for g in plan.group_by.iter() {
                    key.push(self.eval_row_sub(g, r).await?);
                }
                let mut found = None;
                for (gi, (k, _)) in groups.iter().enumerate() {
                    if k.len() == key.len()
                        && k.iter()
                            .zip(key.iter())
                            .zip(gcolls.iter())
                            .all(|((a, b), c)| crate::query::values_equal_coll(a, b, *c))
                    {
                        found = Some(gi);
                        break;
                    }
                }
                match found {
                    Some(gi) => groups.as_mut_slice()[gi].1.push(r.clone()),
                    None => {
                        let mut g = AVec::new();
                        g.push(r.clone());
                        groups.push((key, g));
                    }
                }
            }
            groups.as_mut_slice().sort_by(|a, b| {
                for (((av, bv), c), d) in
                    a.0.iter()
                        .zip(b.0.iter())
                        .zip(gcolls.iter())
                        .zip(gdesc.iter())
                {
                    let o = crate::record::compare_coll(&av.as_value(), &bv.as_value(), *c);
                    let o = if *d { o.reverse() } else { o };
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        #[cfg(feature = "window")]
        let mut group_at = usize::MAX;
        for (_, grows) in groups.iter() {
            #[cfg(feature = "window")]
            {
                group_at = group_at.wrapping_add(1);
            }
            if cap.is_some_and(|c| out.len() >= c) {
                break;
            }
            let rep = self
                .pick_rep_sub_async(plan, grows.as_slice(), &empty_rep)
                .await?;
            if let Some(h) = &plan.having {
                let mut early = AVec::new();
                having_early(h, plan.group_by.as_slice(), &mut early);
                let mut pass = true;
                for c in early.iter() {
                    pass &= crate::query::truthy(
                        &self.eval_group_sub(c, grows.as_slice(), &rep).await?,
                    ) == Some(true);
                }
                if !pass {
                    continue;
                }
                for a in group_aggs(plan.items.as_slice(), plan.order_by.as_slice()).iter() {
                    self.eval_group_sub(a, grows.as_slice(), &rep).await?;
                }
                if crate::query::truthy(&self.eval_group_sub(h, grows.as_slice(), &rep).await?)
                    != Some(true)
                {
                    continue;
                }
            }
            #[cfg(feature = "window")]
            if !wins.is_empty() {
                let mut ins = AVec::new();
                for w in wins.iter() {
                    let mut v = AVec::new();
                    for k in crate::window::children(w) {
                        v.push(self.eval_group_sub(k, grows.as_slice(), &rep).await?);
                    }
                    ins.push(v);
                }
                pend.push(WinUnit {
                    at: group_at,
                    rep: Some(rep),
                    ins,
                });
                continue;
            }
            let mut vals = AVec::new();
            for it in plan.items.iter() {
                vals.push(self.eval_group_sub(it, grows.as_slice(), &rep).await?);
            }
            let mut keys = AVec::new();
            for (e, _) in plan.order_by.iter() {
                keys.push(self.eval_group_sub(e, grows.as_slice(), &rep).await?);
            }
            out.push(Produced {
                out: vals,
                keys,
                rowid: None,
            });
        }
        #[cfg(feature = "window")]
        if !wins.is_empty() {
            return self
                .win_project_async(
                    plan.items.as_slice(),
                    plan.order_by.as_slice(),
                    false,
                    wins.as_slice(),
                    pend,
                    rows,
                    groups.as_slice(),
                )
                .await;
        }
        Ok(out)
    }

    /// The window path of `project_qp_async` / `sub_project`
    /// (v0.16, `window`; see `win_project`): the projection
    /// evaluates through `eval_row_sub` / `eval_group_sub`.
    #[cfg(feature = "window")]
    #[allow(clippy::too_many_arguments)]
    async fn win_project_async(
        &mut self,
        items: &[RExpr],
        order_by: &[(RExpr, crate::sql::SortOrder)],
        single: bool,
        wins: &[&RExpr],
        mut units: AVec<WinUnit>,
        rows: &[RowData],
        groups: &[(AVec<OwnedValue>, AVec<RowData>)],
    ) -> Result<AVec<Produced>, S::Error> {
        let (order, vals, ti, to) = win_results::<S::Error>(wins, &mut units, items, order_by)?;
        let empty = RowData::empty();
        let mut out = AVec::with_capacity(order.len());
        for &u in order.iter() {
            let (Some(unit), Some(v)) = (units.as_slice().get(u), vals.as_slice().get(u)) else {
                continue;
            };
            let mut vs = AVec::new();
            let mut keys = AVec::new();
            let mut rowid = None;
            for (k, t) in ti.iter().chain(to.iter()).enumerate() {
                let mut e = t.clone();
                crate::window::fill(&mut e, v.as_slice());
                let x = match &unit.rep {
                    None => {
                        let r = rows.get(unit.at).unwrap_or(&empty);
                        if single {
                            rowid = r.rowids.as_slice().first().copied().flatten();
                        }
                        self.eval_row_sub(&e, r).await?
                    }
                    Some(rep) => {
                        let g = groups.get(unit.at).map(|g| g.1.as_slice()).unwrap_or(&[]);
                        self.eval_group_sub(&e, g, rep).await?
                    }
                };
                if k < ti.len() {
                    vs.push(x);
                } else {
                    keys.push(x);
                }
            }
            out.push(Produced {
                out: vs,
                keys,
                rowid,
            });
        }
        Ok(out)
    }

    // ---------------- v0.4 DDL ----------------

    #[cfg(feature = "v04")]
    async fn step_alter(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_alter().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    /// `ALTER TABLE ... ADD COLUMN` (v0.4): rewrite the table's
    /// schema row with the plan-time-computed SQL text, keeping
    /// the schema rowid (creation order is schema order). The
    /// table's records are untouched — short records already
    /// materialise the new column's default on read (§5.5).
    #[cfg(feature = "v04")]
    async fn exec_alter(&mut self) -> Result<(), S::Error> {
        let Plan::AlterAdd {
            root,
            table,
            new_sql,
            verify,
            empty_only,
        } = &self.plan
        else {
            unreachable!()
        };
        let (root, table, new_sql, verify, empty_only) = (
            *root,
            table.clone(),
            new_sql.clone(),
            verify.clone(),
            *empty_only,
        );
        if let Some(kind) = empty_only {
            let mut cursor = ScanCursor::new(root);
            let mut buf = AVec::new();
            if cursor
                .next_row_into(&mut self.conn.pager, &mut buf)
                .await?
                .is_some()
            {
                return Err(Error::Schema { kind });
            }
        }
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        let mut found = None;
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
            if values.len() >= 2
                && matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"table"))
                && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, table.as_slice()))
            {
                found = Some(rowid);
                break;
            }
        }
        let Some(srowid) = found else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        };
        let mut vals: AVec<Value<'_>> = AVec::new();
        vals.push(Value::Text(b"table"));
        vals.push(Value::Text(table.as_slice()));
        vals.push(Value::Text(table.as_slice()));
        vals.push(Value::Integer(i64::from(root)));
        vals.push(Value::Text(new_sql.as_slice()));
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        btree::btree_delete(&mut self.conn.pager, 1, srowid).await?;
        btree::btree_insert(&mut self.conn.pager, 1, srowid, rec.as_slice()).await?;
        // v0.16: verify existing rows under the new definition
        // (NOT NULL, STRICT, CHECK — ABORT semantics, as the
        // pin's post-ADD check raises); any failure rolls the
        // whole ALTER back (step_alter).
        if let Some(v) = verify {
            let gcols = v.rules.gcols.as_ref();
            let ncols = v.defaults.len();
            let mut cursor = ScanCursor::new(root);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let mut row = crate::rules::decode_rec::<S::Error>(
                    buf.as_slice(),
                    ncols,
                    v.defaults.as_slice(),
                    gcols.map(|g| g.virt.as_slice()),
                )?;
                v.rules.resolve::<S::Error>(
                    row.as_mut_slice(),
                    rowid,
                    v.not_null.as_slice(),
                    v.defaults.as_slice(),
                    v.affs.as_slice(),
                    v.rowid_alias,
                    ConflictAction::Abort,
                    None,
                    Some(&self.conn.env),
                )?;
            }
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    #[cfg(feature = "v04")]
    async fn step_create_view(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_create_view().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    /// `CREATE VIEW` (v0.4): a schema row with type `view`, root
    /// page 0, and the statement text as issued. Inert — nothing
    /// else happens, ever (SPEC §2).
    #[cfg(feature = "v04")]
    async fn exec_create_view(&mut self) -> Result<(), S::Error> {
        let Ast::CreateView {
            name,
            if_not_exists,
            ..
        } = &self.ast
        else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        // One namespace for tables, views and indexes; IF NOT EXISTS
        // excuses a table or view of the name, never an index (the
        // pin's "there is already an index named i", v0.16 review
        // fix).
        match self
            .conn
            .catalog
            .taken(&self.conn.catalog_arena, name_b.as_slice())
        {
            Some(true) => {
                return Err(Error::Schema {
                    kind: SchemaKind::DuplicateIndex,
                });
            }
            Some(false) if *if_not_exists => {
                self.phase = Phase::Done;
                return Ok(());
            }
            Some(false) => {
                return Err(Error::Schema {
                    kind: SchemaKind::DuplicateView,
                });
            }
            None => {}
        }
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text::<S::Error>(raw, &self.arena)?;
        self.insert_schema_row(
            b"view",
            name_b.as_slice(),
            name_b.as_slice(),
            0,
            Some(sql_text.as_slice()),
        )
        .await?;
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    #[cfg(feature = "v04")]
    async fn step_drop_view(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_drop_view().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "v04")]
    async fn exec_drop_view(&mut self) -> Result<(), S::Error> {
        let Plan::DropView { exists } = &self.plan else {
            unreachable!()
        };
        if !*exists {
            return Ok(());
        }
        let Ast::DropView { name, .. } = &self.ast else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        let mut found = None;
        // Its triggers go with it (v0.16, every profile).
        let mut trigs = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
            if values.len() >= 3
                && matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"trigger"))
                && matches!(&values.as_slice()[2], Value::Text(n) if eq_ci(n, name_b.as_slice()))
            {
                trigs.push(rowid);
            }
            if values.len() >= 2
                && found.is_none()
                && matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"view"))
                && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, name_b.as_slice()))
            {
                found = Some(rowid);
            }
        }
        let Some(srowid) = found else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownView,
            });
        };
        btree::btree_delete(&mut self.conn.pager, 1, srowid).await?;
        for r in trigs.iter() {
            btree::btree_delete(&mut self.conn.pager, 1, *r).await?;
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// Number of result columns (0 for non-SELECT statements
    /// without a RETURNING clause). With feature `params`, a
    /// statement whose planning was deferred to its first step
    /// (see `Connection::prepare`) reports 0 until it is
    /// stepped.
    pub fn column_count(&self) -> usize {
        match &self.plan {
            Plan::Scan { out, .. } => out.len(),
            #[cfg(feature = "v04")]
            Plan::Query(q) => q.width,
            #[cfg(feature = "v16")]
            Plan::Pragma(p) => p.width(),
            #[cfg(feature = "v16")]
            Plan::Temp { width, .. } => *width,
            #[cfg(feature = "v07")]
            Plan::Insert { returning, .. } => returning.len(),
            #[cfg(all(feature = "v05", feature = "v07"))]
            Plan::Update { returning, .. } => returning.len(),
            #[cfg(all(feature = "v05", feature = "v07"))]
            Plan::Delete { returning, .. } => returning.len(),
            #[cfg(feature = "triggers")]
            Plan::Instead(ip) => ip.returning_width(),
            _ => 0,
        }
    }

    /// The current row's value at `i` (valid until the next `step`).
    pub fn column_value(&self, i: usize) -> Option<Value<'_>> {
        self.current.as_slice().get(i).map(|v| v.as_value())
    }

    pub fn column_is_null(&self, i: usize) -> bool {
        matches!(
            self.current.as_slice().get(i),
            Some(OwnedValue::Null) | None
        )
    }

    pub fn column_i64(&self, i: usize) -> Option<i64> {
        match self.current.as_slice().get(i) {
            Some(OwnedValue::Integer(v)) => Some(*v),
            Some(OwnedValue::Real(f)) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn column_f64(&self, i: usize) -> Option<f64> {
        match self.current.as_slice().get(i) {
            Some(OwnedValue::Real(f)) => Some(*f),
            Some(OwnedValue::Integer(v)) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn column_bytes(&self, i: usize) -> Option<&[u8]> {
        match self.current.as_slice().get(i) {
            Some(OwnedValue::Text(b)) | Some(OwnedValue::Blob(b)) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Rowid of the current scan row.
    pub fn rowid(&self) -> Option<i64> {
        self.current_rowid
    }

    /// Statement arena high-water mark (§7.5.3 stats).
    pub fn arena_high_water(&self) -> usize {
        self.arena.high_water()
    }
}

/// A boxed subquery-execution future (v0.6).
#[cfg(feature = "v06")]
type SubplanFuture<'x, E> = core::pin::Pin<
    alloc::boxed::Box<dyn core::future::Future<Output = Result<AVec<AVec<OwnedValue>>, E>> + 'x>,
>;

/// A boxed recursive-join future (v0.6).
#[cfg(feature = "v06")]
type JoinFuture<'x, E> =
    core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = Result<(), E>> + 'x>>;

/// The result-column name of one SELECT item (v0.9): the
/// alias when written, else the column name for a direct
/// column reference, else the expression's source text (the
/// pin's naming rules, header-verified).
#[cfg(feature = "v04")]
fn item_name(
    arena: &Arena,
    e: &Expr,
    alias: &Option<crate::arena::ABytes>,
    text: &crate::arena::ABytes,
) -> AVec<u8> {
    let h = match (alias, e) {
        (Some(a), _) => *a,
        (None, Expr::Column { name, .. }) => *name,
        _ => *text,
    };
    copy_vec(handle_bytes(arena, h))
}

/// A VALUES core's result column name (v0.9): `column1`,
/// `column2`, ... (pin-verified via the CLI header).
#[cfg(feature = "v04")]
#[cfg_attr(not(feature = "v09"), allow(dead_code))]
fn values_col_name(i: usize) -> AVec<u8> {
    let mut v = AVec::new();
    v.extend_from_slice(b"column");
    crate::record::render_int((i + 1) as i64, &mut v);
    v
}

/// Resolve a compound SELECT's ORDER BY terms (v0.9) to
/// output-column indices. The pin's rule
/// (`resolveCompoundOrderBy`): a term is an output ordinal, or
/// a (possibly qualified) column name that some constituent
/// core matches — the head first, then each tail (see
/// `CompKeys::hit`); anything else ("ORDER BY a+0", a
/// constant, a name no core matches) fails prepare with
/// "ORDER BY term does not match any column in the result
/// set", mirrored here as `SchemaKind::UnknownColumn`.
#[cfg(feature = "v09")]
fn resolve_compound_order<E>(
    order_by: &[(Expr, crate::sql::SortOrder)],
    arena: &Arena,
    head: &CompKeys,
    tails: &[CompKeys],
    width: usize,
) -> Result<AVec<(usize, crate::sql::SortOrder)>, E> {
    let mut out = AVec::new();
    for (e, desc) in order_by.iter() {
        // v0.16: `<term> COLLATE name` sorts the matched column
        // by that collation; any other term by the compound
        // column's own (the leftmost core's that has one).
        #[allow(unused_mut)]
        let (mut e, mut explicit) = (e, None);
        #[cfg(feature = "v16")]
        while let Expr::Collate(inner, c) = e {
            if explicit.is_none() {
                explicit = Some(*c);
            }
            e = inner;
        }
        let idx = match e {
            Expr::Literal(Literal::Integer(n)) if *n >= 1 && (*n as usize) <= width => {
                Some(*n as usize - 1)
            }
            Expr::Column { table, name } => {
                let nb = handle_bytes(arena, *name);
                let qb = table.map(|h| handle_bytes(arena, h));
                core::iter::once(head)
                    .chain(tails.iter())
                    .find_map(|k| k.hit(qb, nb))
            }
            _ => None,
        };
        match idx {
            Some(i) => {
                let mut o = *desc;
                o.coll = explicit.unwrap_or_else(|| {
                    compound_col_coll(
                        core::iter::once(head).chain(tails.iter()).map(|k| &k.colls),
                        i,
                    )
                });
                out.push((i, o));
            }
            None => {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownColumn,
                });
            }
        }
    }
    Ok(out)
}

/// A core's per-column collations (v0.16): each result item's
/// (`query::expr_coll`). A VALUES core follows the pin's
/// `sqlite3MultiValues`: several constant rows whose first row
/// has no affinity become a co-routine read through a FROM
/// subquery, whose columns are columns (the first row's
/// collation, else BINARY — so `VALUES ('X'), ('x') UNION
/// SELECT nocase_col` deduplicates by BINARY); otherwise the
/// rows are a UNION ALL chain of one-row cores, and the first
/// row with a collation lends it.
#[cfg(feature = "v04")]
fn core_colls(items: &[RExpr], values: &[AVec<RExpr>]) -> AVec<Option<crate::record::Coll>> {
    use crate::query::{expr_coll, is_constant, item_result_affinity};
    let mut out = AVec::new();
    let Some(first) = values.first() else {
        for e in items {
            out.push(expr_coll(e));
        }
        return out;
    };
    let coroutine = values.len() > 1
        && values.iter().all(|r| r.iter().all(is_constant))
        && first.iter().all(|e| item_result_affinity(e).is_none());
    for (i, e) in first.iter().enumerate() {
        out.push(if coroutine {
            Some(expr_coll(e).unwrap_or_default())
        } else {
            values
                .iter()
                .find_map(|r| r.as_slice().get(i).and_then(expr_coll))
        });
    }
    out
}

/// A compound result column's collation (v0.16, the pin's
/// `multiSelectCollSeq`): the leftmost core whose item `i` has
/// one decides; BINARY when none does.
#[cfg(feature = "v09")]
fn compound_col_coll<'x>(
    cores: impl Iterator<Item = &'x AVec<Option<crate::record::Coll>>>,
    i: usize,
) -> crate::record::Coll {
    for c in cores {
        if let Some(Some(x)) = c.as_slice().get(i) {
            return *x;
        }
    }
    crate::record::Coll::Binary
}

/// Row equality under per-column collations, NULLs equal
/// (v0.16; v0.9 compared raw): the pin applies NO affinity in
/// compound operations or DISTINCT (`SELECT 1 UNION SELECT
/// '1'` keeps both rows).
#[cfg(feature = "v04")]
fn compound_rows_equal(a: &[OwnedValue], b: &[OwnedValue], colls: &[crate::record::Coll]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).enumerate().all(|(i, (x, y))| {
            crate::query::values_equal_coll(x, y, colls.get(i).copied().unwrap_or_default())
        })
}

/// Remove duplicate rows, keeping first occurrences (v0.9;
/// under the columns' collations since v0.16).
#[cfg(feature = "v09")]
fn dedup_compound_rows(
    rows: AVec<AVec<OwnedValue>>,
    colls: &[crate::record::Coll],
) -> AVec<AVec<OwnedValue>> {
    let mut kept: AVec<AVec<OwnedValue>> = AVec::new();
    for r in rows.iter() {
        if !kept
            .iter()
            .any(|k| compound_rows_equal(k.as_slice(), r.as_slice(), colls))
        {
            kept.push(r.clone());
        }
    }
    kept
}

/// Compare two compound rows ascending by every column under
/// the merge key's collations (v0.16).
#[cfg(feature = "v09")]
fn compound_key_cmp(
    a: &[OwnedValue],
    b: &[OwnedValue],
    colls: &[crate::record::Coll],
) -> core::cmp::Ordering {
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        let c = crate::record::compare_coll(
            &x.as_value(),
            &y.as_value(),
            colls.get(i).copied().unwrap_or_default(),
        );
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    core::cmp::Ordering::Equal
}

/// One compound operator as the pin's merge (v0.16, its
/// `multiSelectOrderBy`, which 3.5x uses for every UNION /
/// INTERSECT / EXCEPT): both sides stably sorted by the merge
/// key `key`, then merged — on equal keys UNION and EXCEPT
/// advance the left side silently (so the RIGHT row survives a
/// UNION: `SELECT 1 UNION SELECT 1.0` is 1.0), INTERSECT emits
/// the left row, UNION ALL emits left before right. Every
/// emitted row of a non-ALL operator is dropped when it equals
/// the previous emitted row under `dup` (the operator's own
/// column collations, which can differ from a key pushed down
/// from an enclosing operator).
#[cfg(feature = "v09")]
fn merge_compound(
    op: crate::sql::CompoundOp,
    mut left: AVec<AVec<OwnedValue>>,
    mut right: AVec<AVec<OwnedValue>>,
    key: &[crate::record::Coll],
    dup: &[crate::record::Coll],
) -> AVec<AVec<OwnedValue>> {
    use crate::sql::CompoundOp::{Except, Intersect, Union, UnionAll};
    use core::cmp::Ordering::{Equal, Greater, Less};
    left.as_mut_slice()
        .sort_by(|a, b| compound_key_cmp(a.as_slice(), b.as_slice(), key));
    right
        .as_mut_slice()
        .sort_by(|a, b| compound_key_cmp(a.as_slice(), b.as_slice(), key));
    let (l, r) = (left.as_slice(), right.as_slice());
    let mut out: AVec<AVec<OwnedValue>> = AVec::new();
    let emit = |out: &mut AVec<AVec<OwnedValue>>, row: &AVec<OwnedValue>| {
        if op != UnionAll
            && out
                .as_slice()
                .last()
                .is_some_and(|p| compound_rows_equal(p.as_slice(), row.as_slice(), dup))
        {
            return;
        }
        out.push(row.clone());
    };
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        match (l.get(i), r.get(j)) {
            (Some(a), Some(b)) => match compound_key_cmp(a.as_slice(), b.as_slice(), key) {
                Less => {
                    if op != Intersect {
                        emit(&mut out, a);
                    }
                    i += 1;
                }
                Equal => {
                    if matches!(op, UnionAll | Intersect) {
                        emit(&mut out, a);
                    }
                    i += 1;
                }
                Greater => {
                    if matches!(op, Union | UnionAll) {
                        emit(&mut out, b);
                    }
                    j += 1;
                }
            },
            (Some(a), None) => {
                if op == Intersect {
                    break;
                }
                emit(&mut out, a);
                i += 1;
            }
            (None, Some(b)) => {
                if matches!(op, Intersect | Except) {
                    break;
                }
                emit(&mut out, b);
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

/// Evaluate a compound SELECT over its cores' rows (v0.16; the
/// pin's algorithm, which collations make observable). `cores`
/// holds each core's per-column collations (head first, see
/// `core_colls`); `order` is the compound ORDER BY with each
/// term's collation resolved.
///
/// Operators up to the last non-ALL one — or every operator,
/// when an ORDER BY whose collations agree with the columns'
/// is merged straight in — run as `merge_compound` with one
/// merge key: the column collations over the cores up to that
/// point (the key the pin pushes down). Trailing UNION ALLs
/// past it append in order. An ORDER BY naming a collation
/// other than its column's makes the pin evaluate the compound
/// without it and sort the result afterwards; the final stable
/// sort by `order` below covers both shapes.
#[cfg(feature = "v09")]
fn run_compound(
    head: AVec<AVec<OwnedValue>>,
    tails: &[(crate::sql::CompoundOp, AVec<AVec<OwnedValue>>)],
    cores: &[AVec<Option<crate::record::Coll>>],
    order: &[(usize, crate::sql::SortOrder)],
) -> AVec<AVec<OwnedValue>> {
    use crate::sql::CompoundOp::UnionAll;
    let width = cores.first().map_or(0, |c| c.len());
    let prefix = |k: usize| -> AVec<crate::record::Coll> {
        let mut v = AVec::new();
        for i in 0..width {
            v.push(compound_col_coll(cores.iter().take(k + 1), i));
        }
        v
    };
    let full = prefix(tails.len());
    let direct = !order.is_empty()
        && order
            .iter()
            .all(|(i, o)| full.as_slice().get(*i).is_some_and(|c| *c == o.coll));
    // The last operator that merges (index into `tails`).
    let last = if direct {
        tails.len().checked_sub(1)
    } else {
        tails.iter().rposition(|(op, _)| *op != UnionAll)
    };
    let key = last.map(|h| prefix(h + 1)).unwrap_or_default();
    let mut rows = head;
    for (k, (op, right)) in tails.iter().enumerate() {
        if last.is_some_and(|h| k <= h) {
            rows = merge_compound(
                *op,
                rows,
                right.clone(),
                key.as_slice(),
                prefix(k + 1).as_slice(),
            );
        } else {
            for r in right.iter() {
                rows.push(r.clone());
            }
        }
    }
    rows.as_mut_slice()
        .sort_by(|a, b| compound_order_cmp(a, b, order));
    rows
}

/// The pin's UNION-with-LIMIT-1 shortcut (3.54.0: "UNION queries
/// with a LIMIT of exactly 1 are now handled faster"; ungated, a
/// silent wrong answer on the claimed v0.9 compound path): a
/// compound whose every operator is UNION, with no ORDER BY, no
/// OFFSET and a LIMIT that is the integer literal 1 — written, or
/// the implicit one of a scalar / EXISTS subquery without its own
/// LIMIT (`query::first_row_limit`) — yields its first row in
/// left-to-right core order, not the first of the sorted,
/// deduplicated result. CI evidence: `LIMIT (SELECT 2 UNION SELECT
/// 1)` is 2 on the pin. Inferred (no CI evidence yet): the written
/// `LIMIT 1` case, and that mixed operator chains, `LIMIT +1`,
/// bound limits and a subquery's own `LIMIT X` (rewritten to
/// `X <> 0`) keep the sorted answer.
#[cfg(feature = "v09")]
fn union_first_row(
    limit: &Option<RExpr>,
    offset: &Option<RExpr>,
    order: &[(usize, crate::sql::SortOrder)],
    tails: &[(crate::sql::CompoundOp, AVec<AVec<OwnedValue>>)],
) -> bool {
    matches!(limit, Some(RExpr::Literal(OwnedValue::Integer(1))))
        && offset.is_none()
        && order.is_empty()
        && tails
            .iter()
            .all(|(op, _)| *op == crate::sql::CompoundOp::Union)
}

/// The first row of the first core that has one (see
/// `union_first_row`).
#[cfg(feature = "v09")]
fn first_core_row(
    head: AVec<AVec<OwnedValue>>,
    tails: &[(crate::sql::CompoundOp, AVec<AVec<OwnedValue>>)],
) -> AVec<AVec<OwnedValue>> {
    let mut out = AVec::new();
    let first = head
        .as_slice()
        .first()
        .or_else(|| tails.iter().find_map(|(_, rows)| rows.as_slice().first()));
    if let Some(r) = first {
        out.push(r.clone());
    }
    out
}

/// A row window `(skip, take)` from `limit_window` over
/// finished rows (v0.9 extraction of the subquery finisher's
/// tail; v0.16 takes the evaluated window).
#[cfg(feature = "v09")]
fn apply_window(
    rows: AVec<AVec<OwnedValue>>,
    (skip, take): (usize, usize),
) -> AVec<AVec<OwnedValue>> {
    let mut out: AVec<AVec<OwnedValue>> = AVec::new();
    for r in rows.iter().skip(skip).take(take) {
        out.push(r.clone());
    }
    out
}

/// Compare two compound result rows under the compound ORDER
/// BY (v0.9; NULLS FIRST / LAST since v0.16).
#[cfg(feature = "v09")]
fn compound_order_cmp(
    a: &AVec<OwnedValue>,
    b: &AVec<OwnedValue>,
    order: &[(usize, crate::sql::SortOrder)],
) -> core::cmp::Ordering {
    for (idx, o) in order.iter() {
        if let (Some(x), Some(y)) = (a.as_slice().get(*idx), b.as_slice().get(*idx)) {
            let c = crate::query::order_cmp(x, y, *o);
            if c != core::cmp::Ordering::Equal {
                return c;
            }
        }
    }
    core::cmp::Ordering::Equal
}

/// The HAVING conjuncts the pin moves into WHERE (v0.16,
/// ungated; its `havingToWhere`, run only for a GROUP BY query):
/// AND terms that pass `sqlite3ExprIsConstantOrGroupBy`
/// (`const_or_group`) and are not the literal 0 / FALSE
/// (`ExprAlwaysFalse`). A group failing one never forms there, so
/// none of its aggregates is finalized — `sum()` cannot raise
/// "integer overflow" for it. hipp evaluates them first, against
/// the group's representative row, and skips the group. Review
/// fix: any term without aggregates or subqueries used to move,
/// so `HAVING random() = 5`, `HAVING 0`, a bare column, a NOCASE
/// group column and a query without GROUP BY skipped errors the
/// pin raises.
#[cfg(feature = "v04")]
fn having_early<'x>(h: &'x RExpr, group_by: &[RExpr], out: &mut AVec<&'x RExpr>) {
    if group_by.is_empty() || always_false(h) {
        return;
    }
    match h {
        RExpr::Binary(crate::sql::BinOp::And, a, b) => {
            having_early(a, group_by, out);
            having_early(b, group_by, out);
        }
        e if const_or_group(e, group_by) => out.push(e),
        _ => {}
    }
}

/// The pin's `ExprAlwaysFalse` after its parser's AND folding
/// (v0.16): the literal 0 (or FALSE), or an AND with such an
/// operand and no function call on either side, which
/// `sqlite3ExprAnd` turns into the literal 0 — so `HAVING k = 1
/// AND 0` moves nothing while `HAVING abs(k) = 1 AND 0` moves
/// `abs(k) = 1`. hipp's parser folds FALSE, `-0` and `+0` into
/// the literal 0, where the pin folds no AND over FALSE and
/// keeps a (movable) unary operator (STATUS.md gap).
#[cfg(feature = "v04")]
fn always_false(e: &RExpr) -> bool {
    match e {
        RExpr::Literal(OwnedValue::Integer(0)) => true,
        RExpr::Binary(crate::sql::BinOp::And, a, b) => {
            (always_false(a) || always_false(b)) && !has_call(a) && !has_call(b)
        }
        _ => false,
    }
}

/// Whether `e` holds a function call outside its subqueries
/// (v0.16): the pin's `EP_HasFunc`, which LIKE / GLOB carry too.
#[cfg(feature = "v04")]
fn has_call(e: &RExpr) -> bool {
    match e {
        RExpr::Scalar(..) | RExpr::Agg { .. } | RExpr::Like(..) | RExpr::Win(..) => true,
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => false,
        RExpr::InSub(x, ..)
        | RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => has_call(x),
        RExpr::Binary(_, a, b) => has_call(a) || has_call(b),
        RExpr::InList(x, l, ..) => has_call(x) || l.iter().any(has_call),
        RExpr::Between(x, a, b, _) => has_call(x) || has_call(a) || has_call(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_some_and(has_call)
                || whens.iter().any(|(w, t)| has_call(w) || has_call(t))
                || otherwise.as_deref().is_some_and(has_call)
        }
    }
}

/// The pin's `sqlite3ExprIsConstantOrGroupBy` (v0.16): every
/// part of `e` is a literal, an operator or deterministic
/// function over such parts, or a GROUP BY term with BINARY
/// collation (`group_match`). Columns elsewhere, subqueries,
/// aggregates and the non-constant functions (`random()`,
/// `changes()`, ...) fail.
#[cfg(feature = "v04")]
fn const_or_group(e: &RExpr, group_by: &[RExpr]) -> bool {
    if group_by.iter().any(|g| {
        group_match(e, g)
            && crate::query::expr_coll(g).unwrap_or_default() == crate::record::Coll::Binary
    }) {
        return true;
    }
    let f = |x: &RExpr| const_or_group(x, group_by);
    match e {
        RExpr::Literal(_) | RExpr::AffLiteral(..) => true,
        RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_)
        | RExpr::InSub(..)
        | RExpr::Agg { .. }
        | RExpr::Win(..) => false,
        RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => f(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) && f(b),
        RExpr::InList(x, l, ..) => f(x) && l.iter().all(f),
        RExpr::Between(x, a, b, _) => f(x) && f(a) && f(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(f)
                && whens.iter().all(|(w, t)| f(w) && f(t))
                && otherwise.as_deref().is_none_or(f)
        }
        RExpr::Scalar(g, args) => {
            // sqlite_version() is a constant function there.
            #[cfg(feature = "v16")]
            let varies = g.nondeterministic() && *g != crate::query::ScalarFn::SqliteVersion;
            #[cfg(not(feature = "v16"))]
            let varies = {
                let _ = g;
                false
            };
            !varies && args.iter().all(f)
        }
    }
}

/// `sqlite3ExprCompare(a, b) < 2` (v0.16): the same expression,
/// or one that differs only by a COLLATE on top of either side.
#[cfg(feature = "v04")]
fn group_match(a: &RExpr, b: &RExpr) -> bool {
    if crate::query::same_expr(a, b) {
        return true;
    }
    match (a, b) {
        (RExpr::Collate(..), RExpr::Collate(..)) => false,
        (RExpr::Collate(x, _), _) => group_match(x, b),
        (_, RExpr::Collate(y, _)) => group_match(a, y),
        _ => false,
    }
}

/// The aggregates a group finalizes before its HAVING decides
/// (v0.16, ungated): every aggregate of the result list and the
/// ORDER BY. The pin computes them all before testing HAVING, so
/// one that errors (`sum()` overflow, the percentile checks) fails
/// the statement even for a group HAVING drops.
#[cfg(feature = "v04")]
fn group_aggs<'x>(
    items: &'x [RExpr],
    order_by: &'x [(RExpr, crate::sql::SortOrder)],
) -> AVec<&'x RExpr> {
    let mut aggs = AVec::new();
    for it in items.iter() {
        crate::query::collect_outer_aggs(it, &mut aggs);
    }
    for (e, _) in order_by.iter() {
        crate::query::collect_outer_aggs(e, &mut aggs);
    }
    aggs
}

/// How many groups a grouped query needs before it can stop
/// (v0.16, ungated): with a LIMIT and output in group order (no
/// ORDER BY, or one naming exactly the GROUP BY terms), and no
/// DISTINCT, the pin emits groups as it finalizes them and stops
/// at OFFSET + LIMIT — later groups are never finalized, so their
/// aggregates cannot error. None: every group is needed.
#[cfg(feature = "v04")]
fn group_cap(
    group_by: &[RExpr],
    order_by: &[(RExpr, crate::sql::SortOrder)],
    distinct: bool,
    (skip, take): (usize, usize),
) -> Option<usize> {
    if group_by.is_empty() || distinct || take == usize::MAX {
        return None;
    }
    let in_group_order = order_by.is_empty()
        || (order_by.len() == group_by.len()
            && order_by
                .iter()
                .zip(group_by.iter())
                .all(|((o, _), g)| crate::query::same_expr(o, g)));
    in_group_order.then(|| skip.saturating_add(take))
}

/// How many rows a query needs from its FROM clause when that
/// is one derived table it reads in order (v0.16 review fix):
/// with a LIMIT, no WHERE / grouping / aggregates (`reduced`)
/// and no ORDER BY / DISTINCT (`reordered`), the pin stops reading the
/// subquery after OFFSET + LIMIT rows. None: every row is
/// needed (an outer WHERE may be pushed into the subquery on the
/// pin, which hipp does not model; see STATUS.md).
/// Whether a reader's `need` bounds what derived plan `p`
/// computes (v0.16 review fix, `v16`): a recursive CTE stops its
/// queue there, and a plain core passes the bound on to a first
/// source that honours it (`streams_first`).
#[cfg(feature = "v16")]
fn need_bounds(p: &crate::query::SubPlan) -> bool {
    if p.rec.is_some() {
        return true;
    }
    p.compound.is_empty()
        && p.values.is_empty()
        && streams_first(
            p.sources.as_slice(),
            p.group_by.is_empty() && !p.has_agg && p.order_by.is_empty() && !p.distinct,
        )
}

/// Whether a query over `sources` (`plain`: no grouping,
/// aggregate, ORDER BY or DISTINCT) makes its first rows from a
/// prefix of its first source's: the first source is a derived
/// table that honours a row bound (`need_bounds`) and no RIGHT /
/// FULL join emits rows after the main loop. The pin runs such a
/// first source as a co-routine and stops it at the reader's
/// LIMIT, so an endless recursive CTE read through a WHERE or a
/// join answers (v0.16 review fix, `v16`).
#[cfg(feature = "v16")]
fn streams_first(sources: &[SourcePlan], plain: bool) -> bool {
    plain
        && !sources.iter().any(|s| {
            matches!(
                s.kind,
                crate::sql::JoinKind::Right | crate::sql::JoinKind::Full
            )
        })
        && sources
            .first()
            .and_then(|s| s.derived.as_deref())
            .is_some_and(|d| d.mat == 0 && need_bounds(d))
}

#[cfg(feature = "v09")]
fn derived_need(
    sources: &[SourcePlan],
    reduced: bool,
    reordered: bool,
    (skip, take): (usize, usize),
) -> Option<usize> {
    let [only] = sources else {
        return None;
    };
    (only.derived.is_some() && !reduced && !reordered && take != usize::MAX)
        .then(|| skip.saturating_add(take))
}

/// The result column ORDER BY term `e` (resolved: `r`) names, if
/// any (v0.16 review fix, `window`; the pin's `iOrderByCol`): an
/// ordinal, a bare alias (under COLLATE too), or a window call the
/// same as a result column's (the last such column).
#[cfg(feature = "window")]
fn order_ref(
    e: &Expr,
    r: &RExpr,
    arena: &Arena,
    items: &[RExpr],
    aliases: &[Option<AVec<u8>>],
) -> Option<usize> {
    let (mut e, mut r) = (e, r);
    while let (Expr::Collate(x, _), RExpr::Collate(y, _)) = (e, r) {
        e = x;
        r = y;
    }
    match e {
        Expr::Literal(Literal::Integer(n)) => usize::try_from(*n)
            .ok()
            .and_then(|k| k.checked_sub(1))
            .filter(|k| *k < items.len()),
        Expr::Column { table: None, name } => {
            let nb = handle_bytes(arena, *name);
            aliases
                .iter()
                .position(|a| a.as_ref().is_some_and(|x| eq_ci(x.as_slice(), nb)))
        }
        _ => items.iter().rposition(|it| crate::window::same_call(r, it)),
    }
}

/// The direction each GROUP BY term's groups come out in
/// (v0.16): when the ORDER BY has exactly as many terms as the
/// GROUP BY, the pin's group sorter takes each ORDER BY term's
/// ASC / DESC for the GROUP BY term in the same position
/// (`sqlite3Select`'s orderByGrp set-up), whatever the terms
/// are — so ties under the ORDER BY keep that group order
/// (`GROUP BY a ORDER BY b DESC` lists equal-`b` groups by `a`
/// descending). Otherwise every group term is ascending.
///
/// A core with window calls (v0.16 review fix, `window`) ignores
/// its ORDER BY here: the pin's `sqlite3WindowRewrite` moves the
/// grouping into a subquery ordered by the innermost window's sort
/// keys (`window::inner_sort_desc`), so those directions apply —
/// when there are as many keys as GROUP BY terms — whatever the
/// outer ORDER BY says.
#[cfg(feature = "v04")]
fn group_desc(
    group_by: &[RExpr],
    items: &[RExpr],
    order_by: &[(RExpr, crate::sql::SortOrder)],
) -> AVec<bool> {
    let mut out = AVec::new();
    #[cfg(feature = "window")]
    if let Some(d) = crate::window::inner_sort_desc(items, order_by) {
        for i in 0..group_by.len() {
            out.push(d.len() == group_by.len() && d.as_slice().get(i).copied().unwrap_or(false));
        }
        return out;
    }
    #[cfg(not(feature = "window"))]
    let _ = items;
    for i in 0..group_by.len() {
        out.push(group_by.len() == order_by.len() && order_by.get(i).is_some_and(|(_, o)| o.desc));
    }
    out
}

/// DISTINCT over produced rows (v0.4): first occurrences
/// kept, values compared NULL-safely under each result
/// column's collation (v0.16).
#[cfg(feature = "v04")]
fn dedup_produced(produced: AVec<Produced>, colls: &[crate::record::Coll]) -> AVec<Produced> {
    let mut kept: AVec<Produced> = AVec::new();
    for p in produced.iter() {
        let dup = kept.iter().any(|k: &Produced| {
            k.out.len() == p.out.len()
                && compound_rows_equal(k.out.as_slice(), p.out.as_slice(), colls)
        });
        if !dup {
            kept.push(p.clone());
        }
    }
    kept
}

/// Per-source "matched" flags for a join's RIGHT / FULL
/// sources (v0.16): one flag per materialised row of each such
/// source, empty for every other source.
#[cfg(feature = "v04")]
fn right_match_flags(
    sources: &[SourcePlan],
    src_rows: &[AVec<(AVec<OwnedValue>, Option<i64>)>],
) -> AVec<AVec<bool>> {
    let mut m = AVec::new();
    for (sp, rows) in sources.iter().zip(src_rows.iter()) {
        let mut f = AVec::new();
        if sp.kind.pads_left() {
            for _ in rows.iter() {
                f.push(false);
            }
        }
        m.push(f);
    }
    m
}

/// Record that row `ri` of source `idx` was matched (v0.16; a
/// no-op for sources that keep no flags).
#[cfg(feature = "v04")]
fn mark_matched(matched: &mut AVec<AVec<bool>>, idx: usize, ri: usize) {
    if let Some(f) = matched
        .as_mut_slice()
        .get_mut(idx)
        .and_then(|m| m.as_mut_slice().get_mut(ri))
    {
        *f = true;
    }
}

/// Whether row `ri` of source `idx` was matched (v0.16).
#[cfg(feature = "v04")]
fn row_matched(matched: &AVec<AVec<bool>>, idx: usize, ri: usize) -> bool {
    matched
        .as_slice()
        .get(idx)
        .and_then(|m| m.as_slice().get(ri))
        .copied()
        .unwrap_or(true)
}

/// Append a NULL row (all columns, no rowid) for each of
/// `sources` to `acc` (v0.16).
#[cfg(feature = "v04")]
fn pad_sources(acc: &mut RowData, sources: &[SourcePlan]) {
    for sp in sources.iter() {
        for _ in 0..sp.ncols {
            acc.vals.push(OwnedValue::Null);
        }
        acc.rowids.push(None);
    }
}

/// The accumulator for an unmatched row of RIGHT / FULL source
/// `i` (v0.16): the outer row, NULLs for every source before
/// `i`, then the row itself.
#[cfg(feature = "v04")]
fn left_padded(
    outer: &RowData,
    sources: &[SourcePlan],
    i: usize,
    vals: &AVec<OwnedValue>,
    rowid: Option<i64>,
) -> RowData {
    let mut acc = outer.clone();
    pad_sources(&mut acc, sources.get(..i).unwrap_or(&[]));
    for v in vals.iter() {
        acc.vals.push(v.clone());
    }
    acc.rowids.push(rowid);
    acc
}

/// Whether any expression of a query plan contains a
/// current-scope subquery node (v0.6): such plans execute
/// through the substitution path instead of the synchronous
/// v0.4 executor.
#[cfg(feature = "v06")]
fn query_plan_has_sub(qp: &QueryPlan) -> bool {
    use crate::query::has_sub;
    qp.sources.iter().any(|s| s.derived.is_some())
        || qp
            .sources
            .iter()
            .any(|s| s.on.as_ref().is_some_and(has_sub))
        || qp.filter.as_ref().is_some_and(has_sub)
        || qp.items.iter().any(has_sub)
        || qp.group_by.iter().any(has_sub)
        || qp.having.as_ref().is_some_and(has_sub)
        || qp.order_by.iter().any(|(e, _)| has_sub(e))
}

/// One produced result row of a v0.4 query (output values, sort
/// keys, representative rowid for single-source queries).
#[cfg(feature = "v04")]
#[derive(Debug, Clone)]
struct Produced {
    out: AVec<OwnedValue>,
    keys: AVec<OwnedValue>,
    rowid: Option<i64>,
}

/// The synchronous nested-loop join of a query plan's sources
/// (v0.4; RIGHT / FULL since v0.16): the main loop, then each
/// RIGHT / FULL source's unmatched rows NULL-padded on the left
/// and joined onward — the `join_sources` order, without
/// subquery execution.
#[cfg(feature = "v04")]
fn join_all<E>(
    env: &crate::query::Env,
    qp: &QueryPlan,
    src_rows: &[AVec<(AVec<OwnedValue>, Option<i64>)>],
    out: &mut AVec<RowData>,
) -> Result<(), E> {
    let mut matched = right_match_flags(qp.sources.as_slice(), src_rows);
    join_rows::<E>(
        env,
        qp,
        src_rows,
        0,
        &mut RowData::empty(),
        &mut matched,
        out,
    )?;
    let empty = RowData::empty();
    for (i, (sp, rows)) in qp.sources.iter().zip(src_rows.iter()).enumerate() {
        if !sp.kind.pads_left() {
            continue;
        }
        for (ri, (vals, rowid)) in rows.iter().enumerate() {
            if row_matched(&matched, i, ri) {
                continue;
            }
            let mut acc = left_padded(&empty, qp.sources.as_slice(), i, vals, *rowid);
            join_rows::<E>(env, qp, src_rows, i + 1, &mut acc, &mut matched, out)?;
        }
    }
    Ok(())
}

/// Nested-loop join (v0.4): extend `acc` with source `idx`'s
/// rows, recurse; a LEFT / FULL source that matched nothing
/// extends once with NULLs, and RIGHT / FULL matches are
/// recorded in `matched` for `join_all`'s tail. The WHERE
/// filter applies to complete rows.
#[cfg(feature = "v04")]
fn join_rows<E>(
    env: &crate::query::Env,
    qp: &QueryPlan,
    src_rows: &[AVec<(AVec<OwnedValue>, Option<i64>)>],
    idx: usize,
    acc: &mut RowData,
    matched: &mut AVec<AVec<bool>>,
    out: &mut AVec<RowData>,
) -> Result<(), E> {
    let (Some(sp), Some(rows)) = (qp.sources.as_slice().get(idx), src_rows.get(idx)) else {
        let keep = match &qp.filter {
            None => true,
            Some(f) => {
                crate::query::truthy(&crate::query::eval::<E>(f, &Ctx::of(acc).with(env))?)
                    == Some(true)
            }
        };
        if keep {
            out.push(acc.clone());
        }
        return Ok(());
    };
    let base = acc.vals.len();
    let mut any = false;
    for (ri, (vals, rowid)) in rows.iter().enumerate() {
        for v in vals.iter() {
            acc.vals.push(v.clone());
        }
        acc.rowids.push(*rowid);
        let on_ok = match &sp.on {
            None => true,
            Some(on) => {
                crate::query::truthy(&crate::query::eval::<E>(on, &Ctx::of(acc).with(env))?)
                    == Some(true)
            }
        };
        if on_ok {
            any = true;
            mark_matched(matched, idx, ri);
            join_rows::<E>(env, qp, src_rows, idx + 1, acc, matched, out)?;
        }
        while acc.vals.len() > base {
            acc.vals.pop();
        }
        acc.rowids.pop();
    }
    if sp.kind.pads_right() && !any {
        for _ in 0..sp.ncols {
            acc.vals.push(OwnedValue::Null);
        }
        acc.rowids.push(None);
        join_rows::<E>(env, qp, src_rows, idx + 1, acc, matched, out)?;
        while acc.vals.len() > base {
            acc.vals.pop();
        }
        acc.rowids.pop();
    }
    Ok(())
}

/// Group (when the plan aggregates) and project joined rows.
#[cfg(feature = "v04")]
fn project_rows<E>(
    env: &crate::query::Env,
    qp: &QueryPlan,
    rows: &[RowData],
    cap: Option<usize>,
) -> Result<AVec<Produced>, E> {
    let mut out = AVec::new();
    // Window calls (v0.16, `window`): each input row (or group)
    // first evaluates the calls' inputs; the projection waits for
    // the windows (`win_project`). A group LIMIT cannot apply.
    #[cfg(feature = "window")]
    let wins = crate::window::calls(qp.items.as_slice(), qp.order_by.as_slice());
    #[cfg(feature = "window")]
    let cap = if wins.is_empty() { cap } else { None };
    #[cfg(feature = "window")]
    let mut pend: AVec<WinUnit> = AVec::new();
    if !qp.has_agg {
        #[cfg(feature = "window")]
        let mut row_at = 0usize;
        for r in rows {
            let ctx = Ctx::of(r).with(env);
            #[cfg(feature = "window")]
            if !wins.is_empty() {
                let ins = crate::window::inputs::<Error<E>>(wins.as_slice(), &mut |k| {
                    crate::query::eval::<E>(k, &ctx)
                })?;
                pend.push(WinUnit {
                    at: row_at,
                    rep: None,
                    ins,
                });
                row_at += 1;
                continue;
            }
            let mut vals = AVec::new();
            for it in qp.items.iter() {
                vals.push(crate::query::eval::<E>(it, &ctx)?);
            }
            let mut keys = AVec::new();
            for (e, _) in qp.order_by.iter() {
                keys.push(crate::query::eval::<E>(e, &ctx)?);
            }
            let rowid = if qp.sources.len() == 1 {
                r.rowids.as_slice()[0]
            } else {
                None
            };
            out.push(Produced {
                out: vals,
                keys,
                rowid,
            });
        }
        #[cfg(feature = "window")]
        if !wins.is_empty() {
            return win_project::<E>(env, qp, wins.as_slice(), pend, rows, &[]);
        }
        return Ok(out);
    }
    // Grouping.
    let total_width: usize = qp.sources.iter().map(|s| s.ncols).sum();
    let empty_rep = RowData {
        vals: {
            let mut v = AVec::new();
            for _ in 0..total_width {
                v.push(OwnedValue::Null);
            }
            v
        },
        rowids: {
            let mut v = AVec::new();
            for _ in qp.sources.iter() {
                v.push(None);
            }
            v
        },
    };
    let mut groups: AVec<(AVec<OwnedValue>, AVec<RowData>)> = AVec::new();
    if qp.group_by.is_empty() {
        let mut all = AVec::new();
        for r in rows {
            all.push(r.clone());
        }
        groups.push((AVec::new(), all));
    } else {
        // v0.16: groups form and sort under each term's collation.
        let gcolls = crate::query::exprs_colls(qp.group_by.as_slice());
        let gdesc = group_desc(
            qp.group_by.as_slice(),
            qp.items.as_slice(),
            qp.order_by.as_slice(),
        );
        for r in rows {
            let ctx = Ctx::of(r).with(env);
            let mut key = AVec::new();
            for g in qp.group_by.iter() {
                key.push(crate::query::eval::<E>(g, &ctx)?);
            }
            let mut found = None;
            for (gi, (k, _)) in groups.iter().enumerate() {
                if k.len() == key.len()
                    && k.iter()
                        .zip(key.iter())
                        .zip(gcolls.iter())
                        .all(|((a, b), c)| crate::query::values_equal_coll(a, b, *c))
                {
                    found = Some(gi);
                    break;
                }
            }
            match found {
                Some(gi) => groups.as_mut_slice()[gi].1.push(r.clone()),
                None => {
                    let mut g = AVec::new();
                    g.push(r.clone());
                    groups.push((key, g));
                }
            }
        }
        // SQLite's GROUP BY emits groups in key order (its sorter).
        groups.as_mut_slice().sort_by(|a, b| {
            for (((av, bv), c), d) in
                a.0.iter()
                    .zip(b.0.iter())
                    .zip(gcolls.iter())
                    .zip(gdesc.iter())
            {
                let o = crate::record::compare_coll(&av.as_value(), &bv.as_value(), *c);
                let o = if *d { o.reverse() } else { o };
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            core::cmp::Ordering::Equal
        });
    }
    #[cfg(feature = "window")]
    let mut group_at = usize::MAX;
    for (_, grows) in groups.iter() {
        #[cfg(feature = "window")]
        {
            group_at = group_at.wrapping_add(1);
        }
        if cap.is_some_and(|c| out.len() >= c) {
            break;
        }
        let rep = pick_rep::<E>(env, qp, grows.as_slice(), &empty_rep)?;
        let ctx = Ctx::group(grows.as_slice(), &rep).with(env);
        if let Some(h) = &qp.having {
            let mut early = AVec::new();
            having_early(h, qp.group_by.as_slice(), &mut early);
            let mut pass = true;
            for c in early.iter() {
                pass &= crate::query::truthy(&crate::query::eval::<E>(c, &ctx)?) == Some(true);
            }
            if !pass {
                continue;
            }
            for a in group_aggs(qp.items.as_slice(), qp.order_by.as_slice()).iter() {
                crate::query::eval::<E>(a, &ctx)?;
            }
            if crate::query::truthy(&crate::query::eval::<E>(h, &ctx)?) != Some(true) {
                continue;
            }
        }
        #[cfg(feature = "window")]
        if !wins.is_empty() {
            let ins = crate::window::inputs::<Error<E>>(wins.as_slice(), &mut |k| {
                crate::query::eval::<E>(k, &ctx)
            })?;
            pend.push(WinUnit {
                at: group_at,
                rep: Some(rep),
                ins,
            });
            continue;
        }
        let mut vals = AVec::new();
        for it in qp.items.iter() {
            vals.push(crate::query::eval::<E>(it, &ctx)?);
        }
        let mut keys = AVec::new();
        for (e, _) in qp.order_by.iter() {
            keys.push(crate::query::eval::<E>(e, &ctx)?);
        }
        out.push(Produced {
            out: vals,
            keys,
            rowid: None,
        });
    }
    #[cfg(feature = "window")]
    if !wins.is_empty() {
        return win_project::<E>(env, qp, wins.as_slice(), pend, rows, groups.as_slice());
    }
    Ok(out)
}

/// One input row of a core's windows (v0.16, `window`): the row
/// (`rep` None: index into the joined rows) or group (`rep` its
/// representative: index into the groups), and its window calls'
/// input values.
#[cfg(feature = "window")]
struct WinUnit {
    at: usize,
    rep: Option<RowData>,
    ins: AVec<AVec<OwnedValue>>,
}

/// The windows' results for `units` (v0.16, `window`): the units in
/// the window order, each call's value per unit, and the stripped
/// templates of the result columns and ORDER BY terms.
#[cfg(feature = "window")]
#[allow(clippy::type_complexity)]
fn win_results<E>(
    wins: &[&RExpr],
    units: &mut AVec<WinUnit>,
    items: &[RExpr],
    order_by: &[(RExpr, crate::sql::SortOrder)],
) -> Result<
    (
        AVec<usize>,
        AVec<AVec<OwnedValue>>,
        AVec<RExpr>,
        AVec<RExpr>,
    ),
    E,
> {
    let mut ins: AVec<AVec<AVec<OwnedValue>>> = AVec::with_capacity(units.len());
    for u in units.as_mut_slice() {
        ins.push(core::mem::take(&mut u.ins));
    }
    let (order, vals) =
        crate::window::compute(wins, ins.as_slice()).map_err(|kind| Error::Schema { kind })?;
    let mut ti = AVec::new();
    for it in items {
        ti.push(crate::window::strip(it));
    }
    let mut to = AVec::new();
    for (e, _) in order_by {
        to.push(crate::window::strip(e));
    }
    Ok((order, vals, ti, to))
}

/// `project_rows`' window path (v0.16, `window`): compute the
/// windows, then project each unit in the window order with its
/// calls' values in place.
#[cfg(feature = "window")]
fn win_project<E>(
    env: &crate::query::Env,
    qp: &QueryPlan,
    wins: &[&RExpr],
    mut units: AVec<WinUnit>,
    rows: &[RowData],
    groups: &[(AVec<OwnedValue>, AVec<RowData>)],
) -> Result<AVec<Produced>, E> {
    let (order, vals, ti, to) = win_results::<E>(
        wins,
        &mut units,
        qp.items.as_slice(),
        qp.order_by.as_slice(),
    )?;
    let empty = RowData::empty();
    let mut out = AVec::with_capacity(order.len());
    for &u in order.iter() {
        let (Some(unit), Some(v)) = (units.as_slice().get(u), vals.as_slice().get(u)) else {
            continue;
        };
        let (ctx, rowid) = match &unit.rep {
            None => {
                let r = rows.get(unit.at).unwrap_or(&empty);
                let rowid = if qp.sources.len() == 1 {
                    r.rowids.as_slice().first().copied().flatten()
                } else {
                    None
                };
                (Ctx::of(r).with(env), rowid)
            }
            Some(rep) => {
                let g = groups.get(unit.at).map(|g| g.1.as_slice()).unwrap_or(&[]);
                (Ctx::group(g, rep).with(env), None)
            }
        };
        let mut vs = AVec::new();
        for t in ti.iter() {
            let mut e = t.clone();
            crate::window::fill(&mut e, v.as_slice());
            vs.push(crate::query::eval::<E>(&e, &ctx)?);
        }
        let mut keys = AVec::new();
        for t in to.iter() {
            let mut e = t.clone();
            crate::window::fill(&mut e, v.as_slice());
            keys.push(crate::query::eval::<E>(&e, &ctx)?);
        }
        out.push(Produced {
            out: vs,
            keys,
            rowid,
        });
    }
    Ok(out)
}

/// The min/max aggregate whose extremum row supplies a group's
/// bare columns (v0.16 rule, pin-probed): the LAST min() / max()
/// aggregate among the result items, ORDER BY terms and HAVING
/// (the pin's aggregate order — each one's `OP_CollSeq` resets
/// the shared "hit" flag, so only the last decides), whatever
/// other aggregates the query has. None: bare columns read the
/// group's first row. (v0.4 – v0.15 honoured only a lone
/// min/max aggregate.)
#[cfg(feature = "v04")]
fn rep_minmax<'x>(
    items: &'x [RExpr],
    having: Option<&'x RExpr>,
    order_by: &'x [(RExpr, crate::sql::SortOrder)],
) -> Option<MinMaxRep<'x>> {
    use crate::query::{AggKind, collect_aggs};
    let mut aggs: AVec<&RExpr> = AVec::new();
    for it in items.iter() {
        collect_aggs(it, &mut aggs);
    }
    for (e, _) in order_by.iter() {
        collect_aggs(e, &mut aggs);
    }
    if let Some(h) = having {
        collect_aggs(h, &mut aggs);
    }
    aggs.iter().rev().find_map(|a| {
        let p = crate::query::agg_parts(a)?;
        if !matches!(p.kind, AggKind::Min | AggKind::Max) {
            return None;
        }
        Some(MinMaxRep {
            kind: p.kind,
            arg: p.args.first()?,
            filter: p.filter,
        })
    })
}

/// The min / max aggregate that decides a group's bare columns
/// (`rep_minmax`): its kind, its argument and (v0.16, `v16`) its
/// FILTER — a row the filter rejects is not stepped, so it never
/// loads the bare columns (none passing: the group's first row).
#[cfg(feature = "v04")]
#[derive(Clone, Copy)]
struct MinMaxRep<'x> {
    kind: crate::query::AggKind,
    arg: &'x RExpr,
    filter: Option<&'x RExpr>,
}

#[cfg(feature = "v06")]
impl MinMaxRep<'_> {
    fn has_sub(&self) -> bool {
        crate::query::has_sub(self.arg) || self.filter.is_some_and(crate::query::has_sub)
    }
}

/// One row of the pin's min/max bare-column rule (v0.16):
/// whether the row's argument value `v` loads the bare columns.
/// A NULL loads them only while no value has been seen (so a
/// group of NULLs reads its LAST row); a value loads them when
/// it is the first or strictly beyond the best so far under the
/// argument's collation (ties keep the earlier row).
#[cfg(feature = "v04")]
fn extremum_step(
    kind: crate::query::AggKind,
    coll: crate::record::Coll,
    best: &mut Option<OwnedValue>,
    v: OwnedValue,
) -> bool {
    if matches!(v, OwnedValue::Null) {
        return best.is_none();
    }
    let take = match best {
        None => true,
        Some(b) => {
            let o = crate::record::compare_coll(&v.as_value(), &b.as_value(), coll);
            if kind == crate::query::AggKind::Min {
                o == core::cmp::Ordering::Less
            } else {
                o == core::cmp::Ordering::Greater
            }
        }
    };
    if take {
        *best = Some(v);
    }
    take
}

/// The representative row of a group (SQLite's bare-column
/// rule): the first row, or the extremum row of the min/max
/// aggregate `rep_minmax` picks (see `extremum_step`).
#[cfg(feature = "v04")]
fn pick_rep_from<E>(
    env: &crate::query::Env,
    items: &[RExpr],
    having: Option<&RExpr>,
    order_by: &[(RExpr, crate::sql::SortOrder)],
    grows: &[RowData],
    empty_rep: &RowData,
) -> Result<RowData, E> {
    let Some(first) = grows.first() else {
        return Ok(empty_rep.clone());
    };
    let Some(m) = rep_minmax(items, having, order_by) else {
        return Ok(first.clone());
    };
    let coll = crate::query::expr_coll(m.arg).unwrap_or_default();
    let mut best: Option<OwnedValue> = None;
    let mut rep = first;
    for r in grows {
        let ctx = Ctx::of(r).with(env);
        if let Some(f) = m.filter
            && crate::query::truthy(&crate::query::eval::<E>(f, &ctx)?) != Some(true)
        {
            continue;
        }
        let v = crate::query::eval::<E>(m.arg, &ctx)?;
        if extremum_step(m.kind, coll, &mut best, v) {
            rep = r;
        }
    }
    Ok(rep.clone())
}

#[cfg(feature = "v04")]
fn pick_rep<E>(
    env: &crate::query::Env,
    qp: &QueryPlan,
    grows: &[RowData],
    empty_rep: &RowData,
) -> Result<RowData, E> {
    pick_rep_from::<E>(
        env,
        qp.items.as_slice(),
        qp.having.as_ref(),
        qp.order_by.as_slice(),
        grows,
        empty_rep,
    )
}

/// The `pick_rep` rule for a subquery's group.
#[cfg(feature = "v06")]
fn pick_rep_sub<E>(
    env: &crate::query::Env,
    plan: &crate::query::SubPlan,
    grows: &[RowData],
    empty_rep: &RowData,
) -> Result<RowData, E> {
    pick_rep_from::<E>(
        env,
        plan.items.as_slice(),
        plan.having.as_ref(),
        plan.order_by.as_slice(),
        grows,
        empty_rep,
    )
}

/// A derived table's column `i` comparison affinity (v0.9; the
/// compound demotion since v0.16): the result item's, demoted
/// across a compound's cores (`rules::compound_affinity`); BLOB for
/// none.
#[cfg(feature = "v09")]
fn derived_col_affinity(
    plan: &crate::query::SubPlan,
    i: usize,
    aff: Option<crate::record::Affinity>,
) -> crate::record::Affinity {
    let a = aff.unwrap_or(crate::record::Affinity::Blob);
    crate::rules::compound_affinity(
        a,
        plan.items.as_slice().get(i),
        plan.compound.iter().map(|(_, t)| &**t),
        i,
    )
}
