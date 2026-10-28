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
    Ast, BinOp, ConflictAction, Expr, Literal, SelectItem, SelectStmt, eq_ci, handle_bytes, parse,
};
use crate::storage::Storage;

#[cfg(feature = "indexes")]
use crate::schema::IndexOrigin;

#[cfg(feature = "v04")]
use crate::query::{Ctx, RExpr, RowData};
#[cfg(feature = "v04")]
use crate::sql::JoinKind;

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
}

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
        }
    }
}

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
    pub fn page_size(mut self, n: u32) -> Config {
        self.create_page_size = n;
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
        desc: AVec<bool>,
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
    /// Per-key-column DESC flags, parallel to `cols`.
    desc: AVec<bool>,
}

#[cfg(feature = "indexes")]
#[derive(Debug, Clone, Copy)]
struct IdxColPlan {
    col: Option<usize>,
    desc: bool,
}

enum Plan {
    CreateTable,
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
    },
    /// `DROP TABLE` (v0.5); `root: None` is the IF EXISTS no-op.
    #[cfg(feature = "v05")]
    DropTable {
        root: Option<u32>,
        /// Roots of the table's indexes (explicit + implicit),
        /// in catalog order; their pages are freed with it.
        index_roots: AVec<u32>,
    },
    /// `ALTER TABLE ... ADD COLUMN` (v0.4): the rewritten stored
    /// CREATE TABLE text is computed at plan time.
    #[cfg(feature = "v04")]
    AlterAdd {
        root: u32,
        table: AVec<u8>,
        new_sql: AVec<u8>,
    },
    /// `CREATE VIEW` (v0.4).
    #[cfg(feature = "v04")]
    CreateView,
    /// `DROP VIEW` (v0.4); `exists: false` is the IF EXISTS no-op.
    #[cfg(feature = "v04")]
    DropView {
        exists: bool,
    },
}

/// One FROM source in a v0.4 query plan.
#[cfg(feature = "v04")]
#[derive(Debug, Clone)]
struct SourcePlan {
    root: u32,
    kind: JoinKind,
    on: Option<RExpr>,
    rowid_alias: Option<usize>,
    ncols: usize,
    /// Per-column read-side affinity (REAL forcing, §5.5).
    affs: AVec<crate::record::Affinity>,
    defaults: AVec<OwnedValue>,
    /// A derived table (v0.9): when `Some`, the source's rows
    /// are this subplan's result rows (executed once against
    /// the empty outer row at materialisation time; `root` is
    /// unused). Planned against no enclosing scope — a FROM
    /// subquery sees neither sibling sources nor (at statement
    /// level) any outer query. Only filled under `v09`.
    derived: Option<Box<crate::query::SubPlan>>,
}

/// A v0.4 query plan: everything resolved at prepare time.
#[cfg(feature = "v04")]
#[derive(Debug, Clone)]
struct QueryPlan {
    sources: AVec<SourcePlan>,
    items: AVec<RExpr>,
    filter: Option<RExpr>,
    group_by: AVec<RExpr>,
    having: Option<RExpr>,
    order_by: AVec<(RExpr, bool)>,
    distinct: bool,
    limit: Option<i64>,
    offset: i64,
    has_agg: bool,
    width: usize,
    /// Compound tail (v0.9; see `SubPlan::compound`). When
    /// non-empty, `order_by` is empty, `comp_order` carries
    /// the compound ORDER BY, and `limit` / `offset` apply to
    /// the combined result. Only filled under `v09`.
    compound: AVec<(crate::sql::CompoundOp, Box<QueryPlan>)>,
    /// A VALUES core's resolved rows (v0.9; see
    /// `SubPlan::values`).
    values: AVec<AVec<RExpr>>,
    /// Compound ORDER BY (v0.9; see `SubPlan::comp_order`).
    comp_order: AVec<(usize, bool)>,
    /// Per-result-item ORDER BY name keys (v0.9): `(qualifier,
    /// name)` for a plain column item (qualifier = its source's
    /// qualifier), `(None, alias-or-text)` for other items,
    /// expanded per column for star items. The compound ORDER
    /// BY resolver matches a term against these across ALL of
    /// the compound's cores (pin-verified behaviour).
    order_names: AVec<crate::query::OrderName>,
}

struct InsCol {
    affinity: crate::record::Affinity,
    default: OwnedValue,
    /// `NOT NULL` constraint on the column (v0.10).
    not_null: bool,
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
    AVec<(usize, bool)>,
);

/// A planned top-level compound tail plus its resolved
/// compound ORDER BY (v0.9).
#[cfg(feature = "v04")]
type QueryCompoundParts = (
    AVec<(crate::sql::CompoundOp, Box<QueryPlan>)>,
    AVec<(usize, bool)>,
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
        let mut conn = Connection {
            pager,
            catalog_arena,
            catalog: Catalog::empty(u32::MAX),
            cfg,
            explicit_txn: false,
        };
        conn.rebuild_catalog().await?;
        Ok(conn)
    }

    async fn rebuild_catalog(&mut self) -> Result<(), S::Error> {
        self.catalog_arena.reset();
        self.catalog = build_catalog(&mut self.pager, &self.catalog_arena).await?;
        Ok(())
    }

    /// Prepare one statement (§5.6 grammar). A stale schema
    /// cookie forces a catalog rebuild first (§5.5); a commit left
    /// mid-flight by a dropped future must be resumed or rolled
    /// back before anything new is prepared (§5.1.1).
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
        let ast = parse::<S::Error>(sql.as_bytes(), &arena)?;
        let plan = self.plan(&arena, &ast)?;
        let row_ckpt = arena.checkpoint();
        Ok(Statement {
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
            #[cfg(feature = "v07")]
            ret_rows: AVec::new(),
            #[cfg(feature = "v07")]
            ret_rowids: AVec::new(),
            #[cfg(feature = "v07")]
            ret_pos: 0,
            #[cfg(feature = "v07")]
            ret_loaded: false,
        })
    }

    fn plan(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        match ast {
            Ast::CreateTable { .. } => Ok(Plan::CreateTable),
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
                    })?;
                let mut cols = AVec::new();
                for cd in columns.iter() {
                    let nb = handle_bytes(arena, cd.name);
                    let col = match self.resolve_target(t, nb)? {
                        Target::Rowid => None,
                        Target::Col(i) => {
                            if Some(i) == t.rowid_alias {
                                None
                            } else {
                                Some(i)
                            }
                        }
                    };
                    cols.push(IdxColPlan { col, desc: cd.desc });
                }
                let mut defaults = AVec::new();
                for c in t.columns.iter() {
                    defaults.push(literal_owned(&self.catalog_arena, c.default));
                }
                Ok(Plan::CreateIndex {
                    table_root: t.root,
                    rowid_alias: t.rowid_alias,
                    ncols: t.columns.len(),
                    defaults,
                    cols,
                    unique: *unique,
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
                    })?;
                let mut cols = AVec::new();
                for c in t.columns.iter() {
                    cols.push(InsCol {
                        affinity: c.affinity,
                        default: literal_owned(&self.catalog_arena, c.default),
                        not_null: c.not_null,
                    });
                }
                #[cfg(not(feature = "v08"))]
                let _ = select;
                // Every row of a multi-row VALUES must match the
                // expected width (SQLite: "all VALUES must have
                // the same number of terms").
                let width = match columns {
                    None => t.columns.len(),
                    Some(names) => names.len(),
                };
                for r in rows.iter() {
                    if r.len() != width {
                        return Err(Error::Schema {
                            kind: SchemaKind::ColumnCountMismatch,
                        });
                    }
                }
                let mut targets = AVec::new();
                match columns {
                    None => {
                        for i in 0..t.columns.len() {
                            targets.push(Target::Col(i));
                        }
                    }
                    Some(names) => {
                        for n in names.iter() {
                            let nb = handle_bytes(arena, *n);
                            targets.push(self.resolve_target(t, nb)?);
                        }
                    }
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
                            cols.push(IdxColPlan {
                                col: c.col,
                                desc: c.desc,
                            });
                            desc.push(c.desc);
                        }
                        out.push(IndexPlan {
                            root: ix.root,
                            unique: ix.unique,
                            cols,
                            desc,
                        });
                    }
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
                        // only to a bare single core.
                        if upsert.is_some()
                            && sel.compound.is_empty()
                            && !sel.from.is_empty()
                            && sel.filter.is_none()
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
                    let cur = crate::query::ScopeGroup {
                        sources: AVec::new(),
                        offsets: AVec::new(),
                        first_src: 0,
                    };
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
                            rr.push(re);
                        }
                        resolved.push(rr);
                    }
                    if any_sub {
                        (AVec::new(), Some(resolved))
                    } else {
                        let empty = RowData::empty();
                        let mut folded = AVec::new();
                        for rr in resolved.iter() {
                            let mut rv = AVec::new();
                            for re in rr.iter() {
                                rv.push(crate::query::eval::<S::Error>(re, &Ctx::Row(&empty))?);
                            }
                            folded.push(rv);
                        }
                        (folded, None)
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
                })
            }
            Ast::Begin => {
                if self.explicit_txn {
                    return Err(Error::TransactionState { nested: true });
                }
                Ok(Plan::Begin)
            }
            Ast::Commit => Ok(Plan::Commit),
            Ast::Rollback => Ok(Plan::Rollback),
            Ast::Select(stmt) => self.plan_select(stmt, arena),
            #[cfg(feature = "v04")]
            Ast::AlterAddColumn {
                table,
                column,
                text,
            } => {
                let tname = handle_bytes(arena, *table);
                let t = match self.catalog.find(&self.catalog_arena, tname) {
                    Some(t) => t,
                    None => {
                        if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                            return Err(Error::Unsupported);
                        }
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownTable,
                        });
                    }
                };
                let cname = handle_bytes(arena, column.name);
                if t.columns
                    .iter()
                    .any(|c| eq_ci(handle_bytes(&self.catalog_arena, c.name), cname))
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::DuplicateColumn,
                    });
                }
                if column.primary_key
                    || column.pk_desc
                    || column.unique
                    || column.generated
                    || column.default_nonconst
                {
                    return Err(Error::Schema {
                        kind: SchemaKind::CannotAddColumn,
                    });
                }
                if column.not_null && matches!(column.default, None | Some(Literal::Null)) {
                    return Err(Error::Schema {
                        kind: SchemaKind::CannotAddColumn,
                    });
                }
                let base = handle_bytes(&self.catalog_arena, t.sql);
                let pos = base
                    .iter()
                    .rposition(|b| *b == b')')
                    .ok_or(Error::Unsupported)?;
                let mut new_sql = AVec::new();
                new_sql.extend_from_slice(&base[..pos]);
                new_sql.extend_from_slice(b", ");
                new_sql.extend_from_slice(handle_bytes(arena, *text));
                new_sql.extend_from_slice(&base[pos..]);
                Ok(Plan::AlterAdd {
                    root: t.root,
                    table: copy_vec(tname),
                    new_sql,
                })
            }
            #[cfg(feature = "v04")]
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
                        Ok(Plan::DropTable {
                            root: Some(t.root),
                            index_roots,
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
        match self.catalog.find(&self.catalog_arena, tname) {
            Some(t) => Ok(t),
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
        sources.push(crate::query::SourceDesc {
            qualifier: copy_vec(qualifier),
            columns: cols,
            rowid_alias: t.rowid_alias,
            has_rowid: true,
        });
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
                cols.push(IdxColPlan {
                    col: c.col,
                    desc: c.desc,
                });
                desc.push(c.desc);
            }
            out.push(IndexPlan {
                root: ix.root,
                unique: ix.unique,
                cols,
                desc,
            });
        }
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
    /// scoped in v0.6): an output ordinal or alias substitutes
    /// the item expression itself (SQLite's rule); anything else
    /// resolves as an ordinary expression.
    #[cfg(feature = "v04")]
    #[allow(clippy::too_many_arguments)]
    fn resolve_term(
        &self,
        e: &Expr,
        arena: &Arena,
        items: &[RExpr],
        aliases: &[Option<AVec<u8>>],
        cur: &crate::query::ScopeGroup,
        enclosing: &[crate::query::ScopeGroup],
        planner: crate::query::SubPlanner<'_, S::Error>,
    ) -> Result<RExpr, S::Error> {
        match e {
            Expr::Literal(Literal::Integer(n)) if *n >= 1 && (*n as usize) <= items.len() => {
                Ok(items[*n as usize - 1].clone())
            }
            Expr::Column { table: None, name } => {
                let nb = handle_bytes(arena, *name);
                match aliases
                    .iter()
                    .position(|a| a.as_ref().is_some_and(|x| eq_ci(x.as_slice(), nb)))
                {
                    Some(idx) => Ok(items[idx].clone()),
                    None => {
                        crate::query::resolve_scoped::<S::Error>(e, arena, cur, enclosing, planner)
                    }
                }
            }
            _ => crate::query::resolve_scoped::<S::Error>(e, arena, cur, enclosing, planner),
        }
    }

    /// Plan one subquery (v0.6) against the combined scope:
    /// `enclosing` holds the scope groups of every textually
    /// enclosing query, nearest first, with absolute slot/source
    /// numbering into the combined evaluation row. The
    /// subquery's own sources are numbered after all enclosing
    /// sources. Mirrors `plan_query`, including term resolution
    /// and the aggregate-argument subquery refusal.
    #[cfg(feature = "v06")]
    fn plan_subquery(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
    ) -> Result<crate::query::SubPlan, S::Error> {
        use crate::query::{
            ScopeGroup, SourceDesc, SubPlan, SubSource, has_agg, resolve_scoped, sub_in_agg_args,
        };
        let base_width: usize = enclosing.iter().map(|g| g.width()).sum();
        let base_src: usize = enclosing.iter().map(|g| g.sources.len()).sum();
        // A VALUES core (v0.9): no sources of its own; the rows
        // resolve against the enclosing scopes alone.
        #[cfg(feature = "v09")]
        if !stmt.values.is_empty() {
            let cur = ScopeGroup {
                sources: AVec::new(),
                offsets: AVec::new(),
                first_src: base_src,
            };
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
                for e in r.iter() {
                    let re = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                    if has_agg(&re) {
                        return Err(Error::Unsupported);
                    }
                    rr.push(re);
                }
                rows_r.push(rr);
            }
            let width = width.unwrap_or(0);
            let mut result_names: AVec<AVec<u8>> = AVec::new();
            let mut result_affs: AVec<Option<crate::record::Affinity>> = AVec::new();
            let mut order_names: AVec<crate::query::OrderName> = AVec::new();
            for i in 0..width {
                let n = values_col_name(i);
                result_names.push(n.clone());
                order_names.push((None, n));
                result_affs.push(None);
            }
            let (compound, comp_order) = if stmt.compound.is_empty() {
                (AVec::new(), AVec::new())
            } else {
                self.plan_sub_compound(stmt, arena, enclosing, order_names.as_slice(), width)?
            };
            return Ok(SubPlan {
                sources: AVec::new(),
                items: AVec::new(),
                filter: None,
                group_by: AVec::new(),
                having: None,
                order_by: AVec::new(),
                distinct: false,
                limit: stmt.limit,
                offset: stmt.offset,
                has_agg: false,
                compound,
                values: rows_r,
                comp_order,
                result_names,
                result_affs,
                order_names,
            });
        }
        let mut descs: AVec<SourceDesc> = AVec::new();
        let mut offsets: AVec<usize> = AVec::new();
        let mut subs: AVec<SubSource> = AVec::new();
        let mut total = base_width;
        for s in stmt.from.iter() {
            // A derived table (v0.9): planned against the
            // enclosing scopes ONLY — the pin has no LATERAL, so
            // the subquery cannot see its sibling sources, but
            // correlation with enclosing queries works.
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &s.derived {
                    let dplan = self.plan_subquery(d, arena, enclosing)?;
                    let w = dplan.width();
                    offsets.push(total);
                    let mut cols = AVec::new();
                    let mut affs = AVec::new();
                    let mut defaults = AVec::new();
                    for (nm, aff) in dplan.result_names.iter().zip(dplan.result_affs.iter()) {
                        cols.push((nm.clone(), aff.unwrap_or(crate::record::Affinity::Blob)));
                        affs.push(crate::record::Affinity::Blob);
                        defaults.push(OwnedValue::Null);
                    }
                    let qual = match s.alias {
                        Some(a) => copy_vec(handle_bytes(arena, a)),
                        None => AVec::new(),
                    };
                    descs.push(SourceDesc {
                        qualifier: qual,
                        columns: cols,
                        rowid_alias: None,
                        has_rowid: false,
                    });
                    subs.push(SubSource {
                        root: 0,
                        kind: s.kind,
                        on: None,
                        rowid_alias: None,
                        ncols: w,
                        affs,
                        defaults,
                        derived: Some(Box::new(dplan)),
                    });
                    total += w;
                    continue;
                }
            }
            let tname = handle_bytes(arena, s.table);
            let t = match self.catalog.find(&self.catalog_arena, tname) {
                Some(t) => t,
                None => {
                    if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                        // Views are inert (SPEC §2): a subquery
                        // cannot select from one either.
                        return Err(Error::Unsupported);
                    }
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    });
                }
            };
            offsets.push(total);
            let mut cols = AVec::new();
            let mut affs = AVec::new();
            let mut defaults = AVec::new();
            for c in t.columns.iter() {
                let mut nm = AVec::new();
                nm.extend_from_slice(handle_bytes(&self.catalog_arena, c.name));
                cols.push((nm, c.affinity));
                affs.push(c.affinity);
                defaults.push(literal_owned(&self.catalog_arena, c.default));
            }
            let qual = match s.alias {
                Some(a) => copy_vec(handle_bytes(arena, a)),
                None => copy_vec(tname),
            };
            descs.push(SourceDesc {
                qualifier: qual,
                columns: cols,
                rowid_alias: t.rowid_alias,
                has_rowid: true,
            });
            subs.push(SubSource {
                root: t.root,
                kind: s.kind,
                on: None,
                rowid_alias: t.rowid_alias,
                ncols: t.columns.len(),
                affs,
                defaults,
                derived: None,
            });
            total += t.columns.len();
        }
        let cur = ScopeGroup {
            sources: descs,
            offsets,
            first_src: base_src,
        };
        let mut planner = self.sub_planner(arena);
        let filter = match &stmt.filter {
            None => None,
            Some(e) => {
                let r = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                Some(r)
            }
        };
        for (i, s) in stmt.from.iter().enumerate() {
            if let Some(on) = &s.on {
                let r = resolve_scoped::<S::Error>(on, arena, &cur, enclosing, &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                subs.as_mut_slice()[i].on = Some(r);
            }
        }
        let mut items: AVec<RExpr> = AVec::new();
        let mut item_aliases: AVec<Option<AVec<u8>>> = AVec::new();
        let mut result_names: AVec<AVec<u8>> = AVec::new();
        let mut result_affs: AVec<Option<crate::record::Affinity>> = AVec::new();
        let mut order_names: AVec<crate::query::OrderName> = AVec::new();
        for it in stmt.items.iter() {
            match it {
                SelectItem::Star => {
                    for (si, src) in cur.sources.iter().enumerate() {
                        for (ci, (cname, aff)) in src.columns.iter().enumerate() {
                            items.push(if Some(ci) == src.rowid_alias {
                                RExpr::Rowid(cur.first_src + si)
                            } else {
                                RExpr::Column(cur.offsets.as_slice()[si] + ci, *aff)
                            });
                            item_aliases.push(None);
                            result_names.push(cname.clone());
                            result_affs.push(Some(if Some(ci) == src.rowid_alias {
                                crate::record::Affinity::Integer
                            } else {
                                *aff
                            }));
                            order_names.push((Some(src.qualifier.clone()), cname.clone()));
                        }
                    }
                }
                SelectItem::TableStar(q) => {
                    let qb = handle_bytes(arena, *q);
                    let mut hit = false;
                    for (si, src) in cur.sources.iter().enumerate() {
                        if eq_ci(src.qualifier.as_slice(), qb) {
                            hit = true;
                            for (ci, (cname, aff)) in src.columns.iter().enumerate() {
                                items.push(if Some(ci) == src.rowid_alias {
                                    RExpr::Rowid(cur.first_src + si)
                                } else {
                                    RExpr::Column(cur.offsets.as_slice()[si] + ci, *aff)
                                });
                                item_aliases.push(None);
                                result_names.push(cname.clone());
                                result_affs.push(Some(if Some(ci) == src.rowid_alias {
                                    crate::record::Affinity::Integer
                                } else {
                                    *aff
                                }));
                                order_names.push((Some(src.qualifier.clone()), cname.clone()));
                            }
                        }
                    }
                    if !hit {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownColumn,
                        });
                    }
                }
                SelectItem::Expr(e, alias, text) => {
                    let r = resolve_scoped::<S::Error>(e, arena, &cur, enclosing, &mut planner)?;
                    let (nm, okey) = item_name_keys(arena, e, alias, text, &cur);
                    result_names.push(nm);
                    order_names.push(okey);
                    result_affs.push(crate::query::item_result_affinity(&r));
                    items.push(r);
                    item_aliases.push(alias.map(|a| copy_vec(handle_bytes(arena, a))));
                }
            }
        }
        let mut group_by = AVec::new();
        for e in stmt.group_by.iter() {
            group_by.push(self.resolve_term(
                e,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                enclosing,
                &mut planner,
            )?);
        }
        let having = match &stmt.having {
            None => None,
            Some(e) => Some(self.resolve_term(
                e,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                enclosing,
                &mut planner,
            )?),
        };
        // A compound head's ORDER BY resolves against the
        // combined result (comp_order, below), not this core's
        // rows, so the term loop is skipped for it.
        #[cfg(feature = "v09")]
        let compound_head = !stmt.compound.is_empty();
        #[cfg(not(feature = "v09"))]
        let compound_head = false;
        let mut order_by: AVec<(RExpr, bool)> = AVec::new();
        if !compound_head {
            for (e, desc) in stmt.order_by.iter() {
                order_by.push((
                    self.resolve_term(
                        e,
                        arena,
                        items.as_slice(),
                        item_aliases.as_slice(),
                        &cur,
                        enclosing,
                        &mut planner,
                    )?,
                    *desc,
                ));
            }
        }
        if items.iter().any(sub_in_agg_args)
            || having.as_ref().is_some_and(sub_in_agg_args)
            || order_by.iter().any(|(e, _)| sub_in_agg_args(e))
        {
            return Err(Error::Unsupported);
        }
        let has_agg = !group_by.is_empty()
            || items.iter().any(has_agg)
            || having.as_ref().is_some_and(has_agg)
            || order_by.iter().any(|(e, _)| has_agg(e));
        #[cfg(feature = "v09")]
        let (compound, comp_order) = if stmt.compound.is_empty() {
            (AVec::new(), AVec::new())
        } else {
            self.plan_sub_compound(stmt, arena, enclosing, order_names.as_slice(), items.len())?
        };
        #[cfg(not(feature = "v09"))]
        let (compound, comp_order): SubCompoundParts = (AVec::new(), AVec::new());
        Ok(SubPlan {
            sources: subs,
            items,
            filter,
            group_by,
            having,
            order_by,
            distinct: stmt.distinct,
            limit: stmt.limit,
            offset: stmt.offset,
            has_agg,
            compound,
            values: AVec::new(),
            comp_order,
            result_names,
            result_affs,
            order_names,
        })
    }

    /// Plan one subquery statement's compound tail (v0.9):
    /// each tail core plans against the same enclosing scopes;
    /// all cores must match the head's width (the pin: "SELECTs
    /// to the left and right of UNION do not have the same
    /// number of result columns"). The statement's ORDER BY
    /// resolves to output-column indices over the union of all
    /// cores' result names.
    #[cfg(feature = "v09")]
    fn plan_sub_compound(
        &self,
        stmt: &SelectStmt,
        arena: &Arena,
        enclosing: &[crate::query::ScopeGroup],
        head_names: &[crate::query::OrderName],
        width: usize,
    ) -> Result<SubCompoundParts, S::Error> {
        let mut tails: AVec<(crate::sql::CompoundOp, Box<crate::query::SubPlan>)> = AVec::new();
        for (op, core) in stmt.compound.iter() {
            let p = self.plan_subquery(core, arena, enclosing)?;
            if p.width() != width {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            tails.push((*op, Box::new(p)));
        }
        let mut all_names: AVec<&[crate::query::OrderName]> = AVec::new();
        all_names.push(head_names);
        for (_, p) in tails.iter() {
            all_names.push(p.order_names.as_slice());
        }
        let comp_order = resolve_compound_order::<S::Error>(
            stmt.order_by.as_slice(),
            arena,
            all_names.as_slice(),
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
        head_names: &[crate::query::OrderName],
        width: usize,
    ) -> Result<QueryCompoundParts, S::Error> {
        let mut tails: AVec<(crate::sql::CompoundOp, Box<QueryPlan>)> = AVec::new();
        for (op, core) in stmt.compound.iter() {
            let p = self.plan_query(core, arena)?;
            if p.width != width {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            tails.push((*op, Box::new(p)));
        }
        let mut all_names: AVec<&[crate::query::OrderName]> = AVec::new();
        all_names.push(head_names);
        for (_, p) in tails.iter() {
            all_names.push(p.order_names.as_slice());
        }
        let comp_order = resolve_compound_order::<S::Error>(
            stmt.order_by.as_slice(),
            arena,
            all_names.as_slice(),
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
        use crate::query::{has_agg, resolve_scoped};
        let (sources, offsets) = self.single_source(t, qualifier);
        let cur = crate::query::ScopeGroup {
            sources,
            offsets,
            first_src: 0,
        };
        let mut planner = self.sub_planner(arena);
        let mut out = AVec::new();
        for it in items.iter() {
            match it {
                SelectItem::Star => {
                    let src = &cur.sources.as_slice()[0];
                    for (ci, (_, aff)) in src.columns.iter().enumerate() {
                        out.push(if Some(ci) == src.rowid_alias {
                            RExpr::Rowid(0)
                        } else {
                            RExpr::Column(ci, *aff)
                        });
                    }
                }
                SelectItem::TableStar(q) => {
                    let qb = handle_bytes(arena, *q);
                    let src = &cur.sources.as_slice()[0];
                    if !eq_ci(src.qualifier.as_slice(), qb) {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownColumn,
                        });
                    }
                    for (ci, (_, aff)) in src.columns.iter().enumerate() {
                        out.push(if Some(ci) == src.rowid_alias {
                            RExpr::Rowid(0)
                        } else {
                            RExpr::Column(ci, *aff)
                        });
                    }
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
            let cur = ScopeGroup {
                sources,
                offsets,
                first_src: 0,
            };
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
                        keyset.push(match c.col {
                            None => Target::Rowid,
                            Some(i) if Some(i) == t.rowid_alias => Target::Rowid,
                            Some(i) => Target::Col(i),
                        });
                    }
                    if keyset.len() == cols.len()
                        && keyset.iter().all(|k| cols.iter().any(|c| c == k))
                    {
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
                let cur = ScopeGroup {
                    sources,
                    offsets,
                    first_src: 0,
                };
                let mut ex_cols = AVec::new();
                for c in t.columns.iter() {
                    ex_cols.push((
                        copy_vec(handle_bytes(&self.catalog_arena, c.name)),
                        c.affinity,
                    ));
                }
                let mut ex_sources = AVec::new();
                ex_sources.push(SourceDesc {
                    qualifier: copy_vec(b"excluded"),
                    columns: ex_cols,
                    rowid_alias: t.rowid_alias,
                    has_rowid: true,
                });
                let mut ex_offsets = AVec::new();
                ex_offsets.push(t.columns.len());
                let enclosing = [ScopeGroup {
                    sources: ex_sources,
                    offsets: ex_offsets,
                    first_src: 1,
                }];
                let mut planner = self.sub_planner(arena);
                let mut sets = AVec::new();
                for (colname, e) in assignments.iter() {
                    let target = canon(self, handle_bytes(arena, *colname))?;
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
        let tname = handle_bytes(arena, *table);
        let t = self.write_table(arena, table)?;
        let qualifier: &[u8] = match alias {
            Some(a) => handle_bytes(arena, *a),
            None => tname,
        };
        // Combined scope: target first, then the FROM sources
        // (empty without the v07 parser, which never fills
        // `from`); SourcePlans are kept for the executor.
        let (mut sources, mut offsets) = self.single_source(t, qualifier);
        let mut from_plans: AVec<SourcePlan> = AVec::new();
        let mut total = t.columns.len();
        for s in from.iter() {
            // A derived table in UPDATE ... FROM (v0.9):
            // planned as a subquery against no enclosing scope
            // (it cannot see the target table — no LATERAL).
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &s.derived {
                    let dplan = self.plan_subquery(d, arena, &[])?;
                    let w = dplan.width();
                    offsets.push(total);
                    let mut cols = AVec::new();
                    let mut affs = AVec::new();
                    let mut defaults = AVec::new();
                    for (nm, aff) in dplan.result_names.iter().zip(dplan.result_affs.iter()) {
                        cols.push((nm.clone(), aff.unwrap_or(crate::record::Affinity::Blob)));
                        affs.push(crate::record::Affinity::Blob);
                        defaults.push(OwnedValue::Null);
                    }
                    let qual = match s.alias {
                        Some(a) => copy_vec(handle_bytes(arena, a)),
                        None => AVec::new(),
                    };
                    sources.push(crate::query::SourceDesc {
                        qualifier: qual,
                        columns: cols,
                        rowid_alias: None,
                        has_rowid: false,
                    });
                    from_plans.push(SourcePlan {
                        root: 0,
                        kind: s.kind,
                        on: None,
                        rowid_alias: None,
                        ncols: w,
                        affs,
                        defaults,
                        derived: Some(Box::new(dplan)),
                    });
                    total += w;
                    continue;
                }
            }
            let fname = handle_bytes(arena, s.table);
            let ft = match self.catalog.find(&self.catalog_arena, fname) {
                Some(ft) => ft,
                None => {
                    if self.catalog.find_view(&self.catalog_arena, fname).is_some() {
                        return Err(Error::Unsupported);
                    }
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    });
                }
            };
            offsets.push(total);
            let mut cols = AVec::new();
            let mut affs = AVec::new();
            let mut defaults = AVec::new();
            for c in ft.columns.iter() {
                cols.push((
                    copy_vec(handle_bytes(&self.catalog_arena, c.name)),
                    c.affinity,
                ));
                affs.push(c.affinity);
                defaults.push(literal_owned(&self.catalog_arena, c.default));
            }
            let qual = match s.alias {
                Some(a) => copy_vec(handle_bytes(arena, a)),
                None => copy_vec(fname),
            };
            sources.push(crate::query::SourceDesc {
                qualifier: qual,
                columns: cols,
                rowid_alias: ft.rowid_alias,
                has_rowid: true,
            });
            from_plans.push(SourcePlan {
                root: ft.root,
                kind: s.kind,
                on: None,
                rowid_alias: ft.rowid_alias,
                ncols: ft.columns.len(),
                affs,
                defaults,
                derived: None,
            });
            total += ft.columns.len();
        }
        let cur = crate::query::ScopeGroup {
            sources,
            offsets,
            first_src: 0,
        };
        let mut planner = self.sub_planner(arena);
        for (i, s) in from.iter().enumerate() {
            if let Some(on) = &s.on {
                let r = resolve_scoped::<S::Error>(on, arena, &cur, &[], &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                from_plans.as_mut_slice()[i].on = Some(r);
            }
        }
        let mut sets = AVec::new();
        for (colname, e) in assignments.iter() {
            let target = self.resolve_target(t, handle_bytes(arena, *colname))?;
            let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
            if has_agg(&r) {
                return Err(Error::Unsupported);
            }
            sets.push((target, r));
        }
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
        let mut not_null = AVec::new();
        for c in t.columns.iter() {
            defaults.push(literal_owned(&self.catalog_arena, c.default));
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
        let tname = handle_bytes(arena, *table);
        let t = self.write_table(arena, table)?;
        let (sources, offsets) = self.single_source(t, tname);
        let cur = crate::query::ScopeGroup {
            sources,
            offsets,
            first_src: 0,
        };
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
            defaults.push(literal_owned(&self.catalog_arena, c.default));
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
            Some(t) => t,
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
            defaults.push(literal_owned(&self.catalog_arena, c.default));
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
    #[cfg(feature = "v04")]
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
        use crate::query::{SourceDesc, has_agg, resolve_scoped, sub_in_agg_args};
        // A VALUES core (v0.9): no sources; rows resolve
        // against the empty scope.
        #[cfg(feature = "v09")]
        if !stmt.values.is_empty() {
            let cur = crate::query::ScopeGroup {
                sources: AVec::new(),
                offsets: AVec::new(),
                first_src: 0,
            };
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
                for e in r.iter() {
                    let re = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                    if has_agg(&re) {
                        return Err(Error::Unsupported);
                    }
                    rr.push(re);
                }
                rows_r.push(rr);
            }
            let width = width.unwrap_or(0);
            let mut order_names: AVec<crate::query::OrderName> = AVec::new();
            for i in 0..width {
                order_names.push((None, values_col_name(i)));
            }
            let (compound, comp_order) = if stmt.compound.is_empty() {
                (AVec::new(), AVec::new())
            } else {
                self.plan_query_compound(stmt, arena, order_names.as_slice(), width)?
            };
            return Ok(QueryPlan {
                sources: AVec::new(),
                items: AVec::new(),
                filter: None,
                group_by: AVec::new(),
                having: None,
                order_by: AVec::new(),
                distinct: false,
                limit: stmt.limit,
                offset: stmt.offset,
                has_agg: false,
                width,
                compound,
                values: rows_r,
                comp_order,
                order_names,
            });
        }
        let mut sources: AVec<SourceDesc> = AVec::new();
        let mut offsets: AVec<usize> = AVec::new();
        let mut src_plans: AVec<SourcePlan> = AVec::new();
        let mut total = 0usize;
        for s in stmt.from.iter() {
            // A derived table (v0.9): planned as a subquery
            // against no enclosing scope (a statement-level
            // FROM subquery sees no siblings — no LATERAL).
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &s.derived {
                    let dplan = self.plan_subquery(d, arena, &[])?;
                    let w = dplan.width();
                    offsets.push(total);
                    let mut cols = AVec::new();
                    let mut affs = AVec::new();
                    let mut defaults = AVec::new();
                    for (nm, aff) in dplan.result_names.iter().zip(dplan.result_affs.iter()) {
                        cols.push((nm.clone(), aff.unwrap_or(crate::record::Affinity::Blob)));
                        affs.push(crate::record::Affinity::Blob);
                        defaults.push(OwnedValue::Null);
                    }
                    let qual = match s.alias {
                        Some(a) => copy_vec(handle_bytes(arena, a)),
                        None => AVec::new(),
                    };
                    sources.push(SourceDesc {
                        qualifier: qual,
                        columns: cols,
                        rowid_alias: None,
                        has_rowid: false,
                    });
                    src_plans.push(SourcePlan {
                        root: 0,
                        kind: s.kind,
                        on: None,
                        rowid_alias: None,
                        ncols: w,
                        affs,
                        defaults,
                        derived: Some(Box::new(dplan)),
                    });
                    total += w;
                    continue;
                }
            }
            let tname = handle_bytes(arena, s.table);
            let t = match self.catalog.find(&self.catalog_arena, tname) {
                Some(t) => t,
                None => {
                    if self.catalog.find_view(&self.catalog_arena, tname).is_some() {
                        // Views are inert in v0.4 (SPEC §2):
                        // stored, never executed.
                        return Err(Error::Unsupported);
                    }
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    });
                }
            };
            offsets.push(total);
            let mut cols = AVec::new();
            let mut affs = AVec::new();
            let mut defaults = AVec::new();
            for c in t.columns.iter() {
                let mut nm = AVec::new();
                nm.extend_from_slice(handle_bytes(&self.catalog_arena, c.name));
                cols.push((nm, c.affinity));
                affs.push(c.affinity);
                defaults.push(literal_owned(&self.catalog_arena, c.default));
            }
            let qual = match s.alias {
                Some(a) => copy_vec(handle_bytes(arena, a)),
                None => copy_vec(tname),
            };
            sources.push(SourceDesc {
                qualifier: qual,
                columns: cols,
                rowid_alias: t.rowid_alias,
                has_rowid: true,
            });
            src_plans.push(SourcePlan {
                root: t.root,
                kind: s.kind,
                on: None,
                rowid_alias: t.rowid_alias,
                ncols: t.columns.len(),
                affs,
                defaults,
                derived: None,
            });
            total += t.columns.len();
        }
        let cur = crate::query::ScopeGroup {
            sources: sources.clone(),
            offsets: offsets.clone(),
            first_src: 0,
        };
        let mut planner = self.sub_planner(arena);
        let filter = match &stmt.filter {
            None => None,
            Some(e) => {
                let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                Some(r)
            }
        };
        for (i, s) in stmt.from.iter().enumerate() {
            if let Some(on) = &s.on {
                let r = resolve_scoped::<S::Error>(on, arena, &cur, &[], &mut planner)?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                src_plans.as_mut_slice()[i].on = Some(r);
            }
        }
        // Items, with star expansion; aliases are remembered for
        // ORDER BY resolution.
        let mut items: AVec<RExpr> = AVec::new();
        let mut item_aliases: AVec<Option<AVec<u8>>> = AVec::new();
        let mut order_names: AVec<crate::query::OrderName> = AVec::new();
        for it in stmt.items.iter() {
            match it {
                SelectItem::Star => {
                    for (si, src) in sources.iter().enumerate() {
                        for (ci, (cname, aff)) in src.columns.iter().enumerate() {
                            items.push(if Some(ci) == src.rowid_alias {
                                RExpr::Rowid(si)
                            } else {
                                RExpr::Column(offsets.as_slice()[si] + ci, *aff)
                            });
                            item_aliases.push(None);
                            order_names.push((Some(src.qualifier.clone()), cname.clone()));
                            let _ = aff;
                        }
                    }
                }
                SelectItem::TableStar(q) => {
                    let qb = handle_bytes(arena, *q);
                    let mut hit = false;
                    for (si, src) in sources.iter().enumerate() {
                        if eq_ci(src.qualifier.as_slice(), qb) {
                            hit = true;
                            for (ci, (cname, aff)) in src.columns.iter().enumerate() {
                                items.push(if Some(ci) == src.rowid_alias {
                                    RExpr::Rowid(si)
                                } else {
                                    RExpr::Column(offsets.as_slice()[si] + ci, *aff)
                                });
                                item_aliases.push(None);
                                order_names.push((Some(src.qualifier.clone()), cname.clone()));
                                let _ = aff;
                            }
                        }
                    }
                    if !hit {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownColumn,
                        });
                    }
                }
                SelectItem::Expr(e, alias, text) => {
                    items.push(resolve_scoped::<S::Error>(
                        e,
                        arena,
                        &cur,
                        &[],
                        &mut planner,
                    )?);
                    let (_, okey) = item_name_keys(arena, e, alias, text, &cur);
                    order_names.push(okey);
                    item_aliases.push(alias.map(|a| copy_vec(handle_bytes(arena, a))));
                }
            }
        }
        let width = items.len();
        // GROUP BY / HAVING / ORDER BY terms first resolve as an
        // output ordinal or alias (SQLite's rule), substituting
        // the item expression itself, so evaluation never needs
        // an output-row context.
        let mut group_by = AVec::new();
        for e in stmt.group_by.iter() {
            group_by.push(self.resolve_term(
                e,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                &[],
                &mut planner,
            )?);
        }
        let having = match &stmt.having {
            None => None,
            Some(e) => Some(self.resolve_term(
                e,
                arena,
                items.as_slice(),
                item_aliases.as_slice(),
                &cur,
                &[],
                &mut planner,
            )?),
        };
        // A compound head's ORDER BY resolves against the
        // combined result (comp_order, below), not this core's
        // rows, so the term loop is skipped for it.
        #[cfg(feature = "v09")]
        let compound_head = !stmt.compound.is_empty();
        #[cfg(not(feature = "v09"))]
        let compound_head = false;
        let mut order_by: AVec<(RExpr, bool)> = AVec::new();
        if !compound_head {
            for (e, desc) in stmt.order_by.iter() {
                order_by.push((
                    self.resolve_term(
                        e,
                        arena,
                        items.as_slice(),
                        item_aliases.as_slice(),
                        &cur,
                        &[],
                        &mut planner,
                    )?,
                    *desc,
                ));
            }
        }
        // A subquery inside an aggregate's arguments would need
        // per-group-row execution; v0.6 refuses it at plan time.
        if items.iter().any(sub_in_agg_args)
            || having.as_ref().is_some_and(sub_in_agg_args)
            || order_by.iter().any(|(e, _)| sub_in_agg_args(e))
        {
            return Err(Error::Unsupported);
        }
        let has_agg = !group_by.is_empty()
            || items.iter().any(has_agg)
            || having.as_ref().is_some_and(has_agg)
            || order_by.iter().any(|(e, _)| has_agg(e));
        #[cfg(feature = "v09")]
        let (compound, comp_order) = if stmt.compound.is_empty() {
            (AVec::new(), AVec::new())
        } else {
            self.plan_query_compound(stmt, arena, order_names.as_slice(), width)?
        };
        #[cfg(not(feature = "v09"))]
        let (compound, comp_order): QueryCompoundParts = (AVec::new(), AVec::new());
        Ok(QueryPlan {
            sources: src_plans,
            items,
            filter,
            group_by,
            having,
            order_by,
            distinct: stmt.distinct,
            limit: stmt.limit,
            offset: stmt.offset,
            has_agg,
            width,
            compound,
            values: AVec::new(),
            comp_order,
            order_names,
        })
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
        let mut chosen: Option<(u32, bool, AVec<bool>)> = None;
        for ix in self.catalog.indexes_for(&self.catalog_arena, tname) {
            if ix.opaque {
                continue;
            }
            let Some(first) = ix.columns.as_slice().first() else {
                continue;
            };
            if first.col != Some(col) {
                continue;
            }
            let mut desc = AVec::new();
            for c in ix.columns.iter() {
                desc.push(c.desc);
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
            out.push(TableMeta {
                name: handle_bytes(&self.catalog_arena, t.name),
                sql: handle_bytes(&self.catalog_arena, t.sql),
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

    /// Mutable pager access for tools/tests driving the layers
    /// below the session API (cursor-level scans, §4.1 gates).
    pub fn pager_mut(&mut self) -> &mut Pager<S> {
        &mut self.pager
    }

    /// The underlying storage (for tests/tools that snapshot bytes).
    pub fn storage(&self) -> &S {
        self.pager.storage()
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
#[derive(Debug, Clone, Copy)]
pub struct TableMeta<'a> {
    pub name: &'a [u8],
    pub sql: &'a [u8],
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

pub struct Statement<'c, S: Storage> {
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

fn copy_vec(b: &[u8]) -> AVec<u8> {
    let mut v = AVec::new();
    for x in b {
        v.push(*x);
    }
    v
}

/// The stored schema text for a CREATE statement: the statement
/// as issued, minus any trailing semicolon/whitespace (SQLite's
/// convention).
fn trimmed_sql_text(raw: &[u8]) -> AVec<u8> {
    let mut end = raw.len();
    while end > 0 && (raw[end - 1] == b';' || (raw[end - 1] as char).is_whitespace()) {
        end -= 1;
    }
    copy_vec(&raw[..end])
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
) -> Result<AVec<OwnedValue>, E> {
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
fn resolve_not_null<E>(
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
            ConflictAction::Abort => {
                return Err(Error::Schema {
                    kind: SchemaKind::NotNullViolation,
                });
            }
            ConflictAction::Ignore => return Ok(false),
            ConflictAction::Replace => {
                if matches!(defaults[i], OwnedValue::Null) {
                    return Err(Error::Schema {
                        kind: SchemaKind::NotNullViolation,
                    });
                }
                row[i] = apply_affinity(defaults[i].clone(), affs[i]);
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

/// Whether an upsert clause arbitrates one found conflict
/// (v0.7): no target arbitrates everything; a target arbitrates
/// a rowid conflict only when it is exactly `[Rowid]`, and an
/// index conflict only when the index's canonical key set is
/// exactly the target set.
#[cfg(feature = "v07")]
fn upsert_arbitrates(up: &UpsertPlan, by_rowid: bool, keyset: &Option<AVec<Target>>) -> bool {
    let Some(target) = &up.target else {
        return true;
    };
    if by_rowid {
        return target.len() == 1 && target.as_slice()[0] == Target::Rowid;
    }
    match keyset {
        Some(ks) => ks.len() == target.len() && ks.iter().all(|k| target.iter().any(|t| t == k)),
        None => false,
    }
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
            let b = handle_bytes(arena, h);
            let Ok(s) = core::str::from_utf8(b) else {
                return Access::Empty;
            };
            let s = s.trim();
            if let Ok(i) = s.parse::<i64>() {
                return Access::Rowid(i);
            }
            match s.parse::<f64>() {
                Ok(f) => match integral_f64(f) {
                    Some(i) => Access::Rowid(i),
                    None => Access::Empty,
                },
                Err(_) => Access::Empty,
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

impl<'c, S: Storage> Statement<'c, S> {
    /// Execute/advance the statement by one step.
    pub async fn step(&mut self) -> Result<Step, S::Error> {
        let ddl_or_txn = matches!(self.plan, Plan::CreateTable | Plan::Commit | Plan::Rollback)
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
        let r = match &self.plan {
            Plan::CreateTable => self.step_create().await,
            #[cfg(feature = "v04")]
            Plan::Query(_) => self.step_query().await,
            #[cfg(feature = "v04")]
            Plan::AlterAdd { .. } => self.step_alter().await,
            #[cfg(feature = "v04")]
            Plan::CreateView => self.step_create_view().await,
            #[cfg(feature = "v04")]
            Plan::DropView { .. } => self.step_drop_view().await,
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
            Plan::Commit => self.step_commit().await,
            Plan::Rollback => self.step_rollback().await,
        };
        // Keep the connection's catalog in step with the schema
        // cookie after DDL / transaction boundaries, so metadata
        // readers (tables(), indexes()) see the new state without
        // waiting for the next prepare (§5.5).
        if ddl_or_txn && r.is_ok() && self.conn.pager.schema_cookie() != self.conn.catalog.cookie {
            self.conn.rebuild_catalog().await?;
        }
        r
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
        self.conn.pager.drive_commit().await?;
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
        if self
            .conn
            .catalog
            .find(&self.conn.catalog_arena, name_b.as_slice())
            .is_some()
        {
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
        // trailing semicolon/whitespace (SQLite's convention).
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text(raw);
        let r = self
            .exec_create(name_b.as_slice(), sql_text.as_slice())
            .await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                // §7.5.4: a failed write leaves no trace. Inside
                // an explicit transaction the rollback ends the
                // whole bracket (v0.2 note: stricter than SQLite's
                // statement-level abort — see STATUS).
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
                Err(e)
            }
        }
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
        let rowid = match btree::seek_max_rowid(pager, 1).await? {
            Some(m) => m + 1,
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

    async fn exec_create(&mut self, name: &[u8], sql_text: &[u8]) -> Result<(), S::Error> {
        let (root, _) = self.conn.pager.alloc_page().await?;
        btree::create_empty_root(&mut self.conn.pager, root).await?;
        self.insert_schema_row(b"table", name, name, root, Some(sql_text))
            .await?;
        // Implicit indexes for the table's UNIQUE / PK constraint
        // sets (v0.3), in statement order, exactly as SQLite
        // creates them.
        #[cfg(feature = "indexes")]
        self.create_auto_indexes(name).await?;
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
                        let mut s = 0;
                        let mut e = decl.len();
                        while s < e && (decl[s] as char).is_whitespace() {
                            s += 1;
                        }
                        while e > s && (decl[e - 1] as char).is_whitespace() {
                            e -= 1;
                        }
                        alias = !c.pk_desc && eq_ci(&decl[s..e], b"INTEGER");
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
        // Index names share the schema namespace with tables.
        let taken = self
            .conn
            .catalog
            .find_index(&self.conn.catalog_arena, name_b.as_slice())
            .is_some()
            || self
                .conn
                .catalog
                .find(&self.conn.catalog_arena, name_b.as_slice())
                .is_some();
        if taken {
            if *if_not_exists {
                return Ok(());
            }
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateIndex,
            });
        }
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text(raw);
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
        } = &self.plan
        else {
            unreachable!()
        };
        let (table_root, rowid_alias, ncols, unique) = (*table_root, *rowid_alias, *ncols, *unique);
        let defaults = defaults.clone();
        let cols = cols.clone();
        let mut desc = AVec::new();
        for c in cols.iter() {
            desc.push(c.desc);
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
                && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, name_b.as_slice()))
            {
                schema_rowid = Some(rowid);
                break;
            }
        }
        let Some(schema_rowid) = schema_rowid else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownIndex,
            });
        };
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
            _ => false,
        }
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
    #[cfg(feature = "v07")]
    async fn eval_returning(
        &mut self,
        exprs: &[RExpr],
        rd: &RowData,
    ) -> Result<AVec<OwnedValue>, S::Error> {
        let mut out = AVec::new();
        for e in exprs {
            out.push(self.eval_row_sub(e, rd).await?);
        }
        Ok(out)
    }

    /// Delete one row and every index entry it owns (v0.7):
    /// the shared tail of OR REPLACE and upsert conflict
    /// handling. A rowid that is already gone is a no-op.
    #[cfg(feature = "v07")]
    async fn delete_row_completely(
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
            let Some(payload) = btree::seek_rowid(&mut self.conn.pager, root, rowid).await? else {
                return Ok(());
            };
            for ix in indexes.iter() {
                let key = index_key_for_row(
                    ix.cols.as_slice(),
                    rowid_alias,
                    defaults,
                    payload.as_slice(),
                    ncols,
                    rowid,
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
                Err(e) => {
                    let _ = self.conn.pager.rollback().await;
                    self.conn.explicit_txn = false;
                    return Err(e);
                }
            }
        }
        #[cfg(feature = "v07")]
        if self.has_returning() {
            return Ok(self.serve_returning());
        }
        let r = self.exec_insert().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                // Same abort rule as CREATE (see step_create).
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
                Err(e)
            }
        }
    }

    async fn exec_insert(&mut self) -> Result<(), S::Error> {
        // The full row source is materialised BEFORE the first
        // row is inserted (v0.8): an INSERT ... SELECT reading
        // its own target terminates on the pre-statement rows,
        // and subqueries in VALUES observe the pre-statement
        // table for every row — both oracle-probed.
        let rows = self.insert_source_rows().await?;
        for values in rows.iter() {
            self.exec_insert_row(values.clone()).await?;
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
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
            root,
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
        let mut row: AVec<OwnedValue> = AVec::new();
        for c in cols.iter() {
            // An omitted column's default is a stored value
            // like any other: the column's affinity applies
            // (v0.10; the pin stores DEFAULT '7' in an INT
            // column as the integer 7).
            row.push(apply_affinity(c.default.clone(), c.affinity));
        }
        let mut explicit_rowid: Option<i64> = None;
        for (target, v) in targets.iter().zip(values.iter()) {
            let v = v.clone();
            match target {
                Target::Rowid => match v {
                    OwnedValue::Integer(i) => explicit_rowid = Some(i),
                    OwnedValue::Null => {}
                    _ => {
                        return Err(Error::Schema {
                            kind: SchemaKind::DatatypeMismatch,
                        });
                    }
                },
                Target::Col(i) => {
                    let i = *i;
                    if Some(i) == *rowid_alias {
                        match v {
                            OwnedValue::Integer(x) => explicit_rowid = Some(x),
                            OwnedValue::Null => {}
                            _ => {
                                return Err(Error::Schema {
                                    kind: SchemaKind::DatatypeMismatch,
                                });
                            }
                        }
                        // The alias column's record field stays NULL.
                    } else {
                        row.as_mut_slice()[i] = apply_affinity(v, cols.as_slice()[i].affinity);
                    }
                }
            }
        }
        // v0.10 NOT NULL: the proposed row is resolved under
        // the statement's conflict action before the rowid is
        // assigned and before any uniqueness check (the pin's
        // order) — and, for an upsert, before arbitration: a
        // violating proposal under OR IGNORE is skipped
        // wholesale (its DO UPDATE never runs), under OR
        // REPLACE it is substituted and then arbitrated.
        {
            let action = match &self.plan {
                Plan::Insert { conflict, .. } => *conflict,
                _ => unreachable!(),
            };
            let mut nn_flags = AVec::new();
            let mut nn_defaults = AVec::new();
            let mut nn_affs = AVec::new();
            for c in cols.iter() {
                nn_flags.push(c.not_null);
                nn_defaults.push(c.default.clone());
                nn_affs.push(c.affinity);
            }
            if !resolve_not_null::<S::Error>(
                row.as_mut_slice(),
                nn_flags.as_slice(),
                nn_defaults.as_slice(),
                nn_affs.as_slice(),
                *rowid_alias,
                action,
            )? {
                return Ok(()); // OR IGNORE: skip this row.
            }
        }
        let rowid = {
            let pager = &mut self.conn.pager;
            match explicit_rowid {
                Some(r) => r,
                None => match btree::seek_max_rowid(pager, *root).await? {
                    Some(m) => m.checked_add(1).ok_or(Error::Schema {
                        kind: SchemaKind::DatatypeMismatch,
                    })?,
                    None => 1,
                },
            }
        };
        // v0.7: a non-default conflict clause or an upsert clause
        // routes the row through the conflict-aware path, which
        // finds the conflicting rows first and then replaces,
        // skips, or updates instead of failing.
        #[cfg(feature = "v07")]
        {
            let routed = match &self.plan {
                Plan::Insert {
                    conflict, upsert, ..
                } => *conflict != ConflictAction::Abort || upsert.is_some(),
                _ => unreachable!(),
            };
            if routed {
                return self.exec_insert_row_v07(row, rowid).await;
            }
        }
        let pager = &mut self.conn.pager;
        let mut vals: AVec<Value<'_>> = AVec::new();
        for (i, v) in row.iter().enumerate() {
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
        let mut pending_keys: AVec<(u32, AVec<bool>, AVec<OwnedValue>)> = AVec::new();
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
        btree::btree_insert(pager, *root, rowid, rec.as_slice()).await?;
        // v0.3 index work, part 2: maintain every index.
        #[cfg(feature = "indexes")]
        for (ix_root, desc, key) in pending_keys.iter() {
            let payload = encode_index_payload(key.as_slice(), rowid);
            btree::index_insert(pager, *ix_root, payload.as_slice(), desc.as_slice()).await?;
        }
        #[cfg(feature = "v07")]
        self.push_insert_returning(row.as_slice(), rowid).await?;
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
        let mut vals: AVec<Value<'_>> = AVec::new();
        for (i, v) in row.iter().enumerate() {
            if Some(i) == rowid_alias {
                vals.push(Value::Null);
            } else {
                vals.push(v.as_value());
            }
        }
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
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
        let mut vals: AVec<Value<'_>> = AVec::new();
        for (i, v) in new_rec.iter().enumerate() {
            if Some(i) == rowid_alias {
                vals.push(Value::Null);
            } else {
                vals.push(v.as_value());
            }
        }
        let mut rec_bytes = AVec::new();
        encode_record(vals.as_slice(), &mut rec_bytes);
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
        let mut conflicts: AVec<(i64, bool, Option<AVec<Target>>)> = AVec::new();
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
                    keyset.push(match c.col {
                        None => Target::Rowid,
                        Some(i) if Some(i) == rowid_alias => Target::Rowid,
                        Some(i) => Target::Col(i),
                    });
                }
                conflicts.push((other, false, Some(keyset)));
            }
        }
        if conflicts.is_empty() {
            self.insert_row_raw(row.as_slice(), rowid).await?;
            self.push_insert_returning(row.as_slice(), rowid).await?;
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
                return self
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
                for v in victims.iter() {
                    self.delete_row_completely(root, rowid_alias, ncols, defaults.as_slice(), *v)
                        .await?;
                }
                self.insert_row_raw(row.as_slice(), rowid).await?;
                self.push_insert_returning(row.as_slice(), rowid).await?;
                Ok(())
            }
            ConflictAction::Abort => Err(Error::Schema {
                kind: if conflicts.as_slice()[0].1 {
                    SchemaKind::DuplicateRowid
                } else {
                    SchemaKind::UniqueViolation
                },
            }),
        }
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
        let mut dec: AVec<Value<'_>> = AVec::new();
        crate::record::decode_record_into::<S::Error>(
            payload.as_slice(),
            defaults.len(),
            &mut dec,
        )?;
        let mut orec = AVec::new();
        for (i, d) in defaults.iter().enumerate() {
            orec.push(if i < dec.len() {
                OwnedValue::from_value(dec.as_slice()[i])
            } else {
                d.clone()
            });
        }
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
                Target::Rowid => match v {
                    OwnedValue::Integer(x) => new_rowid = *x,
                    _ => {
                        return Err(Error::Schema {
                            kind: SchemaKind::DatatypeMismatch,
                        });
                    }
                },
                Target::Col(i) => {
                    let i = *i;
                    if Some(i) == rowid_alias {
                        match v {
                            OwnedValue::Integer(x) => new_rowid = *x,
                            _ => {
                                return Err(Error::Schema {
                                    kind: SchemaKind::DatatypeMismatch,
                                });
                            }
                        }
                    } else {
                        new_rec.as_mut_slice()[i] = apply_affinity(v.clone(), affs[i]);
                    }
                }
            }
        }
        // v0.10 NOT NULL: the DO UPDATE result row is checked
        // with ABORT semantics unconditionally (probed on the
        // pin: no default substitution and no skip, whatever
        // the statement's own conflict action — even OR
        // REPLACE), and before its uniqueness checks, matching
        // the pin's error precedence. A WHERE that skipped the
        // update returned above, before any assignment ran.
        resolve_not_null::<S::Error>(
            new_rec.as_mut_slice(),
            not_nulls,
            defaults,
            affs,
            rowid_alias,
            ConflictAction::Abort,
        )?;
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
        self.rewrite_stored_row(
            root,
            rowid_alias,
            orec.as_slice(),
            crowid,
            new_rec.as_slice(),
            new_rowid,
        )
        .await?;
        self.push_insert_returning(new_rec.as_slice(), new_rowid)
            .await?;
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
                    Err(e) => {
                        let _ = self.conn.pager.rollback().await;
                        self.conn.explicit_txn = false;
                        return Err(e);
                    }
                }
            }
            return Ok(self.serve_returning());
        }
        let r = self.exec_update().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                // Same abort rule as INSERT (see step_insert).
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
                Err(e)
            }
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
                    Err(e) => {
                        let _ = self.conn.pager.rollback().await;
                        self.conn.explicit_txn = false;
                        return Err(e);
                    }
                }
            }
            return Ok(self.serve_returning());
        }
        let r = self.exec_delete().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
                Err(e)
            }
        }
    }

    /// Materialise the rows an UPDATE/DELETE touches (v0.5):
    /// full scan, decode each record (short records materialise
    /// defaults), keep the rows the filter accepts. Collecting
    /// before mutating is what keeps the statement's row set
    /// stable while it rewrites the B-tree underneath itself —
    /// and what makes statement atomicity a rollback away.
    #[cfg(feature = "v05")]
    async fn scan_write_rows(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        ncols: usize,
        defaults: &[OwnedValue],
        affs: &[crate::record::Affinity],
        filter: &Option<RExpr>,
    ) -> Result<AVec<WriteRow>, S::Error> {
        let mut out = AVec::new();
        let mut cursor = ScanCursor::new(root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let mut dec: AVec<Value<'_>> = AVec::new();
            crate::record::decode_record_into::<S::Error>(buf.as_slice(), ncols, &mut dec)?;
            let mut rec = AVec::new();
            for (i, d) in defaults.iter().enumerate().take(ncols) {
                rec.push(if i < dec.len() {
                    OwnedValue::from_value(dec.as_slice()[i])
                } else {
                    d.clone()
                });
            }
            if let Some(f) = filter {
                let rd = row_data_for(rowid_alias, affs, rec.as_slice(), rowid);
                #[cfg(feature = "v06")]
                let keep = crate::query::truthy(&self.eval_row_sub(f, &rd).await?) == Some(true);
                #[cfg(not(feature = "v06"))]
                let keep =
                    crate::query::truthy(&crate::query::eval::<S::Error>(f, &Ctx::Row(&rd))?)
                        == Some(true);
                if !keep {
                    continue;
                }
            }
            out.push(WriteRow { rowid, rec });
        }
        Ok(out)
    }

    #[cfg(feature = "v05")]
    async fn exec_update(&mut self) -> Result<(), S::Error> {
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
                self.materialise_sources_sub(from_sources.as_slice(), &RowData::empty())
                    .await?,
            )
        };
        #[cfg(feature = "v07")]
        let scan_filter = if from_rows.is_some() { &None } else { &filter };
        #[cfg(not(feature = "v07"))]
        let scan_filter = &filter;
        let rows = self
            .scan_write_rows(
                root,
                rowid_alias,
                ncols,
                defaults.as_slice(),
                affs.as_slice(),
                scan_filter,
            )
            .await?;
        for wrow in rows.iter() {
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
                self.join_from(
                    from_sources.as_slice(),
                    fr.as_slice(),
                    0,
                    base_rd.clone(),
                    &mut joined,
                )
                .await?;
                for c in joined.iter() {
                    let keep = match &filter {
                        None => true,
                        Some(f) => {
                            crate::query::truthy(&self.eval_row_sub(f, c).await?) == Some(true)
                        }
                    };
                    if keep {
                        combos.push(c.clone());
                    }
                }
            } else {
                combos.push(base_rd);
            }
            #[cfg(not(feature = "v07"))]
            combos.push(base_rd);
            // The row as currently stored. A FROM row matched by
            // several combinations is rewritten once per
            // combination, each result computed from its ORIGINAL
            // values (oracle-verified in the v0.7 probes: with
            // `SET v = t.v + f.w` over three matching FROM rows,
            // the last combination's value, computed from the
            // original v, is what remains).
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
                    let v = crate::query::eval::<S::Error>(e, &Ctx::Row(rd))?;
                    evaluated.push((*target, v));
                }
                let mut new_rec: AVec<OwnedValue> = wrow.rec.clone();
                let mut new_rowid = wrow.rowid;
                for (target, v) in evaluated.iter() {
                    match target {
                        Target::Rowid => match v {
                            OwnedValue::Integer(x) => new_rowid = *x,
                            // UPDATE (unlike INSERT) refuses a NULL
                            // or non-integer rowid: datatype mismatch
                            // in the oracle (v0.5 probe).
                            _ => {
                                return Err(Error::Schema {
                                    kind: SchemaKind::DatatypeMismatch,
                                });
                            }
                        },
                        Target::Col(i) => {
                            let i = *i;
                            if Some(i) == rowid_alias {
                                match v {
                                    OwnedValue::Integer(x) => new_rowid = *x,
                                    _ => {
                                        return Err(Error::Schema {
                                            kind: SchemaKind::DatatypeMismatch,
                                        });
                                    }
                                }
                                // The alias column's record field
                                // stays NULL (the rowid carries it).
                            } else {
                                new_rec.as_mut_slice()[i] =
                                    apply_affinity(v.clone(), affs.as_slice()[i]);
                            }
                        }
                    }
                }
                // v0.10 NOT NULL: the combination's result row
                // is resolved under the statement's conflict
                // action before uniqueness is considered (the
                // pin's order — NOT NULL wins precedence, and
                // an OR REPLACE substitution is what the
                // uniqueness check then sees). OR IGNORE skips
                // this combination; later combinations of the
                // same row still get their turn.
                if !resolve_not_null::<S::Error>(
                    new_rec.as_mut_slice(),
                    not_null.as_slice(),
                    defaults.as_slice(),
                    affs.as_slice(),
                    rowid_alias,
                    conflict_action,
                )? {
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
                        ConflictAction::Abort => {
                            return Err(Error::Schema {
                                kind: first_err.unwrap_or(SchemaKind::UniqueViolation),
                            });
                        }
                        ConflictAction::Replace => {
                            #[cfg(feature = "v07")]
                            {
                                for v in conflict_rows.iter() {
                                    self.delete_row_completely(
                                        root,
                                        rowid_alias,
                                        ncols,
                                        defaults.as_slice(),
                                        *v,
                                    )
                                    .await?;
                                }
                            }
                            #[cfg(not(feature = "v07"))]
                            {
                                return Err(Error::Schema {
                                    kind: first_err.unwrap_or(SchemaKind::UniqueViolation),
                                });
                            }
                        }
                    }
                }
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
                cur_rec = new_rec;
                cur_rowid = new_rowid;
                applied = true;
            }
            #[cfg(feature = "v07")]
            if applied && !returning.is_empty() {
                let rd = row_data_for(rowid_alias, affs.as_slice(), cur_rec.as_slice(), cur_rowid);
                let vals = self.eval_returning(returning.as_slice(), &rd).await?;
                self.ret_rows.push(vals);
                self.ret_rowids.push(Some(cur_rowid));
            }
            let _ = applied;
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    #[cfg(feature = "v05")]
    async fn exec_delete(&mut self) -> Result<(), S::Error> {
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
        let rows = self
            .scan_write_rows(
                root,
                rowid_alias,
                ncols,
                defaults.as_slice(),
                affs.as_slice(),
                &filter,
            )
            .await?;
        #[cfg(feature = "v07")]
        let returning = match &self.plan {
            Plan::Delete { returning, .. } => returning.clone(),
            _ => unreachable!(),
        };
        for wrow in rows.iter() {
            // RETURNING reads the row's pre-delete values,
            // evaluated before this row is removed (earlier rows
            // of the same statement are already gone, as in
            // SQLite's row-by-row processing).
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
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
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
        let (root, index_roots) = match &self.plan {
            Plan::DropTable { root, index_roots } => (*root, index_roots.clone()),
            _ => unreachable!(),
        };
        let Some(root) = root else {
            return Ok(()); // IF EXISTS no-op
        };
        let Ast::DropTable { name, .. } = &self.ast else {
            unreachable!()
        };
        let name_b = copy_vec(handle_bytes(&self.arena, *name));
        // Schema rowids: the table's own row plus every index
        // row it owns (explicit and implicit alike).
        let mut schema_rowids = AVec::new();
        {
            let mut cursor = ScanCursor::new(1);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
                if values.len() >= 3 {
                    let is_table_row = matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"table"))
                        && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, name_b.as_slice()));
                    let is_index_row = matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"index"))
                        && matches!(&values.as_slice()[2], Value::Text(n) if eq_ci(n, name_b.as_slice()));
                    if is_table_row || is_index_row {
                        schema_rowids.push(rowid);
                    }
                }
            }
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
        self.conn.pager.bump_schema_cookie();
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
        let src_rows = self
            .materialise_query_sources(qp.sources.as_slice())
            .await?;
        // Nested-loop join + WHERE, then group / project: the
        // synchronous v0.4 executor (no subqueries in the plan).
        let mut rows: AVec<RowData> = AVec::new();
        join_rows::<S::Error>(
            &qp,
            src_rows.as_slice(),
            0,
            &mut RowData::empty(),
            &mut rows,
        )?;
        let produced = project_rows::<S::Error>(&qp, rows.as_slice())?;
        self.finish_produced(&qp, produced);
        Ok(())
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
            let mut rows = AVec::new();
            let mut cursor = ScanCursor::new(sp.root);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
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
    ) -> Result<AVec<AVec<(AVec<OwnedValue>, Option<i64>)>>, S::Error> {
        let mut src_rows: AVec<AVec<(AVec<OwnedValue>, Option<i64>)>> = AVec::new();
        for sp in sources.iter() {
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &sp.derived {
                    let inner = self.exec_subplan(d, outer).await?;
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
        let mut rows = self.exec_qp_core_values(&qp).await?;
        for (op, tail) in qp.compound.iter() {
            let right = self.exec_qp_core_values(tail).await?;
            rows = combine_compound(*op, rows, right);
        }
        let comp_order = qp.comp_order.clone();
        rows.as_mut_slice().sort_by(|a, b| {
            for (idx, desc) in comp_order.iter() {
                let o = crate::record::compare_values(
                    &a.as_slice()[*idx].as_value(),
                    &b.as_slice()[*idx].as_value(),
                );
                let o = if *desc { o.reverse() } else { o };
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            core::cmp::Ordering::Equal
        });
        let rows = apply_limit_offset(rows, qp.offset, qp.limit);
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
                let mut rv = AVec::new();
                for e in r.iter() {
                    rv.push(self.eval_row_sub(e, &empty).await?);
                }
                out.push(rv);
            }
            return Ok(out);
        }
        let src_rows = self
            .materialise_sources_sub(qp.sources.as_slice(), &RowData::empty())
            .await?;
        let mut rows: AVec<RowData> = AVec::new();
        self.join_qp_async(qp, src_rows.as_slice(), 0, RowData::empty(), &mut rows)
            .await?;
        let produced = self.project_qp_async(qp, rows.as_slice()).await?;
        let mut out: AVec<AVec<OwnedValue>> = AVec::new();
        for p in produced.iter() {
            out.push(p.out.clone());
        }
        if qp.distinct {
            out = dedup_compound_rows(out);
        }
        Ok(out)
    }

    /// DISTINCT, ORDER BY, LIMIT/OFFSET over produced rows
    /// (v0.4 executor steps 4-6, shared by the subquery path).
    #[cfg(feature = "v04")]
    fn finish_produced(&mut self, qp: &QueryPlan, mut produced: AVec<Produced>) {
        if qp.distinct {
            let mut kept: AVec<Produced> = AVec::new();
            for p in produced.iter() {
                let dup = kept.iter().any(|k: &Produced| {
                    k.out.len() == p.out.len()
                        && k.out
                            .iter()
                            .zip(p.out.iter())
                            .all(|(a, b)| crate::query::values_equal_is(a, b))
                });
                if !dup {
                    kept.push(Produced {
                        out: p.out.clone(),
                        keys: p.keys.clone(),
                        rowid: p.rowid,
                    });
                }
            }
            produced = kept;
        }
        produced.as_mut_slice().sort_by(|x, y| {
            for ((av, bv), (_, desc)) in x.keys.iter().zip(y.keys.iter()).zip(qp.order_by.iter()) {
                let o = crate::record::compare_values(&av.as_value(), &bv.as_value());
                let o = if *desc { o.reverse() } else { o };
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            core::cmp::Ordering::Equal
        });
        let start = qp.offset.max(0) as usize;
        let mut kept_rows: AVec<AVec<OwnedValue>> = AVec::new();
        let mut kept_ids: AVec<Option<i64>> = AVec::new();
        for (i, p) in produced.iter().enumerate() {
            if i < start {
                continue;
            }
            if let Some(l) = qp.limit
                && l >= 0
                && (kept_rows.len() as i64) >= l
            {
                break;
            }
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
        let src_rows = self
            .materialise_sources_sub(qp.sources.as_slice(), &RowData::empty())
            .await?;
        let mut rows: AVec<RowData> = AVec::new();
        self.join_qp_async(&qp, src_rows.as_slice(), 0, RowData::empty(), &mut rows)
            .await?;
        let produced = self.project_qp_async(&qp, rows.as_slice()).await?;
        self.finish_produced(&qp, produced);
        Ok(())
    }

    /// Evaluate one expression for one row, executing any
    /// subquery nodes it contains first (v0.6). Expressions
    /// without subqueries take the plain synchronous path.
    #[cfg(feature = "v06")]
    async fn eval_row_sub(&mut self, e: &RExpr, row: &RowData) -> Result<OwnedValue, S::Error> {
        if !crate::query::has_sub(e) {
            return crate::query::eval::<S::Error>(e, &Ctx::Row(row));
        }
        let values = self.compute_sub_values(e, row).await?;
        let mut pos = 0;
        let e2 = crate::query::substitute(e, values.as_slice(), &mut pos);
        crate::query::eval::<S::Error>(&e2, &Ctx::Row(row))
    }

    /// Evaluate one expression for an aggregate group (v0.6):
    /// subqueries resolve against the group's representative
    /// row, exactly like bare columns.
    #[cfg(feature = "v06")]
    async fn eval_group_sub(
        &mut self,
        e: &RExpr,
        rows: &[RowData],
        rep: &RowData,
    ) -> Result<OwnedValue, S::Error> {
        if !crate::query::has_sub(e) {
            return crate::query::eval::<S::Error>(e, &Ctx::Group { rows, rep });
        }
        let values = self.compute_sub_values(e, rep).await?;
        let mut pos = 0;
        let e2 = crate::query::substitute(e, values.as_slice(), &mut pos);
        crate::query::eval::<S::Error>(&e2, &Ctx::Group { rows, rep })
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
            match k {
                SubKind::Scalar(plan) => {
                    let rows = self.exec_subplan(plan, outer).await?;
                    out.push(SubValue::One(
                        match rows.as_slice().first() {
                            Some(r) => r.as_slice().first().cloned().unwrap_or(OwnedValue::Null),
                            None => OwnedValue::Null,
                        },
                        crate::query::result_affinity(plan),
                    ));
                }
                SubKind::Exists(plan) => {
                    let rows = self.exec_subplan(plan, outer).await?;
                    out.push(SubValue::One(
                        OwnedValue::Integer(if rows.is_empty() { 0 } else { 1 }),
                        None,
                    ));
                }
                SubKind::In(plan) => {
                    let rows = self.exec_subplan(plan, outer).await?;
                    let mut vs = AVec::new();
                    for r in rows.iter() {
                        vs.push(r.as_slice().first().cloned().unwrap_or(OwnedValue::Null));
                    }
                    out.push(SubValue::Many(vs, crate::query::result_affinity(plan)));
                }
            }
        }
        Ok(out)
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
        alloc::boxed::Box::pin(self.exec_subplan_inner(plan, outer))
    }

    #[cfg(feature = "v06")]
    async fn exec_subplan_inner(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
    ) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        let plan = plan.clone();
        let produced = self.exec_subplan_core(&plan, outer).await?;
        // Compound finish (v0.9): combine the core rows with
        // each tail core's rows, left to right, then the
        // compound ORDER BY / LIMIT.
        #[cfg(feature = "v09")]
        if !plan.compound.is_empty() {
            let mut rows: AVec<AVec<OwnedValue>> = AVec::new();
            for p in produced.iter() {
                rows.push(p.out.clone());
            }
            for (op, tail) in plan.compound.iter() {
                let tp = self.exec_subplan_core(tail, outer).await?;
                let mut right: AVec<AVec<OwnedValue>> = AVec::new();
                for p in tp.iter() {
                    right.push(p.out.clone());
                }
                rows = combine_compound(*op, rows, right);
            }
            let comp_order = plan.comp_order.clone();
            rows.as_mut_slice().sort_by(|a, b| {
                for (idx, desc) in comp_order.iter() {
                    let o = crate::record::compare_values(
                        &a.as_slice()[*idx].as_value(),
                        &b.as_slice()[*idx].as_value(),
                    );
                    let o = if *desc { o.reverse() } else { o };
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
            return Ok(apply_limit_offset(rows, plan.offset, plan.limit));
        }
        let mut produced = produced;
        produced.as_mut_slice().sort_by(|x, y| {
            for ((av, bv), (_, desc)) in x.keys.iter().zip(y.keys.iter()).zip(plan.order_by.iter())
            {
                let o = crate::record::compare_values(&av.as_value(), &bv.as_value());
                let o = if *desc { o.reverse() } else { o };
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            core::cmp::Ordering::Equal
        });
        let mut rows: AVec<AVec<OwnedValue>> = AVec::new();
        for p in produced.iter() {
            rows.push(p.out.clone());
        }
        Ok(apply_limit_offset(rows, plan.offset, plan.limit))
    }

    /// Execute one subquery CORE (v0.6; v0.9 split): materialise
    /// sources (derived tables execute their subplan against
    /// the outer row), join onto the outer row, project, and
    /// apply the core's own DISTINCT. ORDER BY / LIMIT are the
    /// caller's job (they belong to the whole statement, or to
    /// the compound).
    #[cfg(feature = "v06")]
    async fn exec_subplan_core(
        &mut self,
        plan: &crate::query::SubPlan,
        outer: &RowData,
    ) -> Result<AVec<Produced>, S::Error> {
        // A VALUES core (v0.9): the rows evaluate against the
        // outer row directly.
        #[cfg(feature = "v09")]
        if !plan.values.is_empty() {
            let mut out = AVec::new();
            for r in plan.values.iter() {
                let mut rv = AVec::new();
                for e in r.iter() {
                    rv.push(self.eval_row_sub(e, outer).await?);
                }
                out.push(Produced {
                    out: rv,
                    keys: AVec::new(),
                    rowid: None,
                });
            }
            return Ok(out);
        }
        // Materialise the subquery's own sources.
        let mut src_rows: AVec<AVec<(AVec<OwnedValue>, Option<i64>)>> = AVec::new();
        for sp in plan.sources.iter() {
            // A derived table (v0.9): its rows are the inner
            // subplan's result for this outer row; they carry
            // no rowid.
            #[cfg(feature = "v09")]
            {
                if let Some(d) = &sp.derived {
                    let inner = self.exec_subplan(d, outer).await?;
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
        let mut joined: AVec<RowData> = AVec::new();
        self.sub_join(plan, src_rows.as_slice(), 0, outer.clone(), &mut joined)
            .await?;
        let mut produced = self.sub_project(plan, outer, joined.as_slice()).await?;
        if plan.distinct {
            let mut kept: AVec<Produced> = AVec::new();
            for p in produced.iter() {
                let dup = kept.iter().any(|k: &Produced| {
                    k.out.len() == p.out.len()
                        && k.out
                            .iter()
                            .zip(p.out.iter())
                            .all(|(a, b)| crate::query::values_equal_is(a, b))
                });
                if !dup {
                    kept.push(Produced {
                        out: p.out.clone(),
                        keys: p.keys.clone(),
                        rowid: p.rowid,
                    });
                }
            }
            produced = kept;
        }
        Ok(produced)
    }

    /// Nested-loop join for a subquery (v0.6): like `join_rows`,
    /// but the accumulator starts from the outer row and ON /
    /// WHERE evaluate through `eval_row_sub` (a nested
    /// subquery may itself be correlated to the combined row).
    /// Recursive, so the future is boxed.
    #[cfg(feature = "v06")]
    fn sub_join<'x>(
        &'x mut self,
        plan: &'x crate::query::SubPlan,
        src_rows: &'x [AVec<(AVec<OwnedValue>, Option<i64>)>],
        idx: usize,
        acc: RowData,
        out: &'x mut AVec<RowData>,
    ) -> JoinFuture<'x, S::Error> {
        alloc::boxed::Box::pin(async move {
            if idx == plan.sources.len() {
                let keep = match &plan.filter {
                    None => true,
                    Some(f) => {
                        crate::query::truthy(&self.eval_row_sub(f, &acc).await?) == Some(true)
                    }
                };
                if keep {
                    out.push(acc);
                }
                return Ok(());
            }
            let sp = &plan.sources.as_slice()[idx];
            let mut matched = false;
            for (vals, rowid) in src_rows[idx].iter() {
                let mut acc2 = acc.clone();
                for v in vals.iter() {
                    acc2.vals.push(v.clone());
                }
                acc2.rowids.push(*rowid);
                let on_ok = match &sp.on {
                    None => true,
                    Some(on) => {
                        crate::query::truthy(&self.eval_row_sub(on, &acc2).await?) == Some(true)
                    }
                };
                if on_ok {
                    matched = true;
                    self.sub_join(plan, src_rows, idx + 1, acc2, &mut *out)
                        .await?;
                }
            }
            if sp.kind == JoinKind::Left && !matched {
                let mut acc2 = acc.clone();
                for _ in 0..sp.ncols {
                    acc2.vals.push(OwnedValue::Null);
                }
                acc2.rowids.push(None);
                self.sub_join(plan, src_rows, idx + 1, acc2, &mut *out)
                    .await?;
            }
            Ok(())
        })
    }

    /// Nested-loop join for a top-level query whose ON / WHERE
    /// contain subqueries (v0.6): `join_rows` with per-candidate
    /// subquery execution. Recursive, so the future is boxed.
    #[cfg(feature = "v06")]
    fn join_qp_async<'x>(
        &'x mut self,
        qp: &'x QueryPlan,
        src_rows: &'x [AVec<(AVec<OwnedValue>, Option<i64>)>],
        idx: usize,
        acc: RowData,
        out: &'x mut AVec<RowData>,
    ) -> JoinFuture<'x, S::Error> {
        alloc::boxed::Box::pin(async move {
            if idx == qp.sources.len() {
                let keep = match &qp.filter {
                    None => true,
                    Some(f) => {
                        crate::query::truthy(&self.eval_row_sub(f, &acc).await?) == Some(true)
                    }
                };
                if keep {
                    out.push(acc);
                }
                return Ok(());
            }
            let sp = &qp.sources.as_slice()[idx];
            let mut matched = false;
            for (vals, rowid) in src_rows[idx].iter() {
                let mut acc2 = acc.clone();
                for v in vals.iter() {
                    acc2.vals.push(v.clone());
                }
                acc2.rowids.push(*rowid);
                let on_ok = match &sp.on {
                    None => true,
                    Some(on) => {
                        crate::query::truthy(&self.eval_row_sub(on, &acc2).await?) == Some(true)
                    }
                };
                if on_ok {
                    matched = true;
                    self.join_qp_async(qp, src_rows, idx + 1, acc2, &mut *out)
                        .await?;
                }
            }
            if sp.kind == JoinKind::Left && !matched {
                let mut acc2 = acc.clone();
                for _ in 0..sp.ncols {
                    acc2.vals.push(OwnedValue::Null);
                }
                acc2.rowids.push(None);
                self.join_qp_async(qp, src_rows, idx + 1, acc2, &mut *out)
                    .await?;
            }
            Ok(())
        })
    }

    /// Nested-loop join of an UPDATE's FROM sources onto one
    /// target row (v0.7): like `sub_join`, but the accumulator
    /// starts from the target row and no filter is applied here
    /// (the caller filters each complete combination). Boxed
    /// for the same layout reason as the other recursive joins.
    #[cfg(feature = "v07")]
    fn join_from<'x>(
        &'x mut self,
        sources: &'x [SourcePlan],
        src_rows: &'x [AVec<(AVec<OwnedValue>, Option<i64>)>],
        idx: usize,
        acc: RowData,
        out: &'x mut AVec<RowData>,
    ) -> JoinFuture<'x, S::Error> {
        alloc::boxed::Box::pin(async move {
            if idx == sources.len() {
                out.push(acc);
                return Ok(());
            }
            let sp = &sources[idx];
            let mut matched = false;
            for (vals, rowid) in src_rows[idx].iter() {
                let mut acc2 = acc.clone();
                for v in vals.iter() {
                    acc2.vals.push(v.clone());
                }
                acc2.rowids.push(*rowid);
                let on_ok = match &sp.on {
                    None => true,
                    Some(on) => {
                        crate::query::truthy(&self.eval_row_sub(on, &acc2).await?) == Some(true)
                    }
                };
                if on_ok {
                    matched = true;
                    self.join_from(sources, src_rows, idx + 1, acc2, &mut *out)
                        .await?;
                }
            }
            if sp.kind == JoinKind::Left && !matched {
                let mut acc2 = acc.clone();
                for _ in 0..sp.ncols {
                    acc2.vals.push(OwnedValue::Null);
                }
                acc2.rowids.push(None);
                self.join_from(sources, src_rows, idx + 1, acc2, &mut *out)
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
    ) -> Result<AVec<Produced>, S::Error> {
        let mut out = AVec::new();
        if !qp.has_agg {
            for r in rows {
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
                            .all(|(a, b)| crate::query::values_equal_is(a, b))
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
                for (av, bv) in a.0.iter().zip(b.0.iter()) {
                    let o = crate::record::compare_values(&av.as_value(), &bv.as_value());
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        for (_, grows) in groups.iter() {
            let rep = pick_rep::<S::Error>(qp, grows.as_slice(), &empty_rep)?;
            if let Some(h) = &qp.having
                && crate::query::truthy(&self.eval_group_sub(h, grows.as_slice(), &rep).await?)
                    != Some(true)
            {
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
    ) -> Result<AVec<Produced>, S::Error> {
        let mut out = AVec::new();
        if !plan.has_agg {
            for r in rows {
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
                            .all(|(a, b)| crate::query::values_equal_is(a, b))
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
                for (av, bv) in a.0.iter().zip(b.0.iter()) {
                    let o = crate::record::compare_values(&av.as_value(), &bv.as_value());
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        for (_, grows) in groups.iter() {
            let rep = pick_rep_sub::<S::Error>(plan, grows.as_slice(), &empty_rep)?;
            if let Some(h) = &plan.having
                && crate::query::truthy(&self.eval_group_sub(h, grows.as_slice(), &rep).await?)
                    != Some(true)
            {
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
        } = &self.plan
        else {
            unreachable!()
        };
        let (root, table, new_sql) = (*root, table.clone(), new_sql.clone());
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
        let taken = self
            .conn
            .catalog
            .find(&self.conn.catalog_arena, name_b.as_slice())
            .is_some()
            || self
                .conn
                .catalog
                .find_index(&self.conn.catalog_arena, name_b.as_slice())
                .is_some()
            || self
                .conn
                .catalog
                .find_view(&self.conn.catalog_arena, name_b.as_slice())
                .is_some();
        if taken {
            if *if_not_exists {
                self.phase = Phase::Done;
                return Ok(());
            }
            return Err(Error::Schema {
                kind: SchemaKind::DuplicateView,
            });
        }
        let raw = handle_bytes(&self.arena, self.sql_text);
        let sql_text = trimmed_sql_text(raw);
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
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
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
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
            if values.len() >= 2
                && matches!(&values.as_slice()[0], Value::Text(t) if eq_ci(t, b"view"))
                && matches!(&values.as_slice()[1], Value::Text(n) if eq_ci(n, name_b.as_slice()))
            {
                found = Some(rowid);
                break;
            }
        }
        let Some(srowid) = found else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownView,
            });
        };
        btree::btree_delete(&mut self.conn.pager, 1, srowid).await?;
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// Number of result columns (0 for non-SELECT statements
    /// without a RETURNING clause).
    pub fn column_count(&self) -> usize {
        match &self.plan {
            Plan::Scan { out, .. } => out.len(),
            #[cfg(feature = "v04")]
            Plan::Query(q) => q.width,
            #[cfg(feature = "v07")]
            Plan::Insert { returning, .. } => returning.len(),
            #[cfg(all(feature = "v05", feature = "v07"))]
            Plan::Update { returning, .. } => returning.len(),
            #[cfg(all(feature = "v05", feature = "v07"))]
            Plan::Delete { returning, .. } => returning.len(),
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

/// The result-column name of one SELECT item (v0.9), and its
/// compound ORDER BY match key: the name is the alias when
/// written, else the column name for a direct column
/// reference, else the expression's source text (the pin's
/// naming rules, header-verified). The match key's qualifier
/// is the written qualifier, else the qualifier of the source
/// that supplies the column, else None (expressions/aliases
/// match unqualified terms only).
#[cfg(feature = "v04")]
fn item_name_keys(
    arena: &Arena,
    e: &Expr,
    alias: &Option<crate::arena::ABytes>,
    text: &crate::arena::ABytes,
    cur: &crate::query::ScopeGroup,
) -> (AVec<u8>, crate::query::OrderName) {
    if let Some(a) = alias {
        let n = copy_vec(handle_bytes(arena, *a));
        return (n.clone(), (None, n));
    }
    match e {
        Expr::Column { table, name } => {
            let nb = copy_vec(handle_bytes(arena, *name));
            let qual = match table {
                Some(q) => Some(copy_vec(handle_bytes(arena, *q))),
                None => cur
                    .sources
                    .iter()
                    .find(|s| {
                        s.columns
                            .iter()
                            .any(|(c, _)| eq_ci(c.as_slice(), nb.as_slice()))
                    })
                    .map(|s| s.qualifier.clone()),
            };
            (nb.clone(), (qual, nb))
        }
        _ => {
            let t = copy_vec(handle_bytes(arena, *text));
            (t.clone(), (None, t))
        }
    }
}

/// A VALUES core's result column name (v0.9): `column1`,
/// `column2`, ... (pin-verified via the CLI header).
#[cfg(feature = "v04")]
fn values_col_name(i: usize) -> AVec<u8> {
    let mut v = AVec::new();
    v.extend_from_slice(b"column");
    crate::record::render_int((i + 1) as i64, &mut v);
    v
}

/// Resolve a compound SELECT's ORDER BY terms (v0.9) to
/// output-column indices. The pin's rule: a term is an output
/// ordinal, or a (possibly qualified) name matching a result
/// column of ANY constituent core — anything else ("ORDER BY
/// a+0", a constant, an unknown name) fails prepare with
/// "ORDER BY term does not match any column in the result
/// set", mirrored here as `SchemaKind::UnknownColumn`.
#[cfg(feature = "v09")]
fn resolve_compound_order<E>(
    order_by: &[(Expr, bool)],
    arena: &Arena,
    cores_names: &[&[crate::query::OrderName]],
    width: usize,
) -> Result<AVec<(usize, bool)>, E> {
    let mut out = AVec::new();
    for (e, desc) in order_by.iter() {
        let idx = match e {
            Expr::Literal(Literal::Integer(n)) if *n >= 1 && (*n as usize) <= width => {
                *n as usize - 1
            }
            Expr::Column { table, name } => {
                let nb = handle_bytes(arena, *name);
                let qb = table.map(|h| handle_bytes(arena, h));
                let mut found = None;
                'cores: for names in cores_names.iter() {
                    for (i, (q, n)) in names.iter().enumerate() {
                        let qual_ok = match (&qb, q) {
                            (None, _) => true,
                            (Some(want), Some(have)) => eq_ci(have.as_slice(), want),
                            (Some(_), None) => false,
                        };
                        if qual_ok && eq_ci(n.as_slice(), nb) {
                            found = Some(i);
                            break 'cores;
                        }
                    }
                }
                match found {
                    Some(i) => i,
                    None => {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownColumn,
                        });
                    }
                }
            }
            _ => {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownColumn,
                });
            }
        };
        out.push((idx, *desc));
    }
    Ok(out)
}

/// Compound row equality (v0.9): raw storage-class comparison
/// per column, NULLs equal — the pin applies NO affinity in
/// compound operations (`SELECT 1 UNION SELECT '1'` keeps both
/// rows; a TEXT column does not convert the other side either).
#[cfg(feature = "v09")]
fn compound_rows_equal(a: &[OwnedValue], b: &[OwnedValue]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| crate::query::values_equal_is(x, y))
}

/// Remove duplicate rows, keeping first occurrences (v0.9).
#[cfg(feature = "v09")]
fn dedup_compound_rows(rows: AVec<AVec<OwnedValue>>) -> AVec<AVec<OwnedValue>> {
    let mut kept: AVec<AVec<OwnedValue>> = AVec::new();
    for r in rows.iter() {
        if !kept
            .iter()
            .any(|k| compound_rows_equal(k.as_slice(), r.as_slice()))
        {
            kept.push(r.clone());
        }
    }
    kept
}

/// Sort rows the way the pin's compound sorter emits them
/// (v0.9): ascending by every column, raw comparison.
#[cfg(feature = "v09")]
fn compound_sort_rows(rows: &mut [AVec<OwnedValue>]) {
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b.iter()) {
            let o = crate::record::compare_values(&x.as_value(), &y.as_value());
            if o != core::cmp::Ordering::Equal {
                return o;
            }
        }
        core::cmp::Ordering::Equal
    });
}

/// Combine two cores' rows under one compound operator
/// (v0.9). UNION / INTERSECT / EXCEPT deduplicate and emit in
/// sorter order, like the pin; UNION ALL concatenates in
/// operand order.
#[cfg(feature = "v09")]
fn combine_compound(
    op: crate::sql::CompoundOp,
    left: AVec<AVec<OwnedValue>>,
    right: AVec<AVec<OwnedValue>>,
) -> AVec<AVec<OwnedValue>> {
    use crate::sql::CompoundOp;
    match op {
        CompoundOp::UnionAll => {
            let mut out = left;
            for r in right.iter() {
                out.push(r.clone());
            }
            out
        }
        CompoundOp::Union => {
            // Pin-verified survivor rule: each side first
            // deduplicates first-occurrence-wins; then a right
            // row REPLACES an equal left row (`SELECT 1 UNION
            // SELECT 1.0` yields the real 1.0, `SELECT 1.0
            // UNION SELECT 1` the integer 1), and right rows
            // with no equal append.
            let mut out = dedup_compound_rows(left);
            for r in dedup_compound_rows(right).iter() {
                match out
                    .iter()
                    .position(|k| compound_rows_equal(k.as_slice(), r.as_slice()))
                {
                    Some(i) => out.as_mut_slice()[i] = r.clone(),
                    None => out.push(r.clone()),
                }
            }
            compound_sort_rows(out.as_mut_slice());
            out
        }
        CompoundOp::Intersect => {
            let mut out: AVec<AVec<OwnedValue>> = AVec::new();
            for r in dedup_compound_rows(left).iter() {
                if right
                    .iter()
                    .any(|x| compound_rows_equal(x.as_slice(), r.as_slice()))
                {
                    out.push(r.clone());
                }
            }
            compound_sort_rows(out.as_mut_slice());
            out
        }
        CompoundOp::Except => {
            let mut out: AVec<AVec<OwnedValue>> = AVec::new();
            for r in dedup_compound_rows(left).iter() {
                if !right
                    .iter()
                    .any(|x| compound_rows_equal(x.as_slice(), r.as_slice()))
                {
                    out.push(r.clone());
                }
            }
            compound_sort_rows(out.as_mut_slice());
            out
        }
    }
}

/// OFFSET / LIMIT over finished rows (v0.9 extraction of the
/// tail of the subquery finisher): a negative LIMIT means no
/// limit, as in SQLite.
#[cfg(feature = "v06")]
fn apply_limit_offset(
    rows: AVec<AVec<OwnedValue>>,
    offset: i64,
    limit: Option<i64>,
) -> AVec<AVec<OwnedValue>> {
    let start = offset.max(0) as usize;
    let mut out: AVec<AVec<OwnedValue>> = AVec::new();
    for (i, r) in rows.iter().enumerate() {
        if i < start {
            continue;
        }
        if let Some(l) = limit
            && l >= 0
            && (out.len() as i64) >= l
        {
            break;
        }
        out.push(r.clone());
    }
    out
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

/// The representative row of a subquery's group (v0.6): the
/// `pick_rep` rule (first row, or the extremum row of a lone
/// min/max aggregate) over combined rows. Aggregate arguments
/// are subquery-free by the plan-time check, so plain
/// synchronous evaluation suffices here.
#[cfg(feature = "v06")]
fn pick_rep_sub<E>(
    plan: &crate::query::SubPlan,
    grows: &[RowData],
    empty_rep: &RowData,
) -> Result<RowData, E> {
    if grows.is_empty() {
        return Ok(empty_rep.clone());
    }
    let mut aggs: AVec<&RExpr> = AVec::new();
    for it in plan.items.iter() {
        crate::query::collect_aggs(it, &mut aggs);
    }
    if let Some(h) = &plan.having {
        crate::query::collect_aggs(h, &mut aggs);
    }
    if aggs.len() == 1
        && let RExpr::Agg { kind, args, .. } = aggs.as_slice()[0]
        && matches!(
            kind,
            crate::query::AggKind::Min | crate::query::AggKind::Max
        )
        && !args.is_empty()
    {
        let arg = &args.as_slice()[0];
        let mut best: Option<&RowData> = None;
        let mut bestv: Option<OwnedValue> = None;
        for r in grows {
            let v = crate::query::eval::<E>(arg, &Ctx::Row(r))?;
            if matches!(v, OwnedValue::Null) {
                continue;
            }
            let take = match &bestv {
                None => true,
                Some(b) => {
                    let o = crate::record::compare_values(&v.as_value(), &b.as_value());
                    if *kind == crate::query::AggKind::Min {
                        o == core::cmp::Ordering::Less
                    } else {
                        o == core::cmp::Ordering::Greater
                    }
                }
            };
            if take {
                best = Some(r);
                bestv = Some(v);
            }
        }
        if let Some(b) = best {
            return Ok(b.clone());
        }
    }
    Ok(grows[0].clone())
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

/// Nested-loop join (v0.4): extend `acc` with source `idx`'s
/// rows, recurse; a LEFT source that matched nothing extends
/// once with NULLs. The WHERE filter applies to complete rows.
#[cfg(feature = "v04")]
fn join_rows<E>(
    qp: &QueryPlan,
    src_rows: &[AVec<(AVec<OwnedValue>, Option<i64>)>],
    idx: usize,
    acc: &mut RowData,
    out: &mut AVec<RowData>,
) -> Result<(), E> {
    if idx == qp.sources.len() {
        let keep = match &qp.filter {
            None => true,
            Some(f) => {
                crate::query::truthy(&crate::query::eval::<E>(f, &Ctx::Row(acc))?) == Some(true)
            }
        };
        if keep {
            out.push(acc.clone());
        }
        return Ok(());
    }
    let sp = &qp.sources.as_slice()[idx];
    let base = acc.vals.len();
    let mut matched = false;
    for (vals, rowid) in src_rows[idx].iter() {
        for v in vals.iter() {
            acc.vals.push(v.clone());
        }
        acc.rowids.push(*rowid);
        let on_ok = match &sp.on {
            None => true,
            Some(on) => {
                crate::query::truthy(&crate::query::eval::<E>(on, &Ctx::Row(acc))?) == Some(true)
            }
        };
        if on_ok {
            matched = true;
            join_rows::<E>(qp, src_rows, idx + 1, acc, out)?;
        }
        while acc.vals.len() > base {
            acc.vals.pop();
        }
        acc.rowids.pop();
    }
    if sp.kind == JoinKind::Left && !matched {
        for _ in 0..sp.ncols {
            acc.vals.push(OwnedValue::Null);
        }
        acc.rowids.push(None);
        join_rows::<E>(qp, src_rows, idx + 1, acc, out)?;
        while acc.vals.len() > base {
            acc.vals.pop();
        }
        acc.rowids.pop();
    }
    Ok(())
}

/// Group (when the plan aggregates) and project joined rows.
#[cfg(feature = "v04")]
fn project_rows<E>(qp: &QueryPlan, rows: &[RowData]) -> Result<AVec<Produced>, E> {
    let mut out = AVec::new();
    if !qp.has_agg {
        for r in rows {
            let ctx = Ctx::Row(r);
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
        for r in rows {
            let ctx = Ctx::Row(r);
            let mut key = AVec::new();
            for g in qp.group_by.iter() {
                key.push(crate::query::eval::<E>(g, &ctx)?);
            }
            let mut found = None;
            for (gi, (k, _)) in groups.iter().enumerate() {
                if k.len() == key.len()
                    && k.iter()
                        .zip(key.iter())
                        .all(|(a, b)| crate::query::values_equal_is(a, b))
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
            for (av, bv) in a.0.iter().zip(b.0.iter()) {
                let o = crate::record::compare_values(&av.as_value(), &bv.as_value());
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            core::cmp::Ordering::Equal
        });
    }
    for (_, grows) in groups.iter() {
        let rep = pick_rep::<E>(qp, grows.as_slice(), &empty_rep)?;
        let ctx = Ctx::Group {
            rows: grows.as_slice(),
            rep: &rep,
        };
        if let Some(h) = &qp.having
            && crate::query::truthy(&crate::query::eval::<E>(h, &ctx)?) != Some(true)
        {
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
    Ok(out)
}

/// The representative row of a group (SQLite's bare-column rule):
/// the first row, except that a lone min/max aggregate makes the
/// extremum-achieving row the representative.
#[cfg(feature = "v04")]
fn pick_rep<E>(qp: &QueryPlan, grows: &[RowData], empty_rep: &RowData) -> Result<RowData, E> {
    if grows.is_empty() {
        return Ok(empty_rep.clone());
    }
    let mut aggs: AVec<&RExpr> = AVec::new();
    for it in qp.items.iter() {
        crate::query::collect_aggs(it, &mut aggs);
    }
    if let Some(h) = &qp.having {
        crate::query::collect_aggs(h, &mut aggs);
    }
    if aggs.len() == 1
        && let RExpr::Agg { kind, args, .. } = aggs.as_slice()[0]
        && matches!(
            kind,
            crate::query::AggKind::Min | crate::query::AggKind::Max
        )
        && !args.is_empty()
    {
        let arg = &args.as_slice()[0];
        let mut best: Option<&RowData> = None;
        let mut bestv: Option<OwnedValue> = None;
        for r in grows {
            let v = crate::query::eval::<E>(arg, &Ctx::Row(r))?;
            if matches!(v, OwnedValue::Null) {
                continue;
            }
            let take = match &bestv {
                None => true,
                Some(b) => {
                    let o = crate::record::compare_values(&v.as_value(), &b.as_value());
                    if *kind == crate::query::AggKind::Min {
                        o == core::cmp::Ordering::Less
                    } else {
                        o == core::cmp::Ordering::Greater
                    }
                }
            };
            if take {
                best = Some(r);
                bestv = Some(v);
            }
        }
        if let Some(b) = best {
            return Ok(b.clone());
        }
    }
    Ok(grows[0].clone())
}
