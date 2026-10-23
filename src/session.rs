//! Session API (SPEC §5.8): `Connection` / `Statement` over the
//! async-primitives `Storage` trait, with the planner and the
//! tree-walking executor (§5.7).
//!
//! Ownership encodes the spec's concurrency rules: a `Statement`
//! borrows its `Connection` mutably, so one operation is in flight
//! per connection (§5.1.2) and a catalog rebuild can only happen
//! when no statement is live (§7.5.5) — both by construction.

use crate::arena::{AVec, Arena};
use crate::btree::{self, ScanCursor};
use crate::error::{Error, Result, SchemaKind, Scope};
use crate::pager::Pager;
use crate::record::{OwnedValue, Value, apply_affinity, encode_record};
use crate::schema::{Catalog, build_catalog};
use crate::sql::{Ast, BinOp, Expr, Literal, SelectItem, SelectStmt, eq_ci, handle_bytes, parse};
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
}

struct InsCol {
    affinity: crate::record::Affinity,
    default: OwnedValue,
}

#[derive(Clone, Copy)]
enum Target {
    Col(usize),
    Rowid,
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
                    });
                }
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
                let mut rows_v = AVec::new();
                for r in rows.iter() {
                    let mut rv = AVec::new();
                    for e in r.iter() {
                        rv.push(self.eval_value_expr(arena, e)?);
                    }
                    rows_v.push(rv);
                }
                Ok(Plan::Insert {
                    root: t.root,
                    rowid_alias: t.rowid_alias,
                    cols,
                    targets,
                    rows_v,
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
        }
    }

    /// Evaluate one INSERT VALUES expression at plan time
    /// (v0.4: constant expressions; pre-v0.4: plain literals).
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
        use crate::query::{SourceDesc, has_agg, resolve};
        let mut sources: AVec<SourceDesc> = AVec::new();
        let mut offsets: AVec<usize> = AVec::new();
        let mut src_plans: AVec<SourcePlan> = AVec::new();
        let mut total = 0usize;
        for s in stmt.from.iter() {
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
            });
            src_plans.push(SourcePlan {
                root: t.root,
                kind: s.kind,
                on: None,
                rowid_alias: t.rowid_alias,
                ncols: t.columns.len(),
                affs,
                defaults,
            });
            total += t.columns.len();
        }
        let filter = match &stmt.filter {
            None => None,
            Some(e) => {
                let r = resolve::<S::Error>(e, arena, sources.as_slice(), offsets.as_slice())?;
                if has_agg(&r) {
                    return Err(Error::Unsupported);
                }
                Some(r)
            }
        };
        for (i, s) in stmt.from.iter().enumerate() {
            if let Some(on) = &s.on {
                let r = resolve::<S::Error>(on, arena, sources.as_slice(), offsets.as_slice())?;
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
        for it in stmt.items.iter() {
            match it {
                SelectItem::Star => {
                    for (si, src) in sources.iter().enumerate() {
                        for (ci, (_, aff)) in src.columns.iter().enumerate() {
                            items.push(if Some(ci) == src.rowid_alias {
                                RExpr::Rowid(si)
                            } else {
                                RExpr::Column(offsets.as_slice()[si] + ci, *aff)
                            });
                            item_aliases.push(None);
                        }
                    }
                }
                SelectItem::TableStar(q) => {
                    let qb = handle_bytes(arena, *q);
                    let mut hit = false;
                    for (si, src) in sources.iter().enumerate() {
                        if eq_ci(src.qualifier.as_slice(), qb) {
                            hit = true;
                            for (ci, (_, aff)) in src.columns.iter().enumerate() {
                                items.push(if Some(ci) == src.rowid_alias {
                                    RExpr::Rowid(si)
                                } else {
                                    RExpr::Column(offsets.as_slice()[si] + ci, *aff)
                                });
                                item_aliases.push(None);
                            }
                        }
                    }
                    if !hit {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownColumn,
                        });
                    }
                }
                SelectItem::Expr(e, alias, _) => {
                    items.push(resolve::<S::Error>(
                        e,
                        arena,
                        sources.as_slice(),
                        offsets.as_slice(),
                    )?);
                    item_aliases.push(alias.map(|a| copy_vec(handle_bytes(arena, a))));
                }
            }
        }
        let width = items.len();
        // GROUP BY / HAVING / ORDER BY terms first resolve as an
        // output ordinal or alias (SQLite's rule), substituting
        // the item expression itself, so evaluation never needs
        // an output-row context.
        let resolve_term = |e: &Expr| -> Result<RExpr, S::Error> {
            match e {
                Expr::Literal(Literal::Integer(n)) if *n >= 1 && (*n as usize) <= width => {
                    Ok(items.as_slice()[*n as usize - 1].clone())
                }
                Expr::Column { table: None, name } => {
                    let nb = handle_bytes(arena, *name);
                    match item_aliases
                        .iter()
                        .position(|a| a.as_ref().is_some_and(|x| eq_ci(x.as_slice(), nb)))
                    {
                        Some(idx) => Ok(items.as_slice()[idx].clone()),
                        None => resolve::<S::Error>(
                            e,
                            arena,
                            sources.as_slice(),
                            offsets.as_slice(),
                        ),
                    }
                }
                _ => resolve::<S::Error>(e, arena, sources.as_slice(), offsets.as_slice()),
            }
        };
        let mut group_by = AVec::new();
        for e in stmt.group_by.iter() {
            group_by.push(resolve_term(e)?);
        }
        let having = match &stmt.having {
            None => None,
            Some(e) => Some(resolve_term(e)?),
        };
        let mut order_by: AVec<(RExpr, bool)> = AVec::new();
        for (e, desc) in stmt.order_by.iter() {
            order_by.push((resolve_term(e)?, *desc));
        }
        let has_agg = !group_by.is_empty()
            || items.iter().any(|e| has_agg(e))
            || having.as_ref().is_some_and(|h| has_agg(h))
            || order_by.iter().any(|(e, _)| has_agg(e));
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

    async fn step_insert(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
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
        let Plan::Insert { rows_v, .. } = &self.plan else {
            unreachable!()
        };
        let nrows = rows_v.len();
        for r in 0..nrows {
            self.exec_insert_row(r).await?;
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    async fn exec_insert_row(&mut self, row_idx: usize) -> Result<(), S::Error> {
        let Plan::Insert {
            root,
            rowid_alias,
            cols,
            targets,
            rows_v,
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
        let values: AVec<OwnedValue> = rows_v.as_slice()[row_idx].clone();
        let ncols = cols.len();
        let mut row: AVec<OwnedValue> = AVec::new();
        for c in cols.iter() {
            row.push(c.default.clone());
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
        let pager = &mut self.conn.pager;
        let rowid = match explicit_rowid {
            Some(r) => r,
            None => match btree::seek_max_rowid(pager, *root).await? {
                Some(m) => m.checked_add(1).ok_or(Error::Schema {
                    kind: SchemaKind::DatatypeMismatch,
                })?,
                None => 1,
            },
        };
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
        // 1. Materialise each source's rows.
        let mut src_rows: AVec<AVec<(AVec<OwnedValue>, i64)>> = AVec::new();
        for sp in qp.sources.iter() {
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
                rows.push((vals, rowid));
            }
            src_rows.push(rows);
        }
        // 2. Nested-loop join + WHERE.
        let mut rows: AVec<RowData> = AVec::new();
        join_rows::<S::Error>(&qp, src_rows.as_slice(), 0, &RowData::empty(), &mut rows)?;
        // 3. Group / project.
        let mut produced = project_rows::<S::Error>(&qp, rows.as_slice())?;
        // 4. DISTINCT (on output values, first occurrence wins).
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
        // 5. ORDER BY (stable).
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
        // 6. LIMIT / OFFSET.
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
        Ok(())
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

    /// Number of result columns (0 for non-SELECT statements).
    pub fn column_count(&self) -> usize {
        match &self.plan {
            Plan::Scan { out, .. } => out.len(),
            #[cfg(feature = "v04")]
            Plan::Query(q) => q.width,
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
    src_rows: &[AVec<(AVec<OwnedValue>, i64)>],
    idx: usize,
    acc: &RowData,
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
    let mut matched = false;
    for (vals, rowid) in src_rows[idx].iter() {
        let mut acc2 = acc.clone();
        for v in vals.iter() {
            acc2.vals.push(v.clone());
        }
        acc2.rowids.push(Some(*rowid));
        let on_ok = match &sp.on {
            None => true,
            Some(on) => {
                crate::query::truthy(&crate::query::eval::<E>(on, &Ctx::Row(&acc2))?) == Some(true)
            }
        };
        if on_ok {
            matched = true;
            join_rows::<E>(qp, src_rows, idx + 1, &acc2, out)?;
        }
    }
    if sp.kind == JoinKind::Left && !matched {
        let mut acc2 = acc.clone();
        for _ in 0..sp.ncols {
            acc2.vals.push(OwnedValue::Null);
        }
        acc2.rowids.push(None);
        join_rows::<E>(qp, src_rows, idx + 1, &acc2, out)?;
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
