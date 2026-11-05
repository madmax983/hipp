//! Savepoints and the maintenance statements (v0.16, `v16`).
//!
//! - `SAVEPOINT name` / `RELEASE [SAVEPOINT] name` / `ROLLBACK
//!   [TRANSACTION] TO [SAVEPOINT] name`, with the pin's nesting:
//!   a SAVEPOINT outside a transaction starts one, releasing that
//!   outermost savepoint commits it, ROLLBACK TO keeps the
//!   savepoint open, COMMIT and ROLLBACK end every savepoint. The
//!   undo state is the pager's in-memory sub-journal
//!   (`Pager::sp_open` / `sp_rollback`); crash recovery still
//!   rests on the rollback journal alone. Every write statement
//!   inside a transaction runs under its own statement savepoint,
//!   so a failed statement undoes only itself and the transaction
//!   stays open, as on the pin (the `abort_write` macro).
//! - `VACUUM [main | temp] [INTO expr]`: the database is rebuilt
//!   compactly into an in-memory image with the pin's schema order
//!   (tables with their automatic indexes, then the other indexes,
//!   then views and triggers), row and index records copied byte
//!   for byte, rowids kept except for a table with neither an
//!   INTEGER PRIMARY KEY nor an index (renumbered, as on the pin;
//!   VACUUM INTO keeps every rowid). The image replaces the file in
//!   one journaled transaction (`Pager::replace_image`), or goes to
//!   `Config::vacuum_into`. A pending `PRAGMA page_size` applies.
//! - `REINDEX [collation | [main .] table | [main .] index]`:
//!   rebuilds the named indexes from their tables.
//! - `ANALYZE [main | temp | [main .] table | [main .] index]`:
//!   writes `sqlite_stat1` rows as the pin computes them. hipp's
//!   planner does not read them; they are written so the files
//!   interoperate (the pin plans with them).

use super::*;
use crate::arena::ABytes;
use crate::pager::{be_u32, put_be_u32};
#[cfg(feature = "indexes")]
use crate::record::compare_coll;
use crate::record::{Coll, decode_record, render_int};
use crate::storage::{MemError, MemStorage};

/// A planned savepoint or maintenance statement.
#[derive(Debug, Clone)]
pub(super) enum Maint {
    Savepoint(AVec<u8>),
    Release(AVec<u8>),
    RollbackTo(AVec<u8>),
    /// `VACUUM [main] [INTO expr]`: the target's name expression,
    /// resolved against no table (evaluated when the statement
    /// runs).
    Vacuum {
        into: Option<RExpr>,
    },
    /// A statement with nothing to do in hipp (`VACUUM temp`:
    /// hipp has no temp schema, and the pin's is empty).
    Nothing,
    Reindex(Reindex),
    Analyze(Analyze),
}

/// What a REINDEX rebuilds.
#[derive(Debug, Clone)]
pub(super) enum Reindex {
    All,
    /// Every index with a key column of this collation.
    Coll(Coll),
    /// Every index of this table (none for a view or the schema
    /// table).
    Table(AVec<u8>),
    Index(AVec<u8>),
}

/// What an ANALYZE measures.
#[derive(Debug, Clone)]
pub(super) enum Analyze {
    All,
    /// One table (a view or a system table: none — only the
    /// statistics table is created and its rows for the name
    /// deleted, as on the pin).
    Table(AVec<u8>),
    Index(AVec<u8>),
}

/// One `sqlite_stat1` row to write: `tbl`, `idx`, `stat`.
type StatRow = (AVec<u8>, Option<AVec<u8>>, AVec<u8>);

/// The statistics table's definition, as the pin writes it.
const STAT1_SQL: &[u8] = b"CREATE TABLE sqlite_stat1(tbl,idx,stat)";

/// Whether `e` ends the whole transaction rather than only the
/// failed statement: storage, memory and cache failures, as the
/// pin's I/O and out-of-memory errors do.
fn ends_transaction<E>(e: &Error<E>) -> bool {
    matches!(
        e,
        Error::Storage(_)
            | Error::ArenaExhausted { .. }
            | Error::CacheExhausted
            | Error::OperationInFlight
            | Error::CorruptJournal
            // SQLITE_FULL (AUTOINCREMENT past i64::MAX, no rowid
            // left): the pin rolls the whole transaction back.
            | Error::Schema {
                kind: SchemaKind::DatabaseFull
            }
    )
}

/// An in-memory pager's error, under the connection's storage
/// error type: a failed read of the image being built.
fn mem_err<E>(e: Error<MemError>) -> Error<E> {
    e.map_storage(|_| Error::CorruptPage { page: 0 })
}

/// Whether `name` starts with `sqlite_` in any case (the pin
/// skips such tables in ANALYZE).
fn system_name(name: &[u8]) -> bool {
    name.get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"sqlite_"))
}

/// One `sqlite_schema` row of the database being vacuumed.
struct SchemaRow {
    ty: AVec<u8>,
    name: AVec<u8>,
    tbl: AVec<u8>,
    /// The row's record as stored, for rows copied unchanged.
    raw: AVec<u8>,
    root: Option<i64>,
    sql: Option<AVec<u8>>,
}

impl SchemaRow {
    fn is(&self, ty: &[u8]) -> bool {
        self.ty.as_slice() == ty
    }
}

/// Text of a decoded record field (NULL and non-text: `None`).
fn text_of(v: Option<&Value<'_>>) -> Option<AVec<u8>> {
    match v {
        Some(Value::Text(t)) => Some(copy_vec(t)),
        _ => None,
    }
}

impl<S: Storage> Connection<S> {
    /// Plan a savepoint or maintenance statement: names are
    /// resolved here, so an unknown one fails at prepare, as on
    /// the pin.
    pub(super) fn plan_maint(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        let b = |h: ABytes| copy_vec(handle_bytes(arena, h));
        let m = match ast {
            Ast::Savepoint { name } => Maint::Savepoint(b(*name)),
            Ast::Release { name } => Maint::Release(b(*name)),
            Ast::RollbackTo { name } => Maint::RollbackTo(b(*name)),
            Ast::Vacuum { schema, into } => {
                let temp = match schema {
                    None => false,
                    Some(h) => self.schema_is_temp(handle_bytes(arena, *h))?,
                };
                match (temp, into) {
                    (true, None) => Maint::Nothing,
                    // The temp schema's image: hipp has none.
                    (true, Some(_)) => return Err(Error::Unsupported),
                    (false, None) => Maint::Vacuum { into: None },
                    (false, Some(e)) => {
                        use crate::query::{ScopeGroup, has_agg, resolve_scoped};
                        let cur = ScopeGroup::new(AVec::new(), AVec::new(), 0);
                        let mut planner = self.sub_planner(arena);
                        let re = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                        if has_agg(&re) {
                            return Err(Error::Unsupported);
                        }
                        Maint::Vacuum { into: Some(re) }
                    }
                }
            }
            Ast::Reindex { schema, name } => {
                Maint::Reindex(self.reindex_target(arena, *schema, *name)?)
            }
            Ast::Analyze { schema, name } => {
                Maint::Analyze(self.analyze_target(arena, *schema, *name)?)
            }
            _ => return Err(Error::Unsupported),
        };
        Ok(Plan::Maint(m))
    }

    /// `main` → false, `temp` → true, anything else the pin's
    /// "unknown database".
    fn schema_is_temp(&self, s: &[u8]) -> Result<bool, S::Error> {
        if eq_ci(s, b"main") {
            Ok(false)
        } else if eq_ci(s, b"temp") {
            Ok(true)
        } else {
            Err(Error::Schema {
                kind: SchemaKind::UnknownDatabase,
            })
        }
    }

    /// Whether `n` names a table, a view or the schema table.
    fn names_table(&self, n: &[u8]) -> bool {
        let ca = &self.catalog_arena;
        self.catalog.find(ca, n).is_some()
            || self.catalog.find_view(ca, n).is_some()
            || Catalog::is_schema_name(n)
    }

    /// The pin's REINDEX lookup: with no schema, a collation
    /// name first; then a table (or view), then an index; with
    /// `main.`, a table or an index only. Anything else is
    /// "unable to identify the object to be reindexed".
    fn reindex_target(
        &self,
        arena: &Arena,
        schema: Option<ABytes>,
        name: Option<ABytes>,
    ) -> Result<Reindex, S::Error> {
        let fail = Error::Schema {
            kind: SchemaKind::ReindexTarget,
        };
        let Some(name) = name else {
            return Ok(Reindex::All);
        };
        let n = handle_bytes(arena, name);
        match schema {
            Some(s) if !eq_ci(handle_bytes(arena, s), b"main") => return Err(fail),
            Some(_) => {}
            None => {
                if let Some(c) = Coll::named(n) {
                    return Ok(Reindex::Coll(c));
                }
            }
        }
        if self.names_table(n) {
            Ok(Reindex::Table(copy_vec(n)))
        } else if self.catalog.find_index(&self.catalog_arena, n).is_some() {
            Ok(Reindex::Index(copy_vec(n)))
        } else {
            Err(fail)
        }
    }

    /// The pin's ANALYZE lookup: an unqualified `main` is the whole
    /// database (`temp` is routed away: see below); otherwise an index
    /// first, then a table or view; else "no such table".
    fn analyze_target(
        &self,
        arena: &Arena,
        schema: Option<ABytes>,
        name: Option<ABytes>,
    ) -> Result<Analyze, S::Error> {
        let Some(name) = name else {
            return Ok(Analyze::All);
        };
        let n = handle_bytes(arena, name);
        match schema {
            None if eq_ci(n, b"main") => return Ok(Analyze::All),
            // `ANALYZE temp` and `temp.x` are routed to the TEMP
            // database before planning (`session::temp`); reaching
            // here, they are the TEMP database's own statements.
            None if eq_ci(n, b"temp") => return Err(Error::Unsupported),
            None => {}
            Some(s) if self.schema_is_temp(handle_bytes(arena, s))? => {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
            Some(_) => {}
        }
        // The name as the schema stores it: the pin deletes the old
        // rows by `tbl = zName` / `idx = zName`, exactly.
        let ca = &self.catalog_arena;
        if let Some(ix) = self.catalog.find_index(ca, n) {
            Ok(Analyze::Index(copy_vec(handle_bytes(ca, ix.name))))
        } else if let Some(t) = self.catalog.find(ca, n) {
            Ok(Analyze::Table(copy_vec(handle_bytes(ca, t.name))))
        } else if let Some(v) = self.catalog.find_view(ca, n) {
            Ok(Analyze::Table(copy_vec(handle_bytes(ca, v.name))))
        } else if Catalog::is_schema_name(n) {
            Ok(Analyze::Table(copy_vec(b"sqlite_master")))
        } else {
            Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            })
        }
    }
}

impl<S: Storage> Statement<'_, S> {
    /// Open this statement's savepoint before its first step, when
    /// it may write inside a transaction (see the `abort_write` macro).
    pub(super) fn open_stmt(&mut self) {
        if self.phase != Phase::Fresh {
            return;
        }
        self.upsert_failed = false;
        if !self.conn.explicit_txn || self.stmt_sp.is_some() {
            return;
        }
        // A statement of a trigger program undoes nothing itself (the
        // statement that fired the trigger does, `session::trigger`).
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        if self.conn.in_program() {
            return;
        }
        let writes = match &self.plan {
            Plan::Query(_)
            | Plan::Scan { .. }
            | Plan::Begin
            | Plan::BeginImmediate
            | Plan::Commit
            | Plan::Rollback => false,
            Plan::Maint(m) => matches!(m, Maint::Reindex(_) | Maint::Analyze(_)),
            _ => true,
        };
        if writes {
            self.stmt_sp = Some(self.conn.pager.sp_depth());
            self.conn.pager.sp_open(None, false);
        }
    }

    /// Close this statement's savepoint after a step: kept on
    /// success; on an error that did not already undo the
    /// statement (`abort_write!`) or keep it (OR FAIL), rolled back
    /// so the failed statement leaves no trace in the transaction.
    pub(super) async fn close_stmt(&mut self, failed: bool) {
        let Some(level) = self.stmt_sp.take() else {
            return;
        };
        if self.conn.pager.sp_depth() <= level {
            return; // the transaction ended under it
        }
        if failed && self.conn.explicit_txn && self.conn.pager.sp_rollback(level).await.is_err() {
            let _ = self.conn.pager.rollback().await;
            self.conn.explicit_txn = false;
            return;
        }
        self.conn.pager.sp_release(level);
    }

    /// Keep what this statement changed although it failed (OR
    /// FAIL): its savepoint is released.
    pub(super) fn keep_stmt(&mut self) {
        #[cfg(feature = "foreign_keys")]
        {
            self.conn.fk.kept = true;
        }
        if let Some(level) = self.stmt_sp.take()
            && self.conn.pager.sp_depth() > level
        {
            self.conn.pager.sp_release(level);
        }
    }

    /// Roll back only this failed statement, when it runs under a
    /// statement savepoint and `e` does not end the transaction;
    /// whether it did.
    pub(super) async fn stmt_rollback(&mut self, e: &Error<S::Error>) -> bool {
        let Some(level) = self.stmt_sp else {
            return false;
        };
        if !self.conn.explicit_txn
            || self.conn.pager.sp_depth() <= level
            || ends_transaction(e)
            || self.resolves_rollback(e)
        {
            return false;
        }
        if self.conn.pager.sp_rollback(level).await.is_err() {
            return false;
        }
        self.conn.pager.sp_release(level);
        self.stmt_sp = None;
        true
    }

    /// Whether `e`, a constraint violation of this INSERT / UPDATE,
    /// resolves by ROLLBACK and so ends the whole transaction, as on
    /// the pin: the statement's `OR ROLLBACK`, or, without an OR
    /// clause, the failed constraint's own `ON CONFLICT ROLLBACK`
    /// (NOT NULL; UNIQUE / PRIMARY KEY; v0.16: the very constraint
    /// that failed, recorded in `fail_act`). CHECK constraints take
    /// no conflict clause.
    pub(super) fn resolves_rollback(&self, e: &Error<S::Error>) -> bool {
        let conflict = match &self.plan {
            Plan::Insert { conflict, .. } | Plan::Update { conflict, .. } => *conflict,
            // An R*Tree write (v0.16, `rtree`): its UNIQUE and rtree
            // constraints.
            #[cfg(feature = "rtree")]
            Plan::Vtab(p) => match &**p {
                super::vtab::VtPlan::Write(w) => {
                    return w.conflict == ConflictAction::Rollback
                        && matches!(
                            e,
                            Error::Schema {
                                kind: SchemaKind::UniqueViolation | SchemaKind::RtreeConstraint,
                            }
                        );
                }
                _ => return false,
            },
            _ => return false,
        };
        if !matches!(
            e,
            Error::Schema {
                kind: SchemaKind::NotNullViolation
                    | SchemaKind::UniqueViolation
                    | SchemaKind::DuplicateRowid
                    | SchemaKind::CheckViolation,
            }
        ) {
            return false;
        }
        if self.upsert_failed {
            return false;
        }
        // The failing constraint's own action when it resolved by its
        // clause (NOT NULL, UNIQUE / PRIMARY KEY, the rowid); a CHECK
        // takes the statement's.
        self.fail_act.get().unwrap_or(conflict) == ConflictAction::Rollback
    }

    pub(super) async fn step_maint(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let Plan::Maint(m) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let m = m.clone();
        match m {
            Maint::Nothing => {}
            Maint::Savepoint(name) => {
                #[cfg(feature = "foreign_keys")]
                {
                    let level = self.conn.pager.sp_depth();
                    self.conn.fk.savepoint(level);
                }
                let starts = !self.conn.explicit_txn;
                self.conn.pager.sp_open(Some(name.as_slice()), starts);
                // As at BEGIN (v0.16, `wal`: the checkpoint's
                // read-since-the-transaction-began rule).
                #[cfg(feature = "wal")]
                if starts {
                    self.conn.pager.clear_touched();
                }
                self.conn.explicit_txn = true;
            }
            Maint::Release(name) => self.release(name.as_slice()).await?,
            Maint::RollbackTo(name) => self.rollback_to(name.as_slice()).await?,
            Maint::Vacuum { into: None } => self.vacuum(None).await?,
            Maint::Vacuum { into: Some(e) } => {
                // The pin's "non-text filename": the name must be
                // text (NULL, numbers and blobs are refused).
                let name = match self.eval_row_sub(&e, &RowData::empty()).await? {
                    OwnedValue::Text(t) => t,
                    _ => {
                        return Err(Error::Schema {
                            kind: SchemaKind::VacuumTarget,
                        });
                    }
                };
                self.vacuum(Some(name.as_slice())).await?
            }
            Maint::Reindex(r) => {
                let every = matches!(r, Reindex::All | Reindex::Coll(_));
                let r = self.reindex(&r).await;
                self.finish_write(r).await?;
                // A plain `REINDEX` (or one of a collation) rebuilds
                // every attached database's indexes too (v0.16 review
                // fix, `attach`: the pin's `reindexDatabases`).
                #[cfg(feature = "attach")]
                if every && matches!(self.ast, Ast::Reindex { schema: None, .. }) {
                    let sql = copy_vec(handle_bytes(&self.arena, self.sql_text));
                    self.conn.maint_attached(sql.as_slice()).await?;
                }
                let _ = every;
            }
            Maint::Analyze(a) => {
                let r = self.analyze(&a).await;
                self.finish_write(r).await?;
                // A plain `ANALYZE` analyzes every attached database
                // too, after main (v0.16 review fix, `attach`: the
                // pin's `sqlite3Analyze` covers every database but
                // TEMP).
                #[cfg(feature = "attach")]
                if matches!(self.ast, Ast::Analyze { name: None, .. }) {
                    self.conn.maint_attached(b"ANALYZE").await?;
                }
            }
        }
        self.phase = Phase::Done;
        Ok(Step::Done)
    }

    /// Commit a successful autocommit write, or undo a failed one.
    async fn finish_write(&mut self, r: Result<(), S::Error>) -> Result<(), S::Error> {
        let r = match r {
            Ok(()) if !self.conn.explicit_txn => self.conn.pager.drive_commit().await,
            r => r,
        };
        if let Err(e) = r {
            abort_write!(self, &e);
            return Err(e);
        }
        Ok(())
    }

    fn no_such_savepoint() -> Error<S::Error> {
        Error::Schema {
            kind: SchemaKind::NoSuchSavepoint,
        }
    }

    /// `RELEASE name`: closes the newest savepoint so named and
    /// every newer one; when that is the savepoint that opened the
    /// transaction, the transaction commits (a failed commit
    /// leaves everything open, to retry or roll back).
    async fn release(&mut self, name: &[u8]) -> Result<(), S::Error> {
        let level = self
            .conn
            .pager
            .sp_find(name)
            .ok_or_else(Self::no_such_savepoint)?;
        if level == 0 && self.conn.pager.sp_starts_txn(0) {
            // Outstanding deferred foreign-key violations fail it,
            // everything staying open (`foreign_keys`).
            #[cfg(feature = "foreign_keys")]
            self.conn.fk_commit_check()?;
            // Every database's commit (v0.16, `attach`).
            #[cfg(feature = "attach")]
            self.conn.commit_all().await?;
            self.conn.pager.drive_commit().await?;
            self.conn.pager.sp_clear();
            self.conn.explicit_txn = false;
        } else {
            self.conn.pager.sp_release(level);
            #[cfg(feature = "foreign_keys")]
            self.conn.fk.release(level);
        }
        Ok(())
    }

    /// `ROLLBACK TO name`: undoes everything since the newest
    /// savepoint so named opened; it stays open, newer ones close,
    /// the transaction continues.
    async fn rollback_to(&mut self, name: &[u8]) -> Result<(), S::Error> {
        let level = self
            .conn
            .pager
            .sp_find(name)
            .ok_or_else(Self::no_such_savepoint)?;
        if let Err(e) = self.conn.pager.sp_rollback(level).await {
            let _ = self.conn.pager.rollback().await;
            self.conn.explicit_txn = false;
            return Err(e);
        }
        // The deferred foreign-key counters return with it.
        #[cfg(feature = "foreign_keys")]
        self.conn.fk.rollback_to(level);
        // The schema may have rolled back with it. (Re-reading it is
        // not a read of the transaction's: v0.16, `wal`.)
        #[cfg(feature = "wal")]
        let touched = self.conn.pager.touched();
        self.conn.rebuild_catalog().await?;
        #[cfg(feature = "wal")]
        if !touched {
            self.conn.pager.clear_touched();
        }
        Ok(())
    }

    // ---------------------------------------------------- VACUUM

    async fn vacuum(&mut self, into: Option<&[u8]>) -> Result<(), S::Error> {
        if self.conn.explicit_txn || self.conn.pager.in_txn() {
            return Err(Error::Schema {
                kind: SchemaKind::VacuumInTransaction,
            });
        }
        // The image's auto-vacuum mode: a pending `PRAGMA auto_vacuum`
        // or the file's own, as on the pin (VACUUM INTO too). Without
        // `autovacuum` hipp keeps no pointer maps: an auto-vacuum file
        // can only be vacuumed into a plain one (`auto_vacuum = 0`).
        #[cfg(feature = "autovacuum")]
        let av = self
            .conn
            .prag
            .vacuum_autovac
            .unwrap_or(self.conn.pager.av_mode());
        #[cfg(not(feature = "autovacuum"))]
        let av = self
            .conn
            .prag
            .vacuum_autovac
            .unwrap_or(self.conn.pager.header().auto_vacuum);
        #[cfg(not(feature = "autovacuum"))]
        if av != 0 {
            return Err(Error::Unsupported);
        }
        // `INTO ''` is the pin's anonymous temporary database: the
        // image is built (and its errors reported), then dropped.
        let write = match into {
            Some(n) if !n.is_empty() => Some(self.conn.cfg.vacuum_into.ok_or(Error::Unsupported)?),
            _ => None,
        };
        let ps = self
            .conn
            .prag
            .vacuum_page_size
            .unwrap_or(self.conn.pager.page_size());
        // The page size of a WAL-mode database cannot change: its
        // VACUUM keeps it, as on the pin (VACUUM INTO takes the new
        // one; v0.16, `wal`).
        #[cfg(feature = "wal")]
        let ps = if into.is_none() && self.conn.pager.wal_mode() {
            self.conn.pager.page_size()
        } else {
            ps
        };
        let mut img = self.compact_image(ps, into.is_some(), av).await?;
        // Page 1's header, as the pin leaves it.
        let h = *self.conn.pager.header();
        let i1 = self.conn.pager.load(1).await?;
        let cache_size = be_u32(self.conn.pager.frame_data(i1), 48);
        let npages = u32::try_from(img.len() / ps as usize).map_err(|_| Error::Unsupported)?;
        let counter = if into.is_some() {
            1
        } else {
            h.change_counter.wrapping_add(1)
        };
        let d = img.as_mut_slice();
        if d.len() < 100 {
            return Err(Error::CorruptPage { page: 1 });
        }
        put_be_u32(d, 24, counter);
        put_be_u32(d, 28, npages);
        put_be_u32(d, 32, 0);
        put_be_u32(d, 36, 0);
        put_be_u32(d, 40, h.schema_cookie.wrapping_add(1));
        put_be_u32(d, 44, 4);
        put_be_u32(d, 48, cache_size);
        // An auto-vacuum image keeps the largest root and the mode
        // its build wrote (v0.16, `autovacuum`).
        if av == 0 {
            put_be_u32(d, 52, 0);
            put_be_u32(d, 64, 0);
        }
        #[cfg(feature = "utf16")]
        put_be_u32(d, 56, crate::pager::enc_code(self.conn.pager.text_enc()));
        #[cfg(not(feature = "utf16"))]
        put_be_u32(d, 56, 1);
        put_be_u32(d, 60, h.user_version);
        put_be_u32(d, 68, h.application_id);
        put_be_u32(d, 92, counter);
        put_be_u32(d, 96, crate::pager::PIN_VERSION_NUMBER);
        // File-format bytes: WAL for an in-place VACUUM of a WAL-mode
        // database, rollback for VACUUM INTO, as the pin writes them
        // (v0.16, `wal`).
        #[cfg(feature = "wal")]
        if let Some(v) = d.get_mut(18..20) {
            let w = if into.is_none() && self.conn.pager.wal_mode() {
                2
            } else {
                1
            };
            v.copy_from_slice(&[w, w]);
        }
        match (into, write) {
            (Some(name), Some(write)) => {
                if !write(name, img.as_slice()) {
                    return Err(Error::Schema {
                        kind: SchemaKind::VacuumTarget,
                    });
                }
            }
            (Some(_), None) => {} // `INTO ''`
            (None, _) => {
                self.conn.pager.replace_image(img.as_slice(), ps).await?;
                self.conn.prag.vacuum_page_size = None;
                self.conn.prag.vacuum_autovac = None;
                self.conn.rebuild_catalog().await?;
            }
        }
        Ok(())
    }

    /// The `sqlite_schema` rows of the database, in rowid order.
    async fn schema_rows(&mut self) -> Result<AVec<SchemaRow>, S::Error> {
        let mut out = AVec::new();
        let mut cur = ScanCursor::new(1);
        while let Some((_, payload)) = cur.next_row(&mut self.conn.pager).await? {
            let vals = decode_record::<S::Error>(payload.as_slice(), 5)?;
            let v = vals.as_slice();
            let root = match v.get(3) {
                Some(Value::Integer(r)) => Some(*r),
                _ => None,
            };
            out.push(SchemaRow {
                ty: text_of(v.first()).unwrap_or_default(),
                name: text_of(v.get(1)).unwrap_or_default(),
                tbl: text_of(v.get(2)).unwrap_or_default(),
                root,
                sql: text_of(v.get(4)),
                raw: copy_vec(payload.as_slice()),
            });
        }
        Ok(out)
    }

    /// Build the vacuumed database image (header fields other than
    /// the page size, layout and schema are patched by the
    /// caller). `keep_rowids`: VACUUM INTO keeps every rowid.
    async fn compact_image(
        &mut self,
        ps: u32,
        keep_rowids: bool,
        av: u8,
    ) -> Result<AVec<u8>, S::Error> {
        let rows = self.schema_rows().await?;
        let rs = rows.as_slice();
        // The pin's order: each table (sqlite_sequence once, after
        // the first AUTOINCREMENT table, as its CREATE makes it)
        // with its automatic indexes, then the other indexes, then
        // views, triggers and virtual tables, each group in rowid
        // order.
        let seq = rs
            .iter()
            .position(|r| r.is(b"table") && eq_ci(r.name.as_slice(), b"sqlite_sequence"));
        // The AUTOINCREMENT keyword, as a token: not the word inside
        // a string, a quoted name or a comment.
        let arena = &self.arena;
        let autoinc = |r: &SchemaRow| {
            r.sql.as_ref().is_some_and(|s| {
                crate::sql::parser::has_autoincrement::<S::Error>(s.as_slice(), arena)
            })
        };
        let mut order: AVec<usize> = AVec::new();
        let mut seq_done = false;
        for (i, r) in rs.iter().enumerate() {
            if !r.is(b"table") || r.root.unwrap_or(1) <= 0 || Some(i) == seq {
                continue;
            }
            order.push(i);
            for (j, a) in rs.iter().enumerate() {
                if a.is(b"index") && a.sql.is_none() && eq_ci(a.tbl.as_slice(), r.name.as_slice()) {
                    order.push(j);
                }
            }
            if let Some(s) = seq
                && !seq_done
                && autoinc(r)
            {
                order.push(s);
                seq_done = true;
            }
        }
        for (i, r) in rs.iter().enumerate() {
            if r.is(b"index") && r.sql.is_some() {
                order.push(i);
            }
        }
        let tail: AVec<usize> = rs
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.is(b"view") || r.is(b"trigger") || (r.is(b"table") && r.root.unwrap_or(0) == 0)
            })
            .map(|(i, _)| i)
            .collect_avec();
        let cfg = &self.conn.cfg;
        let arena = Arena::new(Scope::Transaction, cfg.txn_init, cfg.txn_max);
        let (mut dst, _) = Pager::open(
            MemStorage::new(),
            cfg.cache_pages.max(4),
            ps,
            cfg.nonce_seed,
            arena,
        )
        .await
        .map_err(mem_err)?;
        // VACUUM keeps the encoding (v0.16, `utf16`).
        #[cfg(feature = "utf16")]
        dst.set_text_enc(self.conn.pager.text_enc());
        // An auto-vacuum image is built as one (v0.16, `autovacuum`):
        // roots after the largest, pointer-map pages skipped and
        // written at its commit.
        #[cfg(feature = "autovacuum")]
        if av != 0 {
            crate::autovac::set_mode(&mut dst, av)
                .await
                .map_err(mem_err)?;
        }
        #[cfg(not(feature = "autovacuum"))]
        let _ = av;
        let mut rowid = 0i64;
        // (source root, new root, is a table, schema index)
        let mut copies: AVec<(u32, u32, bool, usize)> = AVec::new();
        for &i in order.iter() {
            let r = &rs[i];
            let table = r.is(b"table");
            let root = alloc_root!(&mut dst, mem_err);
            // A WITHOUT ROWID table's tree, and its indexes, are copied
            // as raw records (v0.16).
            #[cfg(feature = "without_rowid")]
            let src_root = r.root.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
            #[cfg(feature = "without_rowid")]
            let wr = self.conn.pager.wr.is_raw(src_root);
            #[cfg(feature = "without_rowid")]
            if wr {
                dst.wr.raw_roots.push(root);
            }
            #[cfg(not(feature = "without_rowid"))]
            let wr = false;
            if table && !wr {
                btree::create_empty_root(&mut dst, root)
                    .await
                    .map_err(mem_err)?;
            } else {
                #[cfg(feature = "indexes")]
                btree::create_empty_index_root(&mut dst, root)
                    .await
                    .map_err(mem_err)?;
                // Without index B-trees hipp cannot copy one.
                #[cfg(not(feature = "indexes"))]
                return Err(Error::Unsupported);
            }
            let src = r
                .root
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(Error::CorruptPage { page: 1 })?;
            copies.push((src, root, table, i));
            let mut vals: AVec<Value<'_>> = AVec::new();
            vals.push(Value::Text(r.ty.as_slice()));
            vals.push(Value::Text(r.name.as_slice()));
            vals.push(Value::Text(r.tbl.as_slice()));
            vals.push(Value::Integer(i64::from(root)));
            vals.push(match &r.sql {
                Some(s) => Value::Text(s.as_slice()),
                None => Value::Null,
            });
            let mut rec = AVec::new();
            encode_record(vals.as_slice(), &mut rec);
            rowid += 1;
            btree::btree_insert(&mut dst, 1, rowid, rec.as_slice())
                .await
                .map_err(mem_err)?;
        }
        for &i in tail.iter() {
            rowid += 1;
            btree::btree_insert(&mut dst, 1, rowid, rs[i].raw.as_slice())
                .await
                .map_err(mem_err)?;
        }
        for &(src, root, table, i) in copies.iter() {
            let r = &rs[i];
            #[cfg(feature = "without_rowid")]
            if let Some(orders) = self.conn.pager.wr.orders_of(src) {
                let keys = btree::index_payloads(&mut self.conn.pager, src).await?;
                for k in keys.iter() {
                    btree::index_insert_core(&mut dst, root, k.as_slice(), orders.as_slice())
                        .await
                        .map_err(mem_err)?;
                }
                continue;
            }
            if table {
                let keep = keep_rowids
                    || rs
                        .iter()
                        .any(|a| a.is(b"index") && eq_ci(a.tbl.as_slice(), r.name.as_slice()))
                    || self
                        .conn
                        .catalog
                        .find(&self.conn.catalog_arena, r.name.as_slice())
                        .is_none_or(|t| t.rowid_alias.is_some());
                let mut cur = ScanCursor::new(src);
                let mut n = 0i64;
                while let Some((id, payload)) = cur.next_row(&mut self.conn.pager).await? {
                    n += 1;
                    let id = if keep { id } else { n };
                    btree::btree_insert(&mut dst, root, id, payload.as_slice())
                        .await
                        .map_err(mem_err)?;
                }
            } else {
                #[cfg(feature = "indexes")]
                self.copy_index(&mut dst, src, root, r.name.as_slice())
                    .await?;
            }
        }
        dst.bump_schema_cookie();
        dst.drive_commit().await.map_err(mem_err)?;
        let mem = dst.into_storage();
        let mut out = AVec::new();
        out.extend_from_slice(mem.snapshot().as_slice());
        Ok(out)
    }

    /// Copy the index named `name` (root `src`) into the image
    /// being built, its records byte for byte in key order.
    #[cfg(feature = "indexes")]
    async fn copy_index(
        &mut self,
        dst: &mut Pager<MemStorage>,
        src: u32,
        root: u32,
        name: &[u8],
    ) -> Result<(), S::Error> {
        let ca = &self.conn.catalog_arena;
        let ix = self
            .conn
            .catalog
            .find_index(ca, name)
            .ok_or(Error::Unsupported)?;
        if ix.opaque {
            // An index hipp cannot order (expressions, partial):
            // refused rather than mis-copied.
            return Err(Error::Unsupported);
        }
        let mut desc = AVec::new();
        for c in ix.columns.iter() {
            desc.push(crate::record::KeyOrder {
                desc: c.desc,
                coll: c.coll,
            });
        }
        let keys = btree::index_payloads(&mut self.conn.pager, src).await?;
        for k in keys.iter() {
            btree::index_insert(dst, root, k.as_slice(), desc.as_slice())
                .await
                .map_err(mem_err)?;
        }
        Ok(())
    }

    // --------------------------------------------------- REINDEX

    async fn reindex(&mut self, target: &Reindex) -> Result<(), S::Error> {
        // Without index B-trees there is nothing hipp can rebuild.
        #[cfg(not(feature = "indexes"))]
        if !self.conn.catalog.indexes.is_empty() {
            return Err(Error::Unsupported);
        }
        let ca = &self.conn.catalog_arena;
        let mut names: AVec<AVec<u8>> = AVec::new();
        for ix in self.conn.catalog.indexes.iter() {
            let hit = match target {
                Reindex::All => true,
                Reindex::Coll(c) => ix.columns.iter().any(|k| k.col.is_some() && k.coll == *c),
                Reindex::Table(t) => eq_ci(handle_bytes(ca, ix.table), t.as_slice()),
                Reindex::Index(n) => eq_ci(handle_bytes(ca, ix.name), n.as_slice()),
            };
            if hit {
                names.push(copy_vec(handle_bytes(ca, ix.name)));
            }
        }
        #[cfg(feature = "indexes")]
        for n in names.iter() {
            self.rebuild_index(n.as_slice()).await?;
        }
        #[cfg(not(feature = "indexes"))]
        let _ = names;
        Ok(())
    }

    /// Empty the index named `name` and fill it again from its
    /// table, as CREATE INDEX does (its root page stays). An index
    /// hipp cannot compute (`opaque`) is left as it is: hipp never
    /// writes its table, so it is still exact.
    #[cfg(feature = "indexes")]
    async fn rebuild_index(&mut self, name: &[u8]) -> Result<(), S::Error> {
        let ca = &self.conn.catalog_arena;
        let Some(ix) = self.conn.catalog.find_index(ca, name) else {
            return Ok(());
        };
        if ix.opaque {
            return Ok(());
        }
        // A WITHOUT ROWID table's key is the table itself (v0.16).
        #[cfg(feature = "without_rowid")]
        if ix.clustered {
            return Ok(());
        }
        let ix = ix.clone();
        let t = self
            .conn
            .catalog
            .find(ca, handle_bytes(ca, ix.table))
            .ok_or(Error::Schema {
                kind: SchemaKind::UnknownTable,
            })?
            .usable()?
            .clone();
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
        let mut defaults = AVec::new();
        for c in t.columns.iter() {
            defaults.push(read_default(&self.conn.catalog_arena, c));
        }
        let gcols = if t.has_generated() {
            self.conn
                .table_rules(&self.arena, &t)?
                .and_then(|r| r.gcols)
                .map(Box::new)
        } else {
            None
        };
        // v0.16 (`expr_index`): an expression / partial index
        // computes its keys (resolved before anything is freed).
        #[cfg(feature = "expr_index")]
        let ext = self.conn.index_ext(&self.arena, &t, &ix)?;
        let pages = btree::collect_index_pages(&mut self.conn.pager, ix.root).await?;
        for &p in pages.iter() {
            if p != ix.root {
                self.conn.pager.free_page(p).await?;
            }
        }
        self.conn.pager.dirty_page(ix.root).await?;
        btree::create_empty_index_root(&mut self.conn.pager, ix.root).await?;
        let mut cursor = ScanCursor::new(t.root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let Some(key) = index_key_for_row(
                cols.as_slice(),
                #[cfg(feature = "expr_index")]
                ext.as_deref(),
                t.rowid_alias,
                defaults.as_slice(),
                buf.as_slice(),
                t.columns.len(),
                rowid,
                gcols.as_deref(),
                #[cfg(feature = "expr_index")]
                Some(&self.conn.env),
            )?
            else {
                continue;
            };
            if ix.unique
                && key_has_no_null(key.as_slice())
                && btree::index_first_with_prefix(
                    &mut self.conn.pager,
                    ix.root,
                    desc.as_slice(),
                    key.as_slice(),
                )
                .await?
                .is_some()
            {
                return Err(Error::Schema {
                    kind: SchemaKind::UniqueViolation,
                });
            }
            let payload = encode_index_payload(key.as_slice(), rowid);
            btree::index_insert(
                &mut self.conn.pager,
                ix.root,
                payload.as_slice(),
                desc.as_slice(),
            )
            .await?;
        }
        Ok(())
    }

    // --------------------------------------------------- ANALYZE

    async fn analyze(&mut self, target: &Analyze) -> Result<(), S::Error> {
        // Measure first: a refusal leaves nothing written.
        let ca = &self.conn.catalog_arena;
        let mut jobs: AVec<(AVec<u8>, Option<AVec<u8>>)> = AVec::new();
        let tables: AVec<crate::schema::TableDef> = match target {
            Analyze::All => self
                .conn
                .catalog
                .tables
                .iter()
                .rev()
                .cloned()
                .collect_avec(),
            Analyze::Table(n) => self
                .conn
                .catalog
                .find(ca, n.as_slice())
                .into_iter()
                .cloned()
                .collect_avec(),
            Analyze::Index(n) => {
                if let Some(ix) = self.conn.catalog.find_index(ca, n.as_slice()) {
                    let t = handle_bytes(ca, ix.table);
                    if !system_name(t) {
                        jobs.push((copy_vec(t), Some(copy_vec(handle_bytes(ca, ix.name)))));
                    }
                }
                AVec::new()
            }
        };
        for t in tables.iter() {
            let tn = handle_bytes(ca, t.name);
            if system_name(tn) {
                continue;
            }
            let mut any = false;
            let idx: AVec<AVec<u8>> = self
                .conn
                .catalog
                .indexes_for(ca, tn)
                .map(|ix| copy_vec(handle_bytes(ca, ix.name)))
                .collect_avec();
            for n in idx.iter().rev() {
                any = true;
                jobs.push((copy_vec(tn), Some(n.clone())));
            }
            if !any {
                jobs.push((copy_vec(tn), None));
            }
        }
        let mut out: AVec<StatRow> = AVec::new();
        for (tn, ixn) in jobs.iter() {
            let stat = match ixn {
                None => self.table_stat(tn.as_slice()).await?,
                Some(n) => self.index_stat(n.as_slice()).await?,
            };
            // A WITHOUT ROWID table's key is listed under the table's
            // name (v0.16), as on the pin.
            #[cfg(feature = "without_rowid")]
            let ixn = &match ixn {
                Some(n)
                    if self
                        .conn
                        .catalog
                        .find_index(&self.conn.catalog_arena, n.as_slice())
                        .is_some_and(|i| i.clustered) =>
                {
                    Some(tn.clone())
                }
                other => other.clone(),
            };
            if let Some(s) = stat {
                out.push((tn.clone(), ixn.clone(), s));
            }
        }
        // The statistics table, created on first use.
        let root = match self
            .conn
            .catalog
            .find(&self.conn.catalog_arena, b"sqlite_stat1")
        {
            Some(t) => t.root,
            None => {
                let root = alloc_root!(&mut self.conn.pager);
                btree::create_empty_root(&mut self.conn.pager, root).await?;
                self.insert_schema_row(
                    b"table",
                    b"sqlite_stat1",
                    b"sqlite_stat1",
                    root,
                    Some(STAT1_SQL),
                )
                .await?;
                self.conn.pager.bump_schema_cookie();
                root
            }
        };
        // Old rows for what was measured go (all of them for a
        // whole-database ANALYZE), compared exactly as the pin's
        // `tbl = name` / `idx = name` do.
        let mut gone: AVec<i64> = AVec::new();
        let mut cur = ScanCursor::new(root);
        while let Some((id, payload)) = cur.next_row(&mut self.conn.pager).await? {
            let vals = decode_record::<S::Error>(payload.as_slice(), 3)?;
            let v = vals.as_slice();
            let hit = match target {
                Analyze::All => true,
                Analyze::Table(n) => text_of(v.first()).is_some_and(|t| t == *n),
                Analyze::Index(n) => text_of(v.get(1)).is_some_and(|t| t == *n),
            };
            if hit {
                gone.push(id);
            }
        }
        for &id in gone.iter() {
            btree::btree_delete(&mut self.conn.pager, root, id).await?;
        }
        for (tn, ixn, stat) in out.iter() {
            let next = btree::new_rowid(&mut self.conn.pager, root).await?;
            let mut vals: AVec<Value<'_>> = AVec::new();
            vals.push(Value::Text(tn.as_slice()));
            vals.push(match ixn {
                Some(n) => Value::Text(n.as_slice()),
                None => Value::Null,
            });
            vals.push(Value::Text(stat.as_slice()));
            let mut rec = AVec::new();
            encode_record(vals.as_slice(), &mut rec);
            btree::btree_insert(&mut self.conn.pager, root, next, rec.as_slice()).await?;
        }
        Ok(())
    }

    /// A table's row-count statistic (`"N"`), or `None` when it is
    /// empty (the pin writes no row then).
    async fn table_stat(&mut self, name: &[u8]) -> Result<Option<AVec<u8>>, S::Error> {
        let Some(t) = self.conn.catalog.find(&self.conn.catalog_arena, name) else {
            return Ok(None);
        };
        let mut cur = ScanCursor::new(t.root);
        let mut n = 0u64;
        let mut buf = AVec::new();
        while cur
            .next_row_into(&mut self.conn.pager, &mut buf)
            .await?
            .is_some()
        {
            n = n.saturating_add(1);
        }
        if n == 0 {
            return Ok(None);
        }
        let mut s = AVec::new();
        push_u64(&mut s, n);
        Ok(Some(s))
    }

    /// An index's statistic: `"N d1 ... dk"`, `N` its entry count
    /// and `dj` the average number of entries per distinct value
    /// of its first `j` key columns — the pin's `(N + D - 1) / D`
    /// for `D` distinct prefixes (NULLs equal, the key columns'
    /// collations deciding), with 2 lowered to 1 when `10 N <=
    /// 11 D`. `None` when the index is empty (the pin writes no
    /// row then). An index hipp cannot read (`opaque`) is refused.
    #[cfg(feature = "indexes")]
    async fn index_stat(&mut self, name: &[u8]) -> Result<Option<AVec<u8>>, S::Error> {
        let Some(ix) = self.conn.catalog.find_index(&self.conn.catalog_arena, name) else {
            return Ok(None);
        };
        if ix.opaque {
            return Err(Error::Unsupported);
        }
        let root = ix.root;
        let mut desc = AVec::new();
        for c in ix.columns.iter() {
            desc.push(crate::record::KeyOrder {
                desc: c.desc,
                coll: c.coll,
            });
        }
        let k = desc.len();
        let mut distinct: AVec<u64> = AVec::new();
        for _ in 0..k {
            distinct.push(0);
        }
        let mut n = 0u64;
        let mut prev: Option<AVec<OwnedValue>> = None;
        let mut cur = btree::IndexCursor::new(root, desc.as_slice());
        while let Some(e) = cur.next_entry(&mut self.conn.pager).await? {
            n = n.saturating_add(1);
            // The first key column whose value differs from the
            // previous entry's: every prefix from there is new.
            let first_new = match &prev {
                None => 0,
                Some(p) => (0..k)
                    .find(|&j| {
                        let coll = desc.as_slice().get(j).map(|o| o.coll).unwrap_or_default();
                        match (p.as_slice().get(j), e.values.as_slice().get(j)) {
                            (Some(a), Some(b)) => {
                                compare_coll(&a.as_value(), &b.as_value(), coll)
                                    != core::cmp::Ordering::Equal
                            }
                            _ => true,
                        }
                    })
                    .unwrap_or(k),
            };
            for d in distinct.as_mut_slice().iter_mut().skip(first_new) {
                *d = d.saturating_add(1);
            }
            prev = Some(e.values);
        }
        if n == 0 {
            return Ok(None);
        }
        let mut s = AVec::new();
        push_u64(&mut s, n);
        for &d in distinct.iter() {
            let d = d.max(1);
            let mut v = n.saturating_add(d - 1) / d;
            if v == 2 && n.saturating_mul(10) <= d.saturating_mul(11) {
                v = 1;
            }
            s.push(b' ');
            push_u64(&mut s, v);
        }
        Ok(Some(s))
    }
}

impl<S: Storage> Statement<'_, S> {
    /// Without index B-trees hipp cannot read an index.
    #[cfg(not(feature = "indexes"))]
    async fn index_stat(&mut self, _name: &[u8]) -> Result<Option<AVec<u8>>, S::Error> {
        Err(Error::Unsupported)
    }
}

/// Append `v` in decimal.
fn push_u64(out: &mut AVec<u8>, v: u64) {
    render_int(i64::try_from(v).unwrap_or(i64::MAX), out);
}

/// Collect an iterator into an `AVec` (the crate's collection).
trait CollectAVec<T> {
    fn collect_avec(self) -> AVec<T>;
}

impl<T, I: Iterator<Item = T>> CollectAVec<T> for I {
    fn collect_avec(self) -> AVec<T> {
        let mut v = AVec::new();
        for x in self {
            v.push(x);
        }
        v
    }
}
