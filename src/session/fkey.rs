//! Foreign-key enforcement (v0.16, leaf feature `foreign_keys`).
//!
//! While `PRAGMA foreign_keys` is ON every write checks the foreign
//! keys of its table as the pin does — with *counters*, not with an
//! immediate verdict per row:
//!
//! - A row added to a child table whose parent key is missing adds
//!   one violation; a row removed from it whose parent is missing
//!   takes one away (only while the counter is non-zero). A row
//!   removed from a parent table adds one violation per child row
//!   still referring to it; a row added to it takes one away per
//!   child row it now satisfies (again only while non-zero). The
//!   counter of an immediate key belongs to the statement (checked
//!   when it ends: "FOREIGN KEY constraint failed", the statement
//!   undone, whatever its conflict clause); a `DEFERRABLE
//!   INITIALLY DEFERRED` key's — and every key's under `PRAGMA
//!   defer_foreign_keys` — to the transaction (checked at COMMIT
//!   and at a RELEASE that ends the transaction, which then stay
//!   open; in autocommit at the statement's end). A savepoint
//!   remembers the transaction counters and ROLLBACK TO restores
//!   them.
//! - The pin's special case: a single-row INSERT of a top-level
//!   statement without triggers, RETURNING, REPLACE or upsert
//!   update fails at once on an immediate key, and as a parent it
//!   resolves nothing.
//! - Child rows are matched as the pin's `fkLookupParent` does: the
//!   child value takes the parent column's affinity and is looked
//!   up in the parent's PRIMARY KEY / UNIQUE index (or as its
//!   rowid); a self-referencing row may satisfy itself. Parent rows
//!   are matched as its `fkScanChildren`: `parent = child` with
//!   both columns' comparison affinities and the parent's
//!   collation.
//! - ON DELETE / ON UPDATE actions run after the row's change, as
//!   the pin's action programs: CASCADE deletes or updates the
//!   child rows, SET NULL / SET DEFAULT update them, RESTRICT fails
//!   at once (unless `defer_foreign_keys`); on UPDATE only when the
//!   key changed (`NOT (old IS new)`). The programs compare with
//!   `OLD.` values (no affinity, the parent's collation), run under
//!   ABORT whatever the firing statement says, are frames of their
//!   own for `changes()` / `last_insert_rowid()`, count towards the
//!   trigger depth limit and fire the child table's triggers.
//! - A parent key must be the parent's PRIMARY KEY or a UNIQUE
//!   index with the parent columns' collations, else "foreign key
//!   mismatch"; a missing parent table is "no such table" — both
//!   when the write starts, and only for the keys it uses.
//! - DROP TABLE first deletes the table's rows (its own triggers
//!   silent, missing parents empty, mismatches ignored) when it is
//!   a parent or has deferred violations to resolve, and fails
//!   before dropping when that leaves immediate violations.

use super::*;
use crate::record::{Affinity, Coll, KeyOrder};
use crate::sql::ast::{BinOp, Expr, Literal};
use alloc::rc::Rc;

/// The pin's action codes (`ForeignKey::on_delete` / `on_update`).
const RESTRICT: u8 = 1;
const SET_NULL: u8 = 2;
const SET_DEFAULT: u8 = 3;
const CASCADE: u8 = 4;

/// The connection's foreign-key state.
#[derive(Debug, Default)]
pub(super) struct FkState {
    /// The running top-level statement's immediate violations (the
    /// pin's `nFkConstraint`).
    stmt: i64,
    /// The transaction's deferred violations (`nDeferredCons`).
    deferred: i64,
    /// Violations counted under `defer_foreign_keys`
    /// (`nDeferredImmCons`).
    deferred_imm: i64,
    /// `PRAGMA defer_foreign_keys`.
    pub defer: bool,
    /// The transaction counters when the top-level statement began
    /// (restored when it is undone).
    start: (i64, i64),
    /// The failed top-level statement kept its changes (OR FAIL).
    pub kept: bool,
    /// An action program failed: the statement aborts, whatever its
    /// conflict clause says.
    pub abort: bool,
    /// Action programs running (counted on the trigger stack when
    /// `triggers` is built).
    #[cfg(not(feature = "triggers"))]
    pub depth: usize,
    /// Statements DROP TABLE runs inline (its DELETE).
    pub nest: usize,
    /// DROP TABLE's DELETE is about to load its keys: missing
    /// parent tables read as empty, unusable keys are skipped (the
    /// pin's `isIgnoreErrors`).
    ignore: bool,
    /// DROP TABLE's DELETE is about to load its triggers: it fires
    /// none (the pin's `disableTriggers`).
    pub silent: bool,
    /// The pin's `isSetNullAction` quirk for the top-level write: the
    /// key whose new rows are not looked up (see `fk_note_replace`).
    skip: Option<(u32, usize)>,
    /// DROP TABLE compiled its implicit DELETE: its count is the
    /// drop's `changes()`.
    pub drop_counted: bool,
    /// An autocommit write with RETURNING left deferred violations:
    /// it fails after its rows.
    pub late: bool,
    /// The top-level statement may write several rows (the pin's
    /// `isMultiWrite`).
    multi: bool,
    /// The transaction counters at each user savepoint, by level.
    sp: AVec<(usize, i64, i64)>,
}

impl FkState {
    fn count(&mut self, deferred: bool, n: i64) {
        let c = if self.defer {
            &mut self.deferred_imm
        } else if deferred {
            &mut self.deferred
        } else {
            &mut self.stmt
        };
        *c = c.saturating_add(n);
    }

    /// The pin's `OP_FkIfZero`.
    fn zero(&self, deferred: bool) -> bool {
        self.deferred_imm == 0
            && if deferred {
                self.deferred == 0
            } else {
                self.stmt == 0
            }
    }

    /// Outstanding transaction violations (the COMMIT check).
    pub(super) fn pending(&self) -> bool {
        self.deferred.saturating_add(self.deferred_imm) > 0
    }

    /// The transaction ended: every counter goes.
    fn end_txn(&mut self) {
        self.stmt = 0;
        self.deferred = 0;
        self.deferred_imm = 0;
        self.sp.clear();
    }

    /// A user savepoint opens at `level`.
    pub(super) fn savepoint(&mut self, level: usize) {
        self.cut(level);
        self.sp.push((level, self.deferred, self.deferred_imm));
    }

    /// RELEASE of the savepoint at `level` (and every newer one).
    pub(super) fn release(&mut self, level: usize) {
        self.cut(level);
    }

    /// Forget the savepoints at `level` and above (kept in level
    /// order).
    fn cut(&mut self, level: usize) {
        let keep = self.sp.iter().take_while(|e| e.0 < level).count();
        self.sp.truncate(keep);
    }

    /// ROLLBACK TO the savepoint at `level`: its counters return.
    pub(super) fn rollback_to(&mut self, level: usize) {
        self.cut(level.saturating_add(1));
        if let Some(e) = self.sp.iter().find(|e| e.0 == level) {
            self.deferred = e.1;
            self.deferred_imm = e.2;
        }
    }
}

fn violation<E>() -> Error<E> {
    Error::Schema {
        kind: SchemaKind::ForeignKeyViolation,
    }
}

fn mismatch<E>() -> Error<E> {
    Error::Schema {
        kind: SchemaKind::ForeignKeyMismatch,
    }
}

/// A table's columns as the checks read its rows.
#[derive(Debug)]
struct Shape {
    name: AVec<u8>,
    root: u32,
    ncols: usize,
    alias: Option<usize>,
    defaults: AVec<OwnedValue>,
    affs: AVec<Affinity>,
    colls: AVec<Coll>,
    names: AVec<AVec<u8>>,
    /// Each column's DEFAULT (SET DEFAULT), NULL for a generated one.
    dflt: AVec<OwnedValue>,
    rules: Option<Box<crate::rules::TableRules>>,
}

/// How a parent key is found.
#[derive(Debug)]
struct Parent {
    root: u32,
    /// The UNIQUE / PRIMARY KEY index; `None`: the rowid.
    index: Option<(u32, AVec<KeyOrder>)>,
    /// Each key column's affinity and collation.
    affs: AVec<Affinity>,
    colls: AVec<Coll>,
    /// The key column that is the INTEGER PRIMARY KEY (read as the
    /// rowid by an action program), per key position.
    ipk: AVec<bool>,
}

/// One foreign key a write checks.
#[derive(Debug)]
struct Fk {
    /// The key's identity: its child table's root page and its
    /// position in that table's list.
    id: (u32, usize),
    deferred: bool,
    on_delete: u8,
    on_update: u8,
    /// The child column of each key position.
    ccols: AVec<usize>,
    /// The parent column of each key position.
    pcols: AVec<usize>,
    /// `None`: the parent table is missing (DROP TABLE's DELETE).
    parent: Option<Parent>,
    /// Child and parent are one table.
    selfref: bool,
    /// The child table (parent-side keys: what is scanned).
    child: Shape,
}

/// The foreign keys of one write's table.
#[derive(Debug)]
pub(super) struct FkTarget {
    /// Where the table is the child, in the pin's order (the last
    /// declared first).
    child: AVec<Fk>,
    /// Where it is the parent, in the pin's order (the most
    /// recently created first).
    parent: AVec<Fk>,
    /// The written table's rowid alias and affinities (its rows'
    /// logical form).
    alias: Option<usize>,
}

/// What a write does to its table's rows, as far as loading the
/// keys goes.
#[derive(Clone, Copy)]
pub(super) enum Event<'x> {
    Insert,
    Delete,
    /// The assigned columns and whether the rowid is assigned.
    Update(&'x [bool], bool),
}

/// An UPDATE's assigned columns and whether it assigns the rowid.
type Assigned = (AVec<bool>, bool);

/// Whether one of `cols` is assigned.
fn modified(cols: &[usize], assigned: &[bool], rowid_set: bool, alias: Option<usize>) -> bool {
    cols.iter()
        .any(|c| assigned.get(*c).copied().unwrap_or(false) || (rowid_set && Some(*c) == alias))
}

impl<S: Storage> Connection<S> {
    /// The nesting depth of programs (the trigger depth limit).
    fn program_depth(&self) -> usize {
        #[cfg(feature = "triggers")]
        {
            self.trig.stack.len()
        }
        #[cfg(not(feature = "triggers"))]
        {
            self.fk.depth
        }
    }

    /// One table's row shape.
    fn fk_shape(
        &self,
        t: &crate::schema::TableDef,
        arena: &Arena,
        need_dflt: bool,
    ) -> Result<Shape, S::Error> {
        let ca = &self.catalog_arena;
        let mut s = Shape {
            name: copy_vec(handle_bytes(ca, t.name)),
            root: t.root,
            ncols: t.columns.len(),
            alias: t.rowid_alias,
            defaults: AVec::new(),
            affs: AVec::new(),
            colls: AVec::new(),
            names: AVec::new(),
            dflt: AVec::new(),
            rules: self.table_rules(arena, t)?,
        };
        let mut clock = crate::rules::Clock::new(self.cfg.clock);
        for c in t.columns.iter() {
            s.defaults.push(read_default(ca, c));
            s.affs.push(c.affinity);
            s.colls.push(c.coll);
            s.names.push(copy_vec(handle_bytes(ca, c.name)));
            s.dflt.push(if c.gen_expr.is_some() || !need_dflt {
                OwnedValue::Null
            } else {
                self.column_default(arena, c, &mut clock)?
            });
        }
        Ok(s)
    }

    /// The parsed FOREIGN KEY clauses of a table's stored text, in
    /// the pin's list order (the last declared first); `None` when
    /// the text does not parse (and names no REFERENCES).
    fn fk_clauses(
        &self,
        t: &crate::schema::TableDef,
        sc: &Arena,
    ) -> Result<AVec<crate::sql::ast::ForeignKey>, S::Error> {
        let sql = handle_bytes(&self.catalog_arena, t.sql);
        match crate::sql::parse::<S::Error>(sql, sc, self.cfg.limits()) {
            Ok(Ast::CreateTable { mut fkeys, .. }) => {
                fkeys.as_mut_slice().reverse();
                Ok(fkeys)
            }
            _ => {
                // A text hipp cannot read: its keys are never skipped
                // silently.
                if sql
                    .windows(10)
                    .any(|w| w.eq_ignore_ascii_case(b"REFERENCES"))
                {
                    return Err(Error::Unsupported);
                }
                Ok(AVec::new())
            }
        }
    }

    /// The pin's `sqlite3FkLocateIndex`: the parent key of `f`
    /// (child `c`, parent `p`) as (index or rowid, child column and
    /// parent column per key position); `None`: a mismatch.
    #[allow(clippy::type_complexity)]
    fn fk_locate(
        &self,
        c: &crate::schema::TableDef,
        p: &crate::schema::TableDef,
        f: &crate::sql::ast::ForeignKey,
        sc: &Arena,
    ) -> Option<(Option<(u32, AVec<KeyOrder>)>, AVec<usize>, AVec<usize>)> {
        let ca = &self.catalog_arena;
        let ccol = |h: crate::arena::ABytes| {
            let n = handle_bytes(sc, h);
            c.columns
                .iter()
                .position(|x| eq_ci(handle_bytes(ca, x.name), n))
        };
        let pname = |i: usize| {
            p.columns
                .as_slice()
                .get(i)
                .map(|x| handle_bytes(ca, x.name))
        };
        let n = f.from.len();
        if n == 1
            && let Some(a) = p.rowid_alias
        {
            let to = f.to.as_slice().first();
            if to.is_none_or(|h| pname(a).is_some_and(|nm| eq_ci(nm, handle_bytes(sc, *h)))) {
                let mut cc = AVec::new();
                cc.push(ccol(*f.from.as_slice().first()?)?);
                let mut pc = AVec::new();
                pc.push(a);
                return Some((None, cc, pc));
            }
        }
        let pk = p.auto_sets.iter().find(|s| s.primary_key);
        let pname_b = handle_bytes(ca, p.name);
        'idx: for ix in self.catalog.indexes_for(ca, pname_b) {
            if !ix.unique || ix.opaque || ix.columns.len() != n {
                continue;
            }
            let mut cc = AVec::new();
            let mut pc = AVec::new();
            if f.to.is_empty() {
                let Some(pk) = pk else {
                    continue;
                };
                if ix.origin != crate::schema::IndexOrigin::Auto
                    || pk.columns.len() != n
                    || pk
                        .columns
                        .iter()
                        .zip(ix.columns.iter())
                        .any(|(a, b)| a.col != b.col)
                {
                    continue;
                }
                for (i, col) in ix.columns.iter().enumerate() {
                    let Some(pcol) = col.col else {
                        continue 'idx;
                    };
                    pc.push(pcol);
                    cc.push(ccol(*f.from.as_slice().get(i)?)?);
                }
            } else {
                for col in ix.columns.iter() {
                    let Some(pcol) = col.col else {
                        continue 'idx;
                    };
                    let Some(pdef) = p.columns.as_slice().get(pcol) else {
                        continue 'idx;
                    };
                    if col.coll != pdef.coll {
                        continue 'idx;
                    }
                    let pn = handle_bytes(ca, pdef.name);
                    let Some(j) = f.to.iter().position(|h| eq_ci(handle_bytes(sc, *h), pn)) else {
                        continue 'idx;
                    };
                    pc.push(pcol);
                    cc.push(ccol(*f.from.as_slice().get(j)?)?);
                }
            }
            let mut desc = AVec::new();
            for col in ix.columns.iter() {
                desc.push(KeyOrder {
                    desc: col.desc,
                    coll: col.coll,
                });
            }
            return Some((Some((ix.root, desc)), cc, pc));
        }
        None
    }

    /// The parent side of a located key.
    fn fk_parent(
        p: &crate::schema::TableDef,
        index: Option<(u32, AVec<KeyOrder>)>,
        pcols: &[usize],
    ) -> Parent {
        let mut out = Parent {
            root: p.root,
            index,
            affs: AVec::new(),
            colls: AVec::new(),
            ipk: AVec::new(),
        };
        for c in pcols.iter() {
            let col = p.columns.as_slice().get(*c);
            out.affs.push(col.map_or(Affinity::Blob, |x| x.affinity));
            out.colls.push(col.map_or(Coll::default(), |x| x.coll));
            out.ipk.push(Some(*c) == p.rowid_alias);
        }
        if out.index.is_none()
            && let Some(a) = out.affs.as_mut_slice().first_mut()
        {
            *a = Affinity::Integer;
        }
        out
    }

    /// The foreign keys a write of `table` checks (`None`: none, or
    /// `PRAGMA foreign_keys` is off). Errors as the pin's prepare:
    /// a missing parent table, a parent key that is no PRIMARY KEY
    /// / UNIQUE index.
    pub(super) fn fk_target(
        &self,
        table: &[u8],
        ev: Event<'_>,
        multi: bool,
        ignore: bool,
        arena: &Arena,
    ) -> Result<Option<Rc<FkTarget>>, S::Error> {
        if !self.prag.foreign_keys {
            return Ok(None);
        }
        let ca = &self.catalog_arena;
        let Some(t) = self.catalog.find(ca, table) else {
            return Ok(None);
        };
        let top = !self.in_program();
        let mut out = FkTarget {
            child: AVec::new(),
            parent: AVec::new(),
            alias: t.rowid_alias,
        };
        let sc = Arena::new(Scope::Statement, 0, 1 << 20);
        // An UPDATE changing any key "requires" foreign keys: the pin
        // then locates every key referring to the table (its
        // `sqlite3FkOldmask`), so an unusable one fails it.
        let mut required = false;
        let mut unusable = false;
        // This table as the child.
        for (fi, f) in self.fk_clauses(t, &sc)?.iter().enumerate() {
            let ptab = handle_bytes(&sc, f.table);
            let selfref = eq_ci(ptab, handle_bytes(ca, t.name));
            let mut ccols = AVec::new();
            for h in f.from.iter() {
                let n = handle_bytes(&sc, *h);
                if let Some(i) = t
                    .columns
                    .iter()
                    .position(|x| eq_ci(handle_bytes(ca, x.name), n))
                {
                    ccols.push(i);
                }
            }
            if let Event::Update(assigned, rowid_set) = ev {
                let m = modified(ccols.as_slice(), assigned, rowid_set, t.rowid_alias);
                required |= m;
                if !selfref && !m {
                    continue;
                }
            }
            let Some(p) = self.catalog.find(ca, ptab) else {
                if ignore {
                    out.child.push(Fk {
                        id: (t.root, fi),
                        deferred: f.deferred,
                        on_delete: f.on_delete,
                        on_update: f.on_update,
                        ccols,
                        pcols: AVec::new(),
                        parent: None,
                        selfref,
                        child: self.fk_shape(t, arena, false)?,
                    });
                    continue;
                }
                if self.catalog.find_view(ca, ptab).is_some() {
                    return Err(mismatch());
                }
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            };
            let Some((index, ccols, pcols)) = self.fk_locate(t, p, f, &sc) else {
                if ignore {
                    continue;
                }
                return Err(mismatch());
            };
            out.child.push(Fk {
                id: (t.root, fi),
                deferred: f.deferred,
                on_delete: f.on_delete,
                on_update: f.on_update,
                parent: Some(Self::fk_parent(p, index, pcols.as_slice())),
                ccols,
                pcols,
                selfref,
                child: self.fk_shape(t, arena, false)?,
            });
        }
        // This table as the parent: every key naming it, the most
        // recently created first.
        for c in self.catalog.tables.iter().rev() {
            sc.reset();
            for (fi, f) in self.fk_clauses(c, &sc)?.iter().enumerate() {
                if !eq_ci(handle_bytes(&sc, f.table), table) {
                    continue;
                }
                let Some((index, ccols, pcols)) = self.fk_locate(c, t, f, &sc) else {
                    if ignore {
                        continue;
                    }
                    // A key that does not change is not looked at.
                    if let Event::Update(assigned, rowid_set) = ev {
                        let mut named = AVec::new();
                        for (i, x) in t.columns.iter().enumerate() {
                            let n = handle_bytes(ca, x.name);
                            if f.to.iter().any(|h| eq_ci(handle_bytes(&sc, *h), n))
                                || (f.to.is_empty()
                                    && (Some(i) == t.rowid_alias
                                        || t.auto_sets.iter().any(|s| {
                                            s.primary_key
                                                && s.columns.iter().any(|k| k.col == Some(i))
                                        })))
                            {
                                named.push(i);
                            }
                        }
                        if !modified(named.as_slice(), assigned, rowid_set, t.rowid_alias) {
                            unusable = true;
                            continue;
                        }
                    }
                    if matches!(ev, Event::Insert) && !f.deferred && !self.fk.defer && top && !multi
                    {
                        continue;
                    }
                    return Err(mismatch());
                };
                if let Event::Update(assigned, rowid_set) = ev {
                    if !modified(pcols.as_slice(), assigned, rowid_set, t.rowid_alias) {
                        continue;
                    }
                    required = true;
                }
                // A single-row INSERT into the parent can neither cause
                // nor fix an immediate violation.
                if matches!(ev, Event::Insert) && !f.deferred && !self.fk.defer && top && !multi {
                    continue;
                }
                out.parent.push(Fk {
                    id: (c.root, fi),
                    deferred: f.deferred,
                    on_delete: f.on_delete,
                    on_update: f.on_update,
                    parent: Some(Self::fk_parent(t, index, pcols.as_slice())),
                    ccols,
                    pcols,
                    selfref: eq_ci(handle_bytes(ca, c.name), table),
                    child: self.fk_shape(
                        c,
                        arena,
                        f.on_delete == SET_DEFAULT || f.on_update == SET_DEFAULT,
                    )?,
                });
            }
        }
        if unusable && required {
            return Err(mismatch());
        }
        if out.child.is_empty() && out.parent.is_empty() {
            return Ok(None);
        }
        Ok(Some(Rc::new(out)))
    }

    /// COMMIT (or a RELEASE that commits): outstanding deferred
    /// violations fail it, the transaction staying open.
    pub(super) fn fk_commit_check(&self) -> Result<(), S::Error> {
        if self.fk.pending() || self.temp.as_ref().is_some_and(|t| t.fk.pending()) {
            return Err(violation());
        }
        Ok(())
    }

    /// Run one action program's statement (`ast`, over `arena`): a
    /// frame of its own for `changes()` / `last_insert_rowid()`,
    /// under ABORT, its failure resolving the firing statement by
    /// ABORT.
    fn fk_program(
        &mut self,
        arena: Arena,
        ast: Ast,
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = Result<(), S::Error>> + '_>> {
        Box::pin(self.fk_program_frame(arena, ast))
    }

    async fn fk_program_frame(&mut self, arena: Arena, ast: Ast) -> Result<(), S::Error> {
        #[cfg(feature = "triggers")]
        self.trig.stack.push(AVec::new());
        #[cfg(not(feature = "triggers"))]
        {
            self.fk.depth += 1;
        }
        #[cfg(feature = "triggers")]
        let orconf = self.trig.orconf.replace(ConflictAction::Abort);
        let env = (
            self.env.pending.get(),
            self.env.changes.get(),
            self.env.last_rowid.get(),
        );
        let txn = self.explicit_txn;
        self.explicit_txn = true;
        let r = self.fk_program_run(arena, ast).await;
        self.explicit_txn = txn;
        self.env.pending.set(env.0);
        self.env.changes.set(env.1);
        self.env.last_rowid.set(env.2);
        #[cfg(feature = "triggers")]
        {
            self.trig.orconf = orconf;
            self.trig.stack.pop();
        }
        #[cfg(not(feature = "triggers"))]
        {
            self.fk.depth = self.fk.depth.saturating_sub(1);
        }
        if let Err(e) = &r {
            self.fk_failed(e);
        }
        r
    }

    /// An action program failed with `e`: the firing statement
    /// aborts.
    fn fk_failed(&mut self, e: &Error<S::Error>) {
        self.fk.abort = true;
        #[cfg(feature = "triggers")]
        self.note_failure(e, Some(ConflictAction::Abort), false);
        let _ = e;
    }

    async fn fk_program_run(&mut self, arena: Arena, ast: Ast) -> Result<(), S::Error> {
        self.env.pending.set(0);
        let plan = self.plan(&arena, &ast)?;
        let sql_text = arena.alloc_copy_handle(b"")?;
        #[cfg(feature = "params")]
        let binds = Binds {
            values: AVec::new(),
            table: crate::sql::lexer::ParamTable::default(),
            src: None,
            plan_ckpt: arena.checkpoint(),
            stale: false,
            started: false,
        };
        let st = Statement::assemble(
            self,
            arena,
            sql_text,
            ast,
            plan,
            #[cfg(feature = "params")]
            binds,
        );
        run_to_end(st).await
    }
}

/// Step a statement to its end, its rows discarded (boxed: the
/// statement may be a write that runs further programs).
fn run_to_end<'a, S: Storage>(
    mut st: Statement<'a, S>,
) -> core::pin::Pin<Box<dyn core::future::Future<Output = Result<(), S::Error>> + 'a>> {
    Box::pin(async move {
        loop {
            match st.step().await? {
                Step::Row => {}
                Step::Done => return Ok(()),
            }
        }
    })
}

/// One table `PRAGMA foreign_key_check` reads.
pub(super) struct CheckTable {
    shape: Shape,
    /// Each key, in list order (its fkid): the parent as written,
    /// the child columns, and how the parent is found (`None`: the
    /// parent table is missing — every row with a key violates).
    keys: AVec<(AVec<u8>, AVec<usize>, Option<Parent>)>,
}

impl<S: Storage> Connection<S> {
    /// Resolve the keys of `tables` for `foreign_key_check`.
    pub(super) fn fk_check_plan(
        &self,
        tables: &[AVec<u8>],
        arena: &Arena,
    ) -> Result<AVec<CheckTable>, S::Error> {
        let ca = &self.catalog_arena;
        let mut out = AVec::new();
        let sc = Arena::new(Scope::Statement, 0, 1 << 20);
        for name in tables.iter() {
            let Some(t) = self.catalog.find(ca, name.as_slice()) else {
                continue;
            };
            sc.reset();
            let fks = self.fk_clauses(t, &sc)?;
            if fks.is_empty() {
                continue;
            }
            let mut keys = AVec::new();
            for f in fks.iter() {
                let ptab = handle_bytes(&sc, f.table);
                let parent = copy_vec(ptab);
                match self.catalog.find(ca, ptab) {
                    None => {
                        if self.catalog.find_view(ca, ptab).is_some() {
                            return Err(mismatch());
                        }
                        let mut ccols = AVec::new();
                        for h in f.from.iter() {
                            let n = handle_bytes(&sc, *h);
                            if let Some(i) = t
                                .columns
                                .iter()
                                .position(|x| eq_ci(handle_bytes(ca, x.name), n))
                            {
                                ccols.push(i);
                            }
                        }
                        keys.push((parent, ccols, None));
                    }
                    Some(p) => {
                        let (index, ccols, pcols) =
                            self.fk_locate(t, p, f, &sc).ok_or_else(mismatch)?;
                        keys.push((
                            parent,
                            ccols,
                            Some(Self::fk_parent(p, index, pcols.as_slice())),
                        ));
                    }
                }
            }
            out.push(CheckTable {
                shape: self.fk_shape(t, arena, false)?,
                keys,
            });
        }
        Ok(out)
    }
}

/// `foreign_key_check`'s violations: (table, rowid, parent as
/// written, fkid).
#[allow(clippy::type_complexity)]
pub(super) async fn fk_check_rows<S: Storage>(
    pager: &mut Pager<S>,
    plan: &[CheckTable],
) -> Result<AVec<(AVec<u8>, i64, AVec<u8>, usize)>, S::Error> {
    let mut out = AVec::new();
    for ct in plan.iter() {
        let sh = &ct.shape;
        let gcols = sh.rules.as_ref().and_then(|r| r.gcols.as_ref());
        let mut cursor = ScanCursor::new(sh.root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(pager, &mut buf).await? {
            let mut rec = crate::rules::decode_rec::<S::Error>(
                buf.as_slice(),
                sh.ncols,
                sh.defaults.as_slice(),
                gcols.map(|g| g.virt.as_slice()),
            )?;
            if let Some(g) = gcols {
                g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
            }
            let row = row_data_for(sh.alias, sh.affs.as_slice(), rec.as_slice(), rowid).vals;
            for (id, (parent, ccols, p)) in ct.keys.iter().enumerate() {
                let mut vals = AVec::new();
                for i in 0..ccols.len() {
                    vals.push(key_val(row.as_slice(), ccols.as_slice(), i));
                }
                if vals.iter().any(|v| matches!(v, OwnedValue::Null)) {
                    continue;
                }
                let ok = match p {
                    Some(p) => parent_found(pager, p, vals.as_slice(), None).await?,
                    None => false,
                };
                if !ok {
                    out.push((sh.name.clone(), rowid, parent.clone(), id));
                }
            }
        }
    }
    Ok(out)
}

/// Whether parent key `vals` (child values) has a parent row: the
/// values take the parent columns' affinities and are looked up in
/// the parent index, or as the rowid (the pin's `MustBeInt`: a
/// value that is no integer has none). `excl`: a parent row to
/// treat as gone.
async fn parent_found<S: Storage>(
    pager: &mut Pager<S>,
    p: &Parent,
    vals: &[OwnedValue],
    excl: Option<i64>,
) -> Result<bool, S::Error> {
    match &p.index {
        None => {
            let v = vals.first().cloned().unwrap_or(OwnedValue::Null);
            Ok(match apply_affinity(v, Affinity::Integer) {
                OwnedValue::Integer(k) => {
                    Some(k) != excl && btree::seek_rowid(pager, p.root, k).await?.is_some()
                }
                _ => false,
            })
        }
        Some((root, desc)) => {
            let mut key = AVec::new();
            for (i, v) in vals.iter().enumerate() {
                let aff = p.affs.as_slice().get(i).copied().unwrap_or(Affinity::Blob);
                key.push(apply_affinity(v.clone(), aff));
            }
            let rowids =
                btree::index_prefix_rowids(pager, *root, desc.as_slice(), key.as_slice()).await?;
            Ok(rowids.iter().any(|r| Some(*r) != excl))
        }
    }
}

/// Whether an UPDATE OR REPLACE compiles REPLACE deletions on the
/// pin: a new rowid, a UNIQUE index whose columns it assigns, or —
/// when it changes a self-referencing child key or a parent key with
/// an ON UPDATE action (`tg`, its loaded keys) — any UNIQUE index.
fn update_replaces(plan: &Plan, tg: Option<&FkTarget>, assigned: &[bool], rowid_set: bool) -> bool {
    let Plan::Update {
        indexes, conflict, ..
    } = plan
    else {
        return false;
    };
    if *conflict != ConflictAction::Replace {
        return false;
    }
    let alias = tg.and_then(|t| t.alias);
    let all = tg.is_some_and(|tg| {
        tg.child
            .iter()
            .any(|f| f.selfref && modified(f.ccols.as_slice(), assigned, rowid_set, alias))
            || tg.parent.iter().any(|f| f.on_update != 0)
    });
    rowid_set
        || indexes.iter().any(|ix| {
            ix.unique
                && (all
                    || ix.cols.iter().any(|c| {
                        c.col
                            .is_some_and(|i| assigned.get(i).copied().unwrap_or(false))
                    }))
        })
}

/// Whether an INSERT OR REPLACE compiles REPLACE deletions on the
/// pin: it supplies the rowid, or the table has a UNIQUE index.
fn insert_replaces(plan: &Plan) -> bool {
    let Plan::Insert {
        conflict,
        targets,
        rowid_alias,
        indexes,
        ..
    } = plan
    else {
        return false;
    };
    *conflict == ConflictAction::Replace
        && (indexes.iter().any(|ix| ix.unique)
            || targets.iter().any(|t| match t {
                Target::Rowid => true,
                Target::Col(i) => Some(*i) == *rowid_alias,
            }))
}

/// `Statement::insert_multi` but for triggers.
fn insert_plan_multi(plan: &Plan) -> bool {
    match plan {
        Plan::Insert {
            rows_v,
            rows_r,
            select,
            conflict,
            upsert,
            returning,
            ..
        } => {
            select.is_some()
                || rows_v.len() > 1
                || rows_r.as_ref().is_some_and(|r| r.len() > 1)
                || *conflict == ConflictAction::Replace
                || upsert.as_ref().is_some_and(|u| u.do_update)
                || !returning.is_empty()
        }
        _ => true,
    }
}

impl<S: Storage> Connection<S> {
    /// Resolve the keys a write will use when it is prepared (the
    /// pin's prepare-time "foreign key mismatch" / "no such table"),
    /// with every action program it compiles — the pin compiles the
    /// whole tree then: a CASCADE's DELETE of the child table, a SET
    /// NULL's UPDATE of it, and theirs in turn.
    pub(super) fn fk_validate(
        &self,
        ast: &Ast,
        plan: &Plan,
        arena: &Arena,
    ) -> Result<(), S::Error> {
        if !self.prag.foreign_keys {
            return Ok(());
        }
        let body = match ast {
            Ast::With { body, .. } => body.as_ref(),
            a => a,
        };
        let ckpt = arena.checkpoint();
        let r = self.fk_validate_inner(body, plan, arena);
        arena.rewind(ckpt);
        r
    }

    fn fk_validate_inner(&self, body: &Ast, plan: &Plan, arena: &Arena) -> Result<(), S::Error> {
        // The work list: (table, an UPDATE's assigned columns and
        // rowid flag — `None`: a DELETE —, multi).
        let mut todo: AVec<(AVec<u8>, Option<Assigned>)> = AVec::new();
        match (body, plan) {
            (Ast::Insert { table, .. }, Plan::Insert { .. }) => {
                let name = copy_vec(handle_bytes(arena, *table));
                #[cfg(feature = "triggers")]
                let trig = self
                    .catalog
                    .triggers_on(&self.catalog_arena, name.as_slice())
                    .any(|t| t.event == crate::sql::ast::TrigEvent::Insert);
                #[cfg(not(feature = "triggers"))]
                let trig = false;
                let multi = trig || insert_plan_multi(plan) || self.in_program();
                self.fk_target(name.as_slice(), Event::Insert, multi, false, arena)?;
                if insert_replaces(plan) {
                    todo.push((name, None));
                }
            }
            (
                Ast::Update { table, .. },
                Plan::Update {
                    sets,
                    ncols,
                    rowid_alias,
                    ..
                },
            ) => {
                let name = copy_vec(handle_bytes(arena, *table));
                let (assigned, rowid_set) = assigned_columns(sets.as_slice(), *ncols, *rowid_alias);
                let tg = self.fk_target(
                    name.as_slice(),
                    Event::Update(assigned.as_slice(), rowid_set),
                    true,
                    false,
                    arena,
                )?;
                if update_replaces(plan, tg.as_deref(), assigned.as_slice(), rowid_set) {
                    todo.push((name.clone(), None));
                }
                todo.push((name, Some((assigned, rowid_set))));
            }
            (Ast::Delete { table, .. }, Plan::Delete { .. }) => {
                todo.push((copy_vec(handle_bytes(arena, *table)), None));
            }
            _ => {}
        }
        // DROP TABLE's DELETE ignores its own unusable keys (its
        // programs check as any statement).
        let mut ignore = self.fk.ignore;
        let mut seen: AVec<((u32, usize), bool)> = AVec::new();
        while let Some((name, upd)) = todo.pop() {
            let ev = match &upd {
                Some((a, r)) => Event::Update(a.as_slice(), *r),
                None => Event::Delete,
            };
            let tg = self.fk_target(name.as_slice(), ev, true, ignore, arena)?;
            ignore = false;
            let Some(tg) = tg else {
                continue;
            };
            for f in tg.parent.iter() {
                let act = if upd.is_some() {
                    f.on_update
                } else {
                    f.on_delete
                };
                if act == 0 || act == RESTRICT {
                    continue;
                }
                let key = (f.id, upd.is_some());
                if seen.iter().any(|k| *k == key) {
                    continue;
                }
                seen.push(key);
                if act == CASCADE && upd.is_none() {
                    todo.push((f.child.name.clone(), None));
                } else {
                    let mut a = AVec::new();
                    for i in 0..f.child.ncols {
                        a.push(f.ccols.as_slice().contains(&i));
                    }
                    let r = f.ccols.iter().any(|c| Some(*c) == f.child.alias);
                    todo.push((f.child.name.clone(), Some((a, r))));
                }
            }
        }
        Ok(())
    }
}

/// An `OLD.` / `NEW.` value of an action program: no comparison
/// affinity and the parent column's collation; the INTEGER PRIMARY
/// KEY reads as the rowid (INTEGER affinity, no collation).
fn trig_value<E>(v: &OwnedValue, ipk: bool, coll: Coll, arena: &Arena) -> Result<Expr, E> {
    let lit = match v {
        OwnedValue::Null => Literal::Null,
        OwnedValue::Integer(i) => Literal::Integer(*i),
        OwnedValue::Real(f) => Literal::Real(*f),
        OwnedValue::Text(t) => Literal::Text(arena.alloc_copy_handle(t.as_slice())?),
        OwnedValue::Blob(t) => Literal::Blob(arena.alloc_copy_handle(t.as_slice())?),
    };
    Ok(if ipk {
        Expr::TrigValue(lit, Some(Affinity::Integer), None)
    } else {
        Expr::TrigValue(lit, None, Some(coll))
    })
}

fn literal_of<E>(v: &OwnedValue, arena: &Arena) -> Result<Expr, E> {
    Ok(Expr::Literal(match v {
        OwnedValue::Null => Literal::Null,
        OwnedValue::Integer(i) => Literal::Integer(*i),
        OwnedValue::Real(f) => Literal::Real(*f),
        OwnedValue::Text(t) => Literal::Text(arena.alloc_copy_handle(t.as_slice())?),
        OwnedValue::Blob(t) => Literal::Blob(arena.alloc_copy_handle(t.as_slice())?),
    }))
}

/// The value of key position `i` of `row` (a logical row: the rowid
/// alias reads as the rowid).
fn key_val(row: &[OwnedValue], cols: &[usize], i: usize) -> OwnedValue {
    cols.get(i)
        .and_then(|c| row.get(*c))
        .cloned()
        .unwrap_or(OwnedValue::Null)
}

impl<'c, S: Storage> Statement<'c, S> {
    /// Load this write's foreign keys (`self.fk`), and note whether
    /// it is a multi-row write (the pin's `isMultiWrite`).
    pub(super) fn fk_load(&mut self, ev: Event<'_>, multi: bool) -> Result<(), S::Error> {
        self.fk = None;
        let ast = match &self.ast {
            Ast::With { body, .. } => body.as_ref(),
            a => a,
        };
        let table = match ast {
            Ast::Insert { table, .. } | Ast::Update { table, .. } | Ast::Delete { table, .. } => {
                *table
            }
            Ast::DropTable { name, .. } => *name,
            _ => return Ok(()),
        };
        let name = copy_vec(handle_bytes(&self.arena, table));
        if !self.conn.in_program() {
            self.conn.fk.multi = multi;
        }
        let multi = self.conn.fk.multi || self.conn.in_program();
        // DROP TABLE's DELETE (its first load only: the action
        // programs it runs check as any statement does).
        let ignore = core::mem::take(&mut self.conn.fk.ignore);
        self.fk = self
            .conn
            .fk_target(name.as_slice(), ev, multi, ignore, &self.arena)?;
        Ok(())
    }

    /// An UPDATE OR REPLACE: the keys of the rows its conflicts
    /// delete (`self.fk_del`).
    pub(super) fn fk_load_victims(&mut self) -> Result<(), S::Error> {
        self.fk_del = None;
        let Ast::Update { table, .. } = (match &self.ast {
            Ast::With { body, .. } => body.as_ref(),
            a => a,
        }) else {
            return Ok(());
        };
        let name = copy_vec(handle_bytes(&self.arena, *table));
        self.fk_del =
            self.conn
                .fk_target(name.as_slice(), Event::Delete, true, false, &self.arena)?;
        Ok(())
    }

    /// A top-level INSERT / UPDATE whose REPLACE conflicts may delete
    /// rows (`replace`): the pin compiles those deletions' ON DELETE
    /// action programs before it looks up the new row's parents,
    /// and its `isSetNullAction` then mistakes the last one compiled
    /// for the program running — when that is a self-referencing
    /// key's SET NULL, the new rows' lookups of that key are skipped
    /// (probed: `INSERT OR REPLACE INTO t VALUES (1, 99)` over
    /// `t(id INTEGER PRIMARY KEY, par REFERENCES t ON DELETE SET
    /// NULL)` succeeds with 99 missing).
    pub(super) fn fk_note_replace(&mut self, replace: bool) {
        if self.conn.in_program() {
            return;
        }
        self.conn.fk.skip = None;
        if !replace {
            return;
        }
        let Some(tg) = self.fk_del.clone().or_else(|| self.fk.clone()) else {
            return;
        };
        let defer = self.conn.fk.defer;
        let last = tg
            .parent
            .iter()
            .rev()
            .find(|f| f.on_delete != 0 && !(f.on_delete == RESTRICT && defer));
        let Some(f) = last.filter(|f| f.selfref && f.on_delete == SET_NULL) else {
            return;
        };
        // Compiling that program compiles programs of its own — the
        // table's UPDATE triggers on those columns, ON UPDATE actions
        // of keys on them — which come last instead.
        let nested_action = tg.parent.iter().any(|g| {
            g.on_update != 0
                && !(g.on_update == RESTRICT && defer)
                && g.pcols.iter().any(|c| f.ccols.as_slice().contains(c))
        });
        #[cfg(feature = "triggers")]
        let nested_trigger = {
            let ca = &self.conn.catalog_arena;
            let mut names: AVec<&[u8]> = AVec::new();
            for c in f.ccols.iter() {
                if let Some(n) = f.child.names.as_slice().get(*c) {
                    names.push(n.as_slice());
                }
            }
            self.conn
                .catalog
                .triggers_on(ca, f.child.name.as_slice())
                .any(|t| {
                    t.event == crate::sql::ast::TrigEvent::Update
                        && (t.of_cols.is_empty()
                            || t.of_cols
                                .iter()
                                .any(|h| names.iter().any(|n| eq_ci(handle_bytes(ca, *h), n))))
                })
        };
        #[cfg(not(feature = "triggers"))]
        let nested_trigger = false;
        if !nested_action && !nested_trigger {
            self.conn.fk.skip = Some(f.id);
        }
    }

    /// Whether an UPDATE OR REPLACE compiles REPLACE deletions (see
    /// `update_replaces`).
    pub(super) fn fk_update_replace(&self, assigned: &[bool], rowid_set: bool) -> bool {
        update_replaces(&self.plan, self.fk.as_deref(), assigned, rowid_set)
    }

    /// Whether this write checks foreign keys (its rows must then be
    /// read again at their turn: an action may have changed them).
    pub(super) fn fk_active(&self) -> bool {
        self.fk.is_some()
    }

    /// Whether an INSERT may write several rows (the pin's
    /// `isMultiWrite`: a SELECT or multi-row VALUES source, triggers
    /// or RETURNING, a REPLACE or upsert update).
    pub(super) fn insert_multi(&self) -> bool {
        #[cfg(feature = "triggers")]
        {
            use crate::sql::ast::{TrigEvent, TrigTime};
            if self.fires(TrigTime::Before, TrigEvent::Insert)
                || self.fires(TrigTime::After, TrigEvent::Insert)
            {
                return true;
            }
        }
        insert_plan_multi(&self.plan)
    }

    /// The write plan's column affinities.
    pub(super) fn plan_affs(&self) -> AVec<Affinity> {
        match &self.plan {
            Plan::Insert { cols, .. } => {
                let mut out = AVec::new();
                for c in cols.iter() {
                    out.push(c.affinity);
                }
                out
            }
            Plan::Update { affs, .. } | Plan::Delete { affs, .. } => affs.clone(),
            _ => AVec::new(),
        }
    }

    /// Whether an INSERT OR REPLACE compiles REPLACE deletions (see
    /// `insert_replaces`).
    pub(super) fn insert_replaces(&self) -> bool {
        insert_replaces(&self.plan)
    }

    /// The logical row of the written table (the rowid alias reads
    /// as the rowid, REAL columns as reals).
    pub(super) fn fk_row(
        &self,
        rec: &[OwnedValue],
        rowid: i64,
        affs: &[Affinity],
    ) -> AVec<OwnedValue> {
        let alias = self.fk.as_ref().and_then(|t| t.alias);
        row_data_for(alias, affs, rec, rowid).vals
    }

    /// The checks of a row added to the table (`row` logical, at
    /// `rowid`; `excl`: a row of the table still stored that is
    /// being replaced — the pin has deleted it by now).
    /// `changed`: an update's assigned columns and whether it
    /// assigns the rowid — keys it does not change are not looked
    /// at.
    pub(super) async fn fk_new_row(
        &mut self,
        row: &[OwnedValue],
        rowid: i64,
        excl: Option<i64>,
        changed: Option<(&[bool], bool)>,
    ) -> Result<(), S::Error> {
        let Some(tg) = self.fk.clone() else {
            return Ok(());
        };
        let skip = if self.conn.in_program() {
            None
        } else {
            self.conn.fk.skip
        };
        for f in tg.child.iter() {
            if !Self::fk_child_changes(&tg, f, changed) || skip == Some(f.id) {
                continue;
            }
            self.fk_lookup(f, row, rowid, 1, excl).await?;
        }
        for f in tg.parent.iter() {
            if !Self::fk_parent_changes(&tg, f, changed) {
                continue;
            }
            self.fk_scan(f, row, rowid, -1, excl).await?;
        }
        Ok(())
    }

    /// The checks of a row removed from the table (still stored).
    pub(super) async fn fk_old_row(
        &mut self,
        row: &[OwnedValue],
        rowid: i64,
        changed: Option<(&[bool], bool)>,
    ) -> Result<(), S::Error> {
        let Some(tg) = self.fk.clone() else {
            return Ok(());
        };
        for f in tg.child.iter() {
            if !Self::fk_child_changes(&tg, f, changed) {
                continue;
            }
            self.fk_lookup(f, row, rowid, -1, None).await?;
        }
        for f in tg.parent.iter() {
            if !Self::fk_parent_changes(&tg, f, changed) {
                continue;
            }
            self.fk_scan(f, row, rowid, 1, None).await?;
        }
        Ok(())
    }

    /// The pin's `fkChildIsModified` (a self-referencing key is
    /// always looked at).
    fn fk_child_changes(tg: &FkTarget, f: &Fk, changed: Option<(&[bool], bool)>) -> bool {
        match changed {
            Some((cols, rowid_set)) => {
                f.selfref || modified(f.ccols.as_slice(), cols, rowid_set, tg.alias)
            }
            None => true,
        }
    }

    /// The pin's `fkParentIsModified`.
    fn fk_parent_changes(tg: &FkTarget, f: &Fk, changed: Option<(&[bool], bool)>) -> bool {
        match changed {
            Some((cols, rowid_set)) => modified(f.pcols.as_slice(), cols, rowid_set, tg.alias),
            None => true,
        }
    }

    /// The pin's `fkLookupParent`: the parent of child row `row` for
    /// key `f`, counted `n` (+1 added, -1 removed) when missing.
    async fn fk_lookup(
        &mut self,
        f: &Fk,
        row: &[OwnedValue],
        rowid: i64,
        n: i64,
        excl: Option<i64>,
    ) -> Result<(), S::Error> {
        let mut vals = AVec::new();
        for i in 0..f.ccols.len() {
            let v = key_val(row, f.ccols.as_slice(), i);
            if matches!(v, OwnedValue::Null) {
                return Ok(());
            }
            vals.push(v);
        }
        let Some(p) = &f.parent else {
            // DROP TABLE's DELETE: a missing parent is empty.
            if n < 0 {
                self.conn.fk.count(f.deferred, n);
            }
            return Ok(());
        };
        if n < 0 && self.conn.fk.zero(f.deferred) {
            return Ok(());
        }
        let excl = if f.selfref { excl } else { None };
        // A self-referencing row being added may be its own parent.
        let own = f.selfref
            && n > 0
            && match &p.index {
                None => matches!(
                    apply_affinity(
                        vals.as_slice().first().cloned().unwrap_or(OwnedValue::Null),
                        Affinity::Integer
                    ),
                    OwnedValue::Integer(k) if k == rowid
                ),
                Some(_) => (0..f.ccols.len()).all(|i| {
                    let a = key_val(row, f.ccols.as_slice(), i);
                    let b = key_val(row, f.pcols.as_slice(), i);
                    !matches!(b, OwnedValue::Null)
                        && crate::record::compare_values(&a.as_value(), &b.as_value())
                            == core::cmp::Ordering::Equal
                }),
            };
        let found = own || parent_found(&mut self.conn.pager, p, vals.as_slice(), excl).await?;
        if found {
            return Ok(());
        }
        if n > 0
            && !f.deferred
            && !self.conn.fk.defer
            && !self.conn.in_program()
            && !self.conn.fk.multi
        {
            return Err(violation());
        }
        self.conn.fk.count(f.deferred, n);
        Ok(())
    }

    /// The child rows of key `f` matching parent key values `pvals`
    /// as `matches` compares them (`skip`: rowids not scanned).
    async fn fk_children(
        &mut self,
        f: &Fk,
        pvals: &[OwnedValue],
        skip: &[i64],
        action: bool,
    ) -> Result<AVec<i64>, S::Error> {
        let mut out = AVec::new();
        let Some(p) = &f.parent else {
            return Ok(out);
        };
        let sh = &f.child;
        let gcols = sh.rules.as_ref().and_then(|r| r.gcols.as_ref());
        let mut cursor = ScanCursor::new(sh.root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            if skip.contains(&rowid) {
                continue;
            }
            let mut rec = crate::rules::decode_rec::<S::Error>(
                buf.as_slice(),
                sh.ncols,
                sh.defaults.as_slice(),
                gcols.map(|g| g.virt.as_slice()),
            )?;
            if let Some(g) = gcols {
                g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
            }
            let row = row_data_for(sh.alias, sh.affs.as_slice(), rec.as_slice(), rowid).vals;
            let hit = (0..f.ccols.len()).all(|i| {
                let cv = key_val(row.as_slice(), f.ccols.as_slice(), i);
                let caff = f
                    .ccols
                    .as_slice()
                    .get(i)
                    .and_then(|c| sh.affs.as_slice().get(*c))
                    .copied()
                    .unwrap_or(Affinity::Blob);
                let ccoll = f
                    .ccols
                    .as_slice()
                    .get(i)
                    .and_then(|c| sh.colls.as_slice().get(*c))
                    .copied()
                    .unwrap_or_default();
                let pv = pvals.get(i).cloned().unwrap_or(OwnedValue::Null);
                let ipk = p.ipk.as_slice().get(i).copied().unwrap_or(false);
                let pcoll = p.colls.as_slice().get(i).copied().unwrap_or_default();
                if action {
                    // `OLD.pk = fk`: no affinity on OLD (INTEGER for
                    // the rowid), OLD's collation.
                    let (la, coll) = if ipk {
                        (Some(Affinity::Integer), ccoll)
                    } else {
                        (None, pcoll)
                    };
                    crate::query::fk_eq(&pv, la, &cv, Some(caff), coll)
                } else {
                    // The pin's register `= fk`: the parent column's
                    // affinity and collation.
                    let paff = p.affs.as_slice().get(i).copied().unwrap_or(Affinity::Blob);
                    let coll = if ipk && p.index.is_none() {
                        ccoll
                    } else {
                        pcoll
                    };
                    crate::query::fk_eq(&pv, Some(paff), &cv, Some(caff), coll)
                }
            });
            if hit {
                out.push(rowid);
            }
        }
        Ok(out)
    }

    /// The pin's `fkScanChildren`: the child rows referring to
    /// parent row `row`, each counted `n` (+1: the parent goes, -1:
    /// it arrives).
    async fn fk_scan(
        &mut self,
        f: &Fk,
        row: &[OwnedValue],
        rowid: i64,
        n: i64,
        excl: Option<i64>,
    ) -> Result<(), S::Error> {
        if n < 0 && self.conn.fk.zero(f.deferred) {
            return Ok(());
        }
        let mut pvals = AVec::new();
        for i in 0..f.pcols.len() {
            let v = key_val(row, f.pcols.as_slice(), i);
            if matches!(v, OwnedValue::Null) {
                return Ok(());
            }
            pvals.push(v);
        }
        let mut skip = AVec::new();
        if f.selfref {
            if n > 0 {
                skip.push(rowid);
            }
            if let Some(x) = excl {
                skip.push(x);
            }
        }
        let hits = self
            .fk_children(f, pvals.as_slice(), skip.as_slice(), false)
            .await?;
        for _ in hits.iter() {
            self.conn.fk.count(f.deferred, n);
        }
        Ok(())
    }

    /// The ON DELETE (`new` None) / ON UPDATE actions after parent
    /// row `old` (logical) changed.
    pub(super) async fn fk_actions(
        &mut self,
        old: &[OwnedValue],
        new: Option<&[OwnedValue]>,
        changed: Option<(&[bool], bool)>,
    ) -> Result<(), S::Error> {
        let Some(tg) = self.fk.clone() else {
            return Ok(());
        };
        for f in tg.parent.iter() {
            if !Self::fk_parent_changes(&tg, f, changed) {
                continue;
            }
            let act = if new.is_some() {
                f.on_update
            } else {
                f.on_delete
            };
            if act == 0 || (act == RESTRICT && self.conn.fk.defer) {
                continue;
            }
            let Some(p) = &f.parent else {
                continue;
            };
            // The program counts before its WHEN clause, as a
            // trigger's.
            if self.conn.program_depth() >= self.conn.cfg.max_trigger_depth {
                let e = Error::Schema {
                    kind: SchemaKind::TriggerDepth,
                };
                self.conn.fk_failed(&e);
                return Err(e);
            }
            // ON UPDATE: `WHEN NOT (old.pk IS new.pk)`.
            if let Some(new) = new
                && (0..f.pcols.len()).all(|i| {
                    let a = key_val(old, f.pcols.as_slice(), i);
                    let b = key_val(new, f.pcols.as_slice(), i);
                    let coll = p.colls.as_slice().get(i).copied().unwrap_or_default();
                    let ipk = p.ipk.as_slice().get(i).copied().unwrap_or(false);
                    crate::query::values_equal_coll(
                        &a,
                        &b,
                        if ipk { Coll::default() } else { coll },
                    )
                })
            {
                continue;
            }
            let mut oldv = AVec::new();
            for i in 0..f.pcols.len() {
                oldv.push(key_val(old, f.pcols.as_slice(), i));
            }
            if act == RESTRICT {
                let hits = self.fk_children(f, oldv.as_slice(), &[], true).await?;
                if !hits.is_empty() {
                    let e = violation();
                    self.conn.fk_failed(&e);
                    return Err(e);
                }
                continue;
            }
            let arena = Arena::new(
                Scope::Statement,
                self.conn.cfg.stmt_init,
                self.conn.cfg.stmt_max,
            );
            let ast = Self::fk_action_ast(f, p, act, oldv.as_slice(), new, &arena)?;
            self.conn.fk_program(arena, ast).await?;
        }
        Ok(())
    }

    /// The statement of one action program.
    fn fk_action_ast(
        f: &Fk,
        p: &Parent,
        act: u8,
        oldv: &[OwnedValue],
        new: Option<&[OwnedValue]>,
        arena: &Arena,
    ) -> Result<Ast, S::Error> {
        let sh = &f.child;
        let col = |i: usize| -> Result<crate::arena::ABytes, S::Error> {
            let c = f.ccols.as_slice().get(i).copied().unwrap_or(0);
            let n = sh.names.as_slice().get(c).map_or(&[][..], |n| n.as_slice());
            Ok(arena.alloc_copy_handle(n)?)
        };
        let mut filter: Option<Expr> = None;
        for (i, v) in oldv.iter().enumerate() {
            let ipk = p.ipk.as_slice().get(i).copied().unwrap_or(false);
            let coll = p.colls.as_slice().get(i).copied().unwrap_or_default();
            let eq = Expr::Binary(
                BinOp::Eq,
                Box::new(trig_value::<S::Error>(v, ipk, coll, arena)?),
                Box::new(Expr::Column {
                    table: None,
                    name: col(i)?,
                }),
            );
            filter = Some(match filter {
                None => eq,
                Some(prev) => Expr::Binary(BinOp::And, Box::new(prev), Box::new(eq)),
            });
        }
        let table = arena.alloc_copy_handle(sh.name.as_slice())?;
        if act == CASCADE && new.is_none() {
            return Ok(Ast::Delete {
                table,
                filter,
                returning: AVec::new(),
            });
        }
        Self::fk_update_ast(f, p, act, new, table, filter, arena)
    }

    /// An action's UPDATE: CASCADE sets the new key, SET NULL NULL,
    /// SET DEFAULT each column's default.
    fn fk_update_ast(
        f: &Fk,
        p: &Parent,
        act: u8,
        new: Option<&[OwnedValue]>,
        table: crate::arena::ABytes,
        filter: Option<Expr>,
        arena: &Arena,
    ) -> Result<Ast, S::Error> {
        let sh = &f.child;
        let mut assignments = AVec::new();
        for i in 0..f.ccols.len() {
            let c = f.ccols.as_slice().get(i).copied().unwrap_or(0);
            let n = sh.names.as_slice().get(c).map_or(&[][..], |n| n.as_slice());
            let name = arena.alloc_copy_handle(n)?;
            let value = match (act, new) {
                (CASCADE, Some(new)) => {
                    let ipk = p.ipk.as_slice().get(i).copied().unwrap_or(false);
                    let coll = p.colls.as_slice().get(i).copied().unwrap_or_default();
                    trig_value::<S::Error>(&key_val(new, f.pcols.as_slice(), i), ipk, coll, arena)?
                }
                (SET_DEFAULT, _) => literal_of::<S::Error>(
                    sh.dflt.as_slice().get(c).unwrap_or(&OwnedValue::Null),
                    arena,
                )?,
                _ => Expr::Literal(Literal::Null),
            };
            assignments.push((name, value));
        }
        Ok(Ast::Update {
            table,
            alias: None,
            conflict: ConflictAction::Abort,
            assignments,
            from: AVec::new(),
            filter,
            returning: AVec::new(),
        })
    }

    /// The end of a top-level write: its immediate violations fail
    /// it; in autocommit so do the transaction's.
    pub(super) fn fk_end(&self) -> Result<(), S::Error> {
        if self.conn.in_program() {
            return Ok(());
        }
        if self.conn.fk.stmt > 0 || (!self.conn.explicit_txn && self.conn.fk.pending()) {
            return Err(violation());
        }
        Ok(())
    }

    /// `fk_end`, then an autocommit write's commit — unless it has
    /// RETURNING rows and deferred violations: those fail it after
    /// its rows (`Statement::serve_returning_fk`), as on the pin.
    pub(super) async fn fk_end_commit(&mut self) -> Result<(), S::Error> {
        if !self.conn.in_program()
            && !self.conn.explicit_txn
            && self.conn.fk.stmt <= 0
            && self.conn.fk.pending()
            && self.has_returning()
        {
            self.conn.fk.late = true;
            return Ok(());
        }
        self.fk_end()?;
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// A top-level statement begins.
    pub(super) fn fk_stmt_begin(&mut self) {
        if self.phase != Phase::Fresh || self.conn.in_program() {
            return;
        }
        let st = &mut self.conn.fk;
        st.stmt = 0;
        st.start = (st.deferred, st.deferred_imm);
        st.kept = false;
        st.abort = false;
        st.multi = false;
        st.late = false;
        st.skip = None;
    }

    /// A top-level statement's step ended (`failed`; `was_txn`: a
    /// transaction was open before it).
    pub(super) fn fk_stmt_end(&mut self, failed: bool, was_txn: bool) {
        if self.conn.in_program() {
            return;
        }
        let touches = self.touches_db();
        let st = &mut self.conn.fk;
        if failed && !st.kept {
            st.deferred = st.start.0;
            st.deferred_imm = st.start.1;
        }
        st.stmt = 0;
        if !self.conn.explicit_txn {
            st.end_txn();
            // The pin clears `defer_foreign_keys` whenever a
            // transaction ends.
            if touches || was_txn {
                st.defer = false;
            }
        }
    }

    /// Whether the statement opens a transaction on the pin (reads
    /// or writes the file): pragmas and table-less queries do not.
    fn touches_db(&self) -> bool {
        match &self.plan {
            Plan::Pragma(_) => false,
            Plan::Query(_) => match &self.ast {
                Ast::Select(s) => !s.from.is_empty(),
                _ => true,
            },
            _ => true,
        }
    }

    /// DROP TABLE of `name` with foreign keys on: the pin's implicit
    /// `DELETE FROM name` first (its triggers silent, missing
    /// parents empty), when the table is a parent, or a child with
    /// deferred violations outstanding; immediate violations then
    /// fail the drop before anything is dropped.
    /// When the pin compiles that DELETE (`FkState::drop_counted`),
    /// the drop's `changes()` is its count (0 when skipped, or when
    /// the drop fails).
    pub(super) async fn fk_drop(&mut self, name: &[u8]) -> Result<(), S::Error> {
        self.conn.fk.drop_counted = false;
        if !self.conn.prag.foreign_keys {
            return Ok(());
        }
        let ca = &self.conn.catalog_arena;
        let Some(t) = self.conn.catalog.find(ca, name) else {
            return Ok(());
        };
        let sc = Arena::new(Scope::Statement, 0, 1 << 20);
        let mut parent = false;
        for c in self.conn.catalog.tables.iter() {
            sc.reset();
            if self
                .conn
                .fk_clauses(c, &sc)?
                .iter()
                .any(|f| eq_ci(handle_bytes(&sc, f.table), name))
            {
                parent = true;
                break;
            }
        }
        if !parent {
            sc.reset();
            let deferred =
                self.conn.fk.defer || self.conn.fk_clauses(t, &sc)?.iter().any(|f| f.deferred);
            if !deferred {
                return Ok(());
            }
            if self.conn.fk.zero(true) {
                self.conn.fk.drop_counted = true;
                self.conn.env.pending.set(0);
                return Ok(());
            }
        }
        self.conn.fk.drop_counted = true;
        let mut sql = AVec::new();
        sql.extend_from_slice(b"DELETE FROM \"");
        for b in handle_bytes(ca, t.name).iter() {
            if *b == b'"' {
                sql.push(b'"');
            }
            sql.push(*b);
        }
        sql.push(b'"');
        let arena = Arena::new(
            Scope::Statement,
            self.conn.cfg.stmt_init,
            self.conn.cfg.stmt_max,
        );
        let ast = crate::sql::parse::<S::Error>(sql.as_slice(), &arena, self.conn.cfg.limits())?;
        self.conn.fk.ignore = true;
        let plan = self.conn.plan(&arena, &ast);
        self.conn.fk.ignore = false;
        let plan = plan?;
        let sql_text = arena.alloc_copy_handle(sql.as_slice())?;
        #[cfg(feature = "params")]
        let binds = Binds {
            values: AVec::new(),
            table: crate::sql::lexer::ParamTable::default(),
            src: None,
            plan_ckpt: arena.checkpoint(),
            stale: false,
            started: false,
        };
        self.conn.fk.nest += 1;
        self.conn.fk.ignore = true;
        self.conn.fk.silent = true;
        let txn = self.conn.explicit_txn;
        self.conn.explicit_txn = true;
        let changes = self.conn.env.changes.get();
        let st = Statement::assemble(
            &mut *self.conn,
            arena,
            sql_text,
            ast,
            plan,
            #[cfg(feature = "params")]
            binds,
        );
        let r = run_to_end(st).await;
        self.conn.explicit_txn = txn;
        self.conn.fk.ignore = false;
        self.conn.fk.silent = false;
        self.conn.fk.nest = self.conn.fk.nest.saturating_sub(1);
        // Its rows are the drop's own count (`changes()` once it
        // succeeds); its programs' counts stay in total_changes().
        let n = if r.is_ok() {
            self.conn.env.changes.get()
        } else {
            0
        };
        self.conn.env.changes.set(changes);
        self.conn
            .env
            .total_changes
            .set(self.conn.env.total_changes.get().saturating_sub(n));
        self.conn.env.pending.set(n);
        r?;
        if !self.conn.fk.defer && !self.conn.fk.zero(false) {
            return Err(violation());
        }
        Ok(())
    }
}
