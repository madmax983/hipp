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
use crate::sql::{Ast, Literal, SelectList, eq_ci, handle_bytes, parse};
use crate::storage::Storage;

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

/// Rowid access path the planner chose for a scan (v0.2). This is
/// the executor seam for general WHERE: richer predicates lower
/// to these same paths in later phases.
#[derive(Debug, Clone, Copy)]
enum Access {
    /// No usable predicate: full B-tree scan.
    FullScan,
    /// `WHERE rowid = <v>`: point lookup via `btree::seek_rowid`.
    Rowid(i64),
    /// A predicate that can never match (e.g. `rowid = NULL`,
    /// `rowid = 'abc'`): the scan yields no rows.
    Empty,
}

enum Plan {
    CreateTable,
    Insert {
        root: u32,
        rowid_alias: Option<usize>,
        /// Per-table-column plan data (affinity + default).
        cols: AVec<InsCol>,
        /// Where each supplied value goes, parallel to one AST
        /// value row (every row of a multi-row VALUES shares it).
        targets: AVec<Target>,
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

    /// Prepare one statement (v0.1 grammar, §5.6). A stale schema
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
        })
    }

    fn plan(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        match ast {
            Ast::CreateTable { .. } => Ok(Plan::CreateTable),
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
                Ok(Plan::Insert {
                    root: t.root,
                    rowid_alias: t.rowid_alias,
                    cols,
                    targets,
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
            Ast::Select {
                table,
                list,
                predicate,
            } => {
                let tname = handle_bytes(arena, *table);
                let t = self
                    .catalog
                    .find(&self.catalog_arena, tname)
                    .ok_or(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    })?;
                // WHERE <col> = <literal> lowers to a rowid access
                // path when <col> is the rowid or its alias; any
                // other real column is beyond v0.2's executor.
                let access = match predicate {
                    None => Access::FullScan,
                    Some((colname, lit)) => {
                        let nb = handle_bytes(arena, *colname);
                        match self.resolve_target(t, nb)? {
                            Target::Rowid => literal_rowid(arena, *lit),
                            Target::Col(i) if Some(i) == t.rowid_alias => {
                                literal_rowid(arena, *lit)
                            }
                            Target::Col(_) => return Err(Error::Unsupported),
                        }
                    }
                };
                let mut out = AVec::new();
                let mut aff = AVec::new();
                match list {
                    SelectList::All => {
                        for (i, c) in t.columns.iter().enumerate() {
                            out.push(i);
                            aff.push(c.affinity);
                        }
                    }
                    SelectList::Columns(names) => {
                        for n in names.iter() {
                            let nb = handle_bytes(arena, *n);
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
        }
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
fn integral_f64(f: f64) -> Option<i64> {
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        Some(f as i64)
    } else {
        None
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    /// Execute/advance the statement by one step.
    pub async fn step(&mut self) -> Result<Step, S::Error> {
        match &self.plan {
            Plan::CreateTable => self.step_create().await,
            Plan::Insert { .. } => self.step_insert().await,
            Plan::Scan { .. } => self.step_scan().await,
            Plan::Begin => self.step_begin().await,
            Plan::Commit => self.step_commit().await,
            Plan::Rollback => self.step_rollback().await,
        }
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
        let mut end = raw.len();
        while end > 0 && (raw[end - 1] == b';' || (raw[end - 1] as char).is_whitespace()) {
            end -= 1;
        }
        let sql_text = copy_vec(&raw[..end]);
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

    async fn exec_create(&mut self, name: &[u8], sql_text: &[u8]) -> Result<(), S::Error> {
        let pager = &mut self.conn.pager;
        let (root, _) = pager.alloc_page().await?;
        btree::create_empty_root(pager, root).await?;
        let schema_rowid = match btree::seek_max_rowid(pager, 1).await? {
            Some(m) => m + 1,
            None => 1,
        };
        let mut vals: AVec<Value<'_>> = AVec::new();
        vals.push(Value::Text(b"table"));
        vals.push(Value::Text(name));
        vals.push(Value::Text(name));
        vals.push(Value::Integer(i64::from(root)));
        vals.push(Value::Text(sql_text));
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        btree::btree_insert(pager, 1, schema_rowid, rec.as_slice()).await?;
        pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            pager.drive_commit().await?;
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
        let Ast::Insert { rows, .. } = &self.ast else {
            unreachable!()
        };
        let nrows = rows.len();
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
        } = &self.plan
        else {
            unreachable!()
        };
        let Ast::Insert { rows, .. } = &self.ast else {
            unreachable!()
        };
        let values = &rows.as_slice()[row_idx];
        let ncols = cols.len();
        let mut row: AVec<OwnedValue> = AVec::new();
        for c in cols.iter() {
            row.push(c.default.clone());
        }
        let mut explicit_rowid: Option<i64> = None;
        for (target, lit) in targets.iter().zip(values.iter()) {
            let v = literal_owned(&self.arena, Some(*lit));
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
        let mut rec = AVec::new();
        encode_record(vals.as_slice(), &mut rec);
        btree::btree_insert(pager, *root, rowid, rec.as_slice()).await?;
        Ok(())
    }

    async fn step_scan(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        // Per-row checkpoint rewind (§7.5.2): the previous row's
        // arena temporaries die before the next row is decoded.
        self.arena.rewind(self.row_ckpt);
        let Plan::Scan { root, access, .. } = &self.plan else {
            unreachable!()
        };
        let (root, access) = (*root, *access);
        match access {
            Access::Empty => {
                self.phase = Phase::Done;
                self.current_rowid = None;
                Ok(Step::Done)
            }
            Access::Rowid(want) => {
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
            Access::FullScan => {
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

    /// Number of result columns (0 for non-SELECT statements).
    pub fn column_count(&self) -> usize {
        match &self.plan {
            Plan::Scan { out, .. } => out.len(),
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
