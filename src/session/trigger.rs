//! Triggers (v0.16, leaf feature `triggers`).
//!
//! `CREATE TRIGGER` stores the pin's text (`CREATE TRIGGER ` and the
//! statement from the bare trigger name to `END`); `DROP TRIGGER`
//! removes it, and DROP TABLE / DROP VIEW remove a table's triggers
//! with it (that part is ungated, `exec_drop_table`).
//!
//! A write loads the triggers of its table once (`Target`) and fires
//! them per row, as the pin does:
//!
//! - INSERT: BEFORE triggers see the row with defaults, affinities
//!   and generated columns applied and `NEW.rowid` -1 when the rowid
//!   is still to be chosen; the rowid is chosen, and constraints
//!   checked, after them; AFTER triggers see the stored row. An
//!   upsert's DO UPDATE fires the UPDATE triggers instead of AFTER
//!   INSERT.
//! - UPDATE / DELETE: the rows are collected first; each is read
//!   again when its turn comes (a trigger may have changed or
//!   deleted it — a deleted one is skipped). After the BEFORE
//!   triggers the row is read once more: the columns the UPDATE does
//!   not assign take their current values. `NEW`'s generated columns
//!   are NULL in a BEFORE UPDATE trigger. AFTER triggers see the
//!   OLD row as it was read before the BEFORE triggers.
//! - `UPDATE OF cols` fires when the statement assigns one of them.
//! - Triggers fire most recently created first (the pin's list order).
//! - A REPLACE conflict's deletions fire the DELETE triggers only
//!   under `PRAGMA recursive_triggers`.
//! - A trigger already running is not fired again unless
//!   `recursive_triggers` is on; programs nest at most
//!   `Config::max_trigger_depth` deep (the pin: 1000 programs; a
//!   program counts before its WHEN clause is read).
//!
//! A trigger program is run statement by statement, each parsed
//! again from the stored text, its `NEW.` / `OLD.` references bound
//! to the row (`sql::trig`), planned and stepped as a statement of its
//! own on the same connection. Such a statement never commits or
//! rolls back: the statement that fired the trigger does, as the
//! failure's *resolution* says (`Resolve`), which the innermost
//! failing program records — the pin's semantics:
//!
//! - an explicit `OR` clause of the firing statement overrides the
//!   conflict clause of every statement of its trigger programs (and
//!   of theirs); without one each body statement keeps its own;
//! - a constraint failure resolves by that conflict action (FAIL
//!   keeps the changes so far, ROLLBACK ends the transaction, else
//!   ABORT undoes the statement), as do `RAISE(FAIL | ROLLBACK |
//!   ABORT, msg)`;
//! - `RAISE(IGNORE)` abandons the program, the remaining triggers of
//!   the row and the row's change itself (when BEFORE); the firing
//!   statement goes on with its next row.
//!
//! Inside a program, `last_insert_rowid()`, `changes()` and the
//! running count of the firing statement are those of a frame of
//! their own, restored when the program ends (`total_changes()`
//! counts everything).
//!
//! INSTEAD OF triggers make a view writable: INSERT / UPDATE /
//! DELETE of a view with such a trigger compute the view's rows (an
//! UPDATE: `SELECT *, <set values> FROM view WHERE ...`) and fire the
//! triggers for each, changing nothing themselves (`changes()` 0).

use super::*;
use crate::record::Coll;
use crate::sql::ast::{CreateTrigger, TrigEvent, TrigTime};
use alloc::rc::Rc;

/// How a failure inside a trigger program resolves the statement
/// that fired it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resolve {
    /// Undo the statement.
    Abort,
    /// Keep what the statement changed so far.
    Fail,
    /// Roll back the transaction.
    Rollback,
}

/// The connection's trigger state.
#[derive(Debug, Default)]
pub(super) struct TrigState {
    /// The trigger programs running, innermost last.
    pub stack: AVec<AVec<u8>>,
    /// How the innermost failed program resolves its statement.
    pub resolve: Option<Resolve>,
    /// The conflict action the statement about to run inherits
    /// from the program running it (`None`: its own).
    pub orconf: Option<ConflictAction>,
    /// Trigger bodies are being planned for validation (no
    /// validation nests).
    pub validating: core::cell::Cell<bool>,
}

/// One trigger a write may fire, copied out of the catalog.
#[derive(Debug)]
pub(super) struct Trig {
    name: AVec<u8>,
    time: TrigTime,
    event: TrigEvent,
    of_cols: AVec<AVec<u8>>,
    sql: AVec<u8>,
    when: Option<(usize, usize)>,
    steps: AVec<(usize, usize)>,
}

/// The triggers of one write's table, in firing order.
#[derive(Debug)]
pub(super) struct Target {
    names: AVec<AVec<u8>>,
    colls: AVec<Option<Coll>>,
    alias: Option<usize>,
    view: bool,
    trigs: AVec<Trig>,
    /// The conflict action the programs' statements inherit.
    orconf: Option<ConflictAction>,
}

impl Target {
    /// Whether any trigger fires on `event` at `time`.
    pub(super) fn has(&self, time: TrigTime, event: TrigEvent) -> bool {
        self.trigs
            .iter()
            .any(|t| t.time == time && t.event == event)
    }
}

/// A row a trigger fires for (values in table order, rowid).
pub(super) type Row<'x> = Option<(&'x [OwnedValue], i64)>;

fn err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

/// A write of a view through its INSTEAD OF triggers: the rows to
/// fire for, and where their values go.
#[derive(Debug, Clone)]
pub(super) struct InsteadPlan {
    event: TrigEvent,
    /// INSERT: the new rows' supplied values; UPDATE: the view's
    /// rows followed by the SET values; DELETE: the view's rows.
    source: Box<crate::query::SubPlan>,
    /// The view's width.
    ncols: usize,
    /// The view column each supplied value (INSERT) or SET value
    /// (UPDATE) goes to.
    map: AVec<usize>,
    /// UPDATE: the columns assigned.
    assigned: AVec<bool>,
}

/// Errors a body statement planned over all-NULL rows may meet only
/// because of those values (`validate_triggers` ignores them).
fn value_error<E>(e: &Error<E>) -> bool {
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
                | SchemaKind::PercentileFraction
                | SchemaKind::PercentileFractionVaries
                | SchemaKind::PercentileNotNumeric
                | SchemaKind::PercentileInfinite
                | SchemaKind::RaiseIgnore
                | SchemaKind::RaiseAbort
                | SchemaKind::RaiseFail
                | SchemaKind::RaiseRollback
                | SchemaKind::TooBig
        }
    )
}

/// Whether a statement's text has an explicit conflict clause
/// (`INSERT OR ...`, `UPDATE OR ...`, `REPLACE`): the one its
/// trigger programs inherit.
fn explicit_conflict<E>(sql: &[u8], arena: &Arena) -> bool {
    use crate::sql::lexer::Tok;
    let Ok(toks) = crate::sql::lexer::lex::<E>(sql, arena) else {
        return false;
    };
    let kw = |i: usize, w: &[u8]| {
        matches!(toks.as_slice().get(i).map(|t| t.tok),
            Some(Tok::Ident(h, false)) if eq_ci(handle_bytes(arena, h), w))
    };
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::LParen => depth += 1,
            Tok::RParen => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if kw(i, b"REPLACE") => return true,
            _ if kw(i, b"INSERT") || kw(i, b"UPDATE") => return kw(i + 1, b"OR"),
            _ if kw(i, b"DELETE") => return false,
            _ => {}
        }
    }
    false
}

/// Set the conflict action of a body statement (the inherited one).
fn force_conflict(ast: &mut Ast, c: ConflictAction) {
    match ast {
        Ast::Insert { conflict, .. } | Ast::Update { conflict, .. } => *conflict = c,
        Ast::With { body, .. } => force_conflict(body, c),
        _ => {}
    }
}

/// A body statement's own conflict action.
fn ast_conflict(ast: &Ast) -> Option<ConflictAction> {
    match ast {
        Ast::Insert { conflict, .. } | Ast::Update { conflict, .. } => Some(*conflict),
        Ast::With { body, .. } => ast_conflict(body),
        _ => None,
    }
}

impl<S: Storage> Connection<S> {
    /// Whether a trigger program is running (its statements commit
    /// and roll back nothing themselves).
    pub(super) fn in_trigger(&self) -> bool {
        !self.trig.stack.is_empty()
    }

    /// The message of the last `RAISE(ROLLBACK | ABORT | FAIL, msg)`
    /// a trigger program ran (v0.16, `triggers`): the text of a
    /// failed statement's `SchemaKind::RaiseRollback` /
    /// `RaiseAbort` / `RaiseFail`.
    pub fn raise_message(&self) -> AVec<u8> {
        self.env.raise_msg.borrow().clone()
    }

    /// Plan `CREATE TRIGGER`: the pin's checks, and the stored text.
    pub(super) fn plan_create_trigger(
        &self,
        arena: &Arena,
        t: &CreateTrigger,
    ) -> Result<Plan, S::Error> {
        let name = handle_bytes(arena, t.name);
        let table = handle_bytes(arena, t.table);
        // TEMP objects belong to the TEMP database (routed there).
        if t.table_temp && !self.is_temp {
            return Err(err(SchemaKind::TriggerTempReference));
        }
        let ca = &self.catalog_arena;
        let is_table = self.catalog.find(ca, table).is_some();
        let is_view = !is_table && self.catalog.find_view(ca, table).is_some();
        if !is_table && !is_view {
            // A TEMP trigger on a main table (in the TEMP database).
            if self.is_temp && !t.table_temp {
                return Err(Error::Unsupported);
            }
            if Catalog::is_schema_name(table) {
                return Err(err(SchemaKind::SystemTable));
            }
            return Err(err(SchemaKind::UnknownTable));
        }
        if table.get(..7).is_some_and(|p| eq_ci(p, b"sqlite_")) {
            return Err(err(SchemaKind::SystemTable));
        }
        if self.catalog.find_trigger(ca, name).is_some() {
            if t.if_not_exists {
                return Ok(Plan::CreateTrigger {
                    span: (0, 0),
                    name: AVec::new(),
                    table: AVec::new(),
                    skip: true,
                });
            }
            return Err(err(SchemaKind::DuplicateTrigger));
        }
        reserved_name::<S::Error>(name)?;
        if is_view && t.time != TrigTime::Instead {
            return Err(err(SchemaKind::TriggerOnView));
        }
        if is_table && t.time == TrigTime::Instead {
            return Err(err(SchemaKind::InsteadOfTable));
        }
        Ok(Plan::CreateTrigger {
            span: (t.name_at, t.end),
            name: copy_vec(name),
            table: copy_vec(table),
            skip: false,
        })
    }

    /// Plan an INSERT / UPDATE / DELETE of a view that has INSTEAD
    /// OF triggers for it (`None`: the target is no such view).
    pub(super) fn plan_instead(&self, arena: &Arena, ast: &Ast) -> Result<Option<Plan>, S::Error> {
        let ca = &self.catalog_arena;
        let (table, event) = match ast {
            Ast::Insert { table, .. } => (*table, TrigEvent::Insert),
            Ast::Update { table, .. } => (*table, TrigEvent::Update),
            Ast::Delete { table, .. } => (*table, TrigEvent::Delete),
            _ => return Ok(None),
        };
        let name = handle_bytes(arena, table);
        if self.catalog.find(ca, name).is_some() || self.catalog.find_view(ca, name).is_none() {
            return Ok(None);
        }
        if self.catalog.trigger_blocked(ca, name) {
            return Err(err(SchemaKind::TableHasTriggers));
        }
        // An UPDATE needs a trigger whose `UPDATE OF` list meets its
        // SET list (else the pin's "cannot modify v because it is a
        // view").
        let sets: &[(crate::arena::ABytes, Expr)] = match ast {
            Ast::Update { assignments, .. } => assignments.as_slice(),
            _ => &[],
        };
        if !self.catalog.triggers_on(ca, name).any(|t| {
            t.time == TrigTime::Instead
                && t.event == event
                && (t.of_cols.is_empty()
                    || t.of_cols.iter().any(|c| {
                        let c = handle_bytes(ca, *c);
                        sets.iter().any(|(n, _)| eq_ci(handle_bytes(arena, *n), c))
                    }))
        }) {
            return Ok(None);
        }
        let (names, _, _, _) = self.trigger_columns(name, arena)?;
        let ncols = names.len();
        let col = |n: crate::arena::ABytes| -> Result<usize, S::Error> {
            let nb = handle_bytes(arena, n);
            names
                .iter()
                .position(|c| eq_ci(c.as_slice(), nb))
                .ok_or(err(SchemaKind::UnknownColumn))
        };
        let mut map = AVec::new();
        let mut assigned = AVec::new();
        let select = match ast {
            Ast::Insert {
                columns,
                rows,
                select,
                upsert,
                returning,
                ..
            } => {
                if upsert.is_some() {
                    return Err(err(SchemaKind::UpsertView));
                }
                if !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                match columns {
                    Some(cs) => {
                        for c in cs.iter() {
                            map.push(col(*c)?);
                        }
                    }
                    None => {
                        for i in 0..ncols {
                            map.push(i);
                        }
                    }
                }
                match select {
                    Some(s) => s.as_ref().clone(),
                    None => {
                        let mut s =
                            crate::sql::parser::star_select(table, None, None, AVec::new(), arena);
                        s.items.clear();
                        s.from.clear();
                        s.values = rows.clone();
                        s
                    }
                }
            }
            Ast::Update {
                assignments,
                from,
                filter,
                returning,
                ..
            } => {
                if !from.is_empty() || !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                for _ in 0..ncols {
                    assigned.push(false);
                }
                let mut extra = AVec::new();
                for (c, e) in assignments.iter() {
                    let i = col(*c)?;
                    map.push(i);
                    if let Some(a) = assigned.as_mut_slice().get_mut(i) {
                        *a = true;
                    }
                    extra.push(e.clone());
                }
                // The pin reads the view under its own name: an alias
                // qualifies nothing there ("no such column: w.a").
                crate::sql::parser::star_select(table, None, filter.clone(), extra, arena)
            }
            Ast::Delete {
                filter, returning, ..
            } => {
                if !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                crate::sql::parser::star_select(table, None, filter.clone(), AVec::new(), arena)
            }
            _ => return Ok(None),
        };
        let wrapped = Ast::Select(select);
        let expanded = self.expand_ast(arena, &wrapped)?;
        let Some(Ast::Select(sel)) = expanded.as_ref().or(Some(&wrapped)) else {
            return Err(Error::Unsupported);
        };
        let source = self.plan_subquery(sel, arena, &[])?;
        let width = source
            .items
            .len()
            .max(source.values.as_slice().first().map_or(0, |r| r.len()));
        if event == TrigEvent::Insert && width != map.len() {
            return Err(err(SchemaKind::ColumnCountMismatch));
        }
        Ok(Some(Plan::Instead(Box::new(InsteadPlan {
            event,
            source: Box::new(source),
            ncols,
            map,
            assigned,
        }))))
    }

    /// Plan `DROP TRIGGER`.
    pub(super) fn plan_drop_trigger(
        &self,
        arena: &Arena,
        name: crate::arena::ABytes,
        if_exists: bool,
    ) -> Result<Plan, S::Error> {
        let n = handle_bytes(arena, name);
        match self.catalog.find_trigger(&self.catalog_arena, n) {
            Some(t) => Ok(Plan::DropTrigger {
                name: Some(copy_vec(handle_bytes(&self.catalog_arena, t.name))),
            }),
            None if if_exists => Ok(Plan::DropTrigger { name: None }),
            None => Err(err(SchemaKind::UnknownTrigger)),
        }
    }

    /// The triggers a write of `table` may fire (`None`: none).
    /// `sql` is the write's text (its explicit conflict clause).
    pub(super) fn trigger_target(
        &self,
        table: &[u8],
        sql: &[u8],
        conflict: Option<ConflictAction>,
        arena: &Arena,
    ) -> Result<Option<Rc<Target>>, S::Error> {
        let ca = &self.catalog_arena;
        let mut trigs = AVec::new();
        for t in self.catalog.triggers_on(ca, table) {
            let mut of_cols = AVec::new();
            for c in t.of_cols.iter() {
                of_cols.push(copy_vec(handle_bytes(ca, *c)));
            }
            trigs.push(Trig {
                name: copy_vec(handle_bytes(ca, t.name)),
                time: t.time,
                event: t.event,
                of_cols,
                sql: copy_vec(handle_bytes(ca, t.sql)),
                when: t.when,
                steps: t.steps.clone(),
            });
        }
        if trigs.is_empty() {
            return Ok(None);
        }
        // Most recently created first.
        trigs.as_mut_slice().reverse();
        let (names, colls, alias, view) = self.trigger_columns(table, arena)?;
        let orconf = if self.in_trigger() {
            self.trig.orconf
        } else if explicit_conflict::<S::Error>(sql, arena) {
            conflict
        } else {
            None
        };
        Ok(Some(Rc::new(Target {
            names,
            colls,
            alias,
            view,
            trigs,
            orconf,
        })))
    }

    /// The column names, collations and rowid alias `NEW` / `OLD`
    /// read for a table or view.
    #[allow(clippy::type_complexity)]
    fn trigger_columns(
        &self,
        table: &[u8],
        arena: &Arena,
    ) -> Result<(AVec<AVec<u8>>, AVec<Option<Coll>>, Option<usize>, bool), S::Error> {
        let ca = &self.catalog_arena;
        let mut names = AVec::new();
        let mut colls = AVec::new();
        if let Some(t) = self.catalog.find(ca, table) {
            for c in t.columns.iter() {
                names.push(copy_vec(handle_bytes(ca, c.name)));
                colls.push(Some(c.coll));
            }
            return Ok((names, colls, t.rowid_alias, false));
        }
        let Some(v) = self.catalog.find_view(ca, table) else {
            return Err(err(SchemaKind::UnknownTable));
        };
        let ckpt = arena.checkpoint();
        let r = self.view_names(arena, v);
        arena.rewind(ckpt);
        for n in r?.iter() {
            names.push(n.clone());
            colls.push(None);
        }
        Ok((names, colls, None, true))
    }

    /// Plan every body statement of the triggers a write of `table`
    /// on `event` may fire, over all-NULL rows, so a statement whose
    /// triggers do not resolve fails when it is prepared, as on the
    /// pin. Errors only the values could cause are not reported. Not
    /// nested (a trigger body's own triggers are checked when it
    /// runs), and not done for a statement of a trigger program.
    pub(super) fn validate_triggers(
        &self,
        table: &[u8],
        event: TrigEvent,
        assigned: Option<&[AVec<u8>]>,
        arena: &Arena,
    ) -> Result<(), S::Error> {
        if self.trig.validating.get() || self.in_trigger() {
            return Ok(());
        }
        let ca = &self.catalog_arena;
        if self.catalog.triggers_on(ca, table).next().is_none() {
            return Ok(());
        }
        self.trig.validating.set(true);
        let r = self.validate_inner(table, event, assigned, arena);
        self.trig.validating.set(false);
        r
    }

    fn validate_inner(
        &self,
        table: &[u8],
        event: TrigEvent,
        assigned: Option<&[AVec<u8>]>,
        arena: &Arena,
    ) -> Result<(), S::Error> {
        let ca = &self.catalog_arena;
        let (names, colls, alias, view) = self.trigger_columns(table, arena)?;
        let mut nulls = AVec::new();
        for _ in names.iter() {
            nulls.push(OwnedValue::Null);
        }
        let row = crate::sql::trig::RowRef {
            vals: nulls.as_slice(),
            rowid: 0,
        };
        let bind = crate::sql::trig::Binding {
            names: names.as_slice(),
            colls: colls.as_slice(),
            alias,
            view,
            new: (event != TrigEvent::Delete).then_some(row),
            old: (event != TrigEvent::Insert).then_some(row),
        };
        for t in self.catalog.triggers_on(ca, table) {
            if t.event != event {
                continue;
            }
            if let Some(cols) = assigned
                && !t.of_cols.is_empty()
                && !t.of_cols.iter().any(|c| {
                    let c = handle_bytes(ca, *c);
                    cols.iter().any(|a| eq_ci(a.as_slice(), c))
                })
            {
                continue;
            }
            let sql = handle_bytes(ca, t.sql);
            let mut spans: AVec<(bool, usize, usize)> = AVec::new();
            if let Some((a, b)) = t.when {
                spans.push((true, a, b));
            }
            for (a, b) in t.steps.iter() {
                spans.push((false, *a, *b));
            }
            for (when, a, b) in spans.iter() {
                let ckpt = arena.checkpoint();
                let r = self.plan_step(sql, *when, *a, *b, &bind, None, arena);
                arena.rewind(ckpt);
                match r {
                    Err(e) if !value_error(&e) => return Err(e),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Parse, bind and plan one statement of a trigger program
    /// (`when`: the WHEN clause, as `SELECT <expr>`).
    #[allow(clippy::too_many_arguments)]
    fn plan_step(
        &self,
        sql: &[u8],
        when: bool,
        a: usize,
        b: usize,
        bind: &crate::sql::trig::Binding<'_>,
        orconf: Option<ConflictAction>,
        arena: &Arena,
    ) -> Result<(Ast, Plan), S::Error> {
        let body = sql.get(a..b).unwrap_or(&[]);
        let mut text = AVec::new();
        if when {
            text.extend_from_slice(b"SELECT ");
        }
        text.extend_from_slice(body);
        let mut ast = crate::sql::parser::parse_trigger_step::<S::Error>(
            text.as_slice(),
            arena,
            self.cfg.limits(),
        )?;
        crate::sql::trig::bind_trigger::<S::Error>(&mut ast, bind, arena)?;
        if let Some(c) = orconf {
            force_conflict(&mut ast, c);
        }
        let plan = self.plan(arena, &ast)?;
        Ok((ast, plan))
    }

    /// Run one trigger program for a row. `Ok(false)`: it ran
    /// `RAISE(IGNORE)`.
    fn run_program<'a>(
        &'a mut self,
        orconf: Option<ConflictAction>,
        t: &'a Trig,
        bind: &'a crate::sql::trig::Binding<'a>,
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = Result<bool, S::Error>> + 'a>> {
        Box::pin(async move {
            self.trig.stack.push(t.name.clone());
            let env = (
                self.env.pending.get(),
                self.env.changes.get(),
                self.env.last_rowid.get(),
            );
            let txn = self.explicit_txn;
            // Its statements must not commit (`explicit_txn` reads so).
            self.explicit_txn = true;
            let r = self.program_steps(orconf, t, bind).await;
            self.explicit_txn = txn;
            self.env.pending.set(env.0);
            self.env.changes.set(env.1);
            self.env.last_rowid.set(env.2);
            self.trig.stack.pop();
            r
        })
    }

    async fn program_steps(
        &mut self,
        orconf: Option<ConflictAction>,
        t: &Trig,
        bind: &crate::sql::trig::Binding<'_>,
    ) -> Result<bool, S::Error> {
        let ignore = |e: &Error<S::Error>| {
            matches!(
                e,
                Error::Schema {
                    kind: SchemaKind::RaiseIgnore
                }
            )
        };
        if let Some((a, b)) = t.when {
            match self
                .run_step(t.sql.as_slice(), true, a, b, bind, None)
                .await
            {
                Ok(v) => {
                    if v.is_none_or(|v| crate::query::truthy(&v) != Some(true)) {
                        return Ok(true);
                    }
                }
                Err(e) if ignore(&e) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        for (a, b) in t.steps.iter() {
            self.env.pending.set(0);
            match self
                .run_step(t.sql.as_slice(), false, *a, *b, bind, orconf)
                .await
            {
                Ok(_) => {}
                Err(e) if ignore(&e) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Run one statement of a trigger program to its end; the first
    /// column of its first row (for a WHEN clause).
    async fn run_step(
        &mut self,
        sql: &[u8],
        when: bool,
        a: usize,
        b: usize,
        bind: &crate::sql::trig::Binding<'_>,
        orconf: Option<ConflictAction>,
    ) -> Result<Option<OwnedValue>, S::Error> {
        let arena = Arena::new(Scope::Statement, self.cfg.stmt_init, self.cfg.stmt_max);
        let planned = self.plan_step(sql, when, a, b, bind, orconf, &arena);
        let (ast, plan) = match planned {
            Ok(x) => x,
            Err(e) => {
                self.note_failure(&e, None, false);
                return Err(e);
            }
        };
        // What the statement's own triggers inherit.
        let explicit =
            orconf.is_some() || explicit_conflict::<S::Error>(sql.get(a..b).unwrap_or(&[]), &arena);
        self.trig.orconf = if explicit { ast_conflict(&ast) } else { None };
        let conflict = ast_conflict(&ast);
        let sql_text = arena.alloc_copy_handle(sql.get(a..b).unwrap_or(&[]))?;
        #[cfg(feature = "params")]
        let binds = Binds {
            values: AVec::new(),
            table: crate::sql::lexer::ParamTable::default(),
            src: None,
            plan_ckpt: arena.checkpoint(),
            stale: false,
            started: false,
        };
        let mut st = Statement::assemble(
            self,
            arena,
            sql_text,
            ast,
            plan,
            #[cfg(feature = "params")]
            binds,
        );
        let mut first = None;
        loop {
            match st.step().await {
                Ok(Step::Row) => {
                    if first.is_none() {
                        first = Some(
                            st.column_value(0)
                                .map_or(OwnedValue::Null, OwnedValue::from_value),
                        );
                    }
                }
                Ok(Step::Done) => break,
                Err(e) => {
                    let rb = st.resolves_rollback(&e);
                    st.conn.note_failure(&e, conflict, rb);
                    return Err(e);
                }
            }
        }
        Ok(first)
    }

    /// Record how a failure of a program's statement resolves the
    /// firing statement (the innermost failure's, which is the first
    /// recorded).
    fn note_failure(&mut self, e: &Error<S::Error>, conflict: Option<ConflictAction>, rb: bool) {
        // RAISE(IGNORE) fails nothing: the program that ran it ends.
        if self.trig.resolve.is_some()
            || matches!(
                e,
                Error::Schema {
                    kind: SchemaKind::RaiseIgnore
                }
            )
        {
            return;
        }
        let r = match e {
            Error::Schema {
                kind: SchemaKind::RaiseRollback,
            } => Resolve::Rollback,
            Error::Schema {
                kind: SchemaKind::RaiseFail,
            } => Resolve::Fail,
            Error::Schema {
                kind:
                    SchemaKind::UniqueViolation
                    | SchemaKind::DuplicateRowid
                    | SchemaKind::NotNullViolation
                    | SchemaKind::CheckViolation,
            } => match conflict {
                Some(ConflictAction::Fail) => Resolve::Fail,
                Some(ConflictAction::Rollback) => Resolve::Rollback,
                _ if rb => Resolve::Rollback,
                _ => Resolve::Abort,
            },
            _ => Resolve::Abort,
        };
        self.trig.resolve = Some(r);
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    /// Load the triggers this write's table has for its execution
    /// (`self.trig`), and forget a stale failure resolution.
    pub(super) fn load_triggers(&mut self) -> Result<(), S::Error> {
        self.trig = None;
        if !self.conn.in_trigger() {
            self.conn.trig.resolve = None;
        }
        let ast = match &self.ast {
            Ast::With { body, .. } => body.as_ref(),
            a => a,
        };
        let (table, conflict) = match ast {
            Ast::Insert {
                table, conflict, ..
            }
            | Ast::Update {
                table, conflict, ..
            } => (*table, Some(*conflict)),
            Ast::Delete { table, .. } => (*table, None),
            _ => return Ok(()),
        };
        let name = copy_vec(handle_bytes(&self.arena, table));
        let sql = copy_vec(handle_bytes(&self.arena, self.sql_text));
        self.trig =
            self.conn
                .trigger_target(name.as_slice(), sql.as_slice(), conflict, &self.arena)?;
        Ok(())
    }

    /// Whether this write's table has triggers on `event` at `time`.
    pub(super) fn fires(&self, time: TrigTime, event: TrigEvent) -> bool {
        self.trig.as_ref().is_some_and(|t| t.has(time, event))
    }

    /// Fire the triggers on `event` at `time` for one row (`old` /
    /// `new`: the row before / after the change). `changed`: the
    /// columns an UPDATE assigns (and whether it assigns the rowid),
    /// which `UPDATE OF` triggers need. `Ok(false)`: a program ran
    /// `RAISE(IGNORE)` — skip the row.
    pub(super) async fn fire(
        &mut self,
        time: TrigTime,
        event: TrigEvent,
        changed: Option<(&[bool], bool)>,
        old: Row<'_>,
        new: Row<'_>,
    ) -> Result<bool, S::Error> {
        let orconf = self.trig.as_ref().and_then(|t| t.orconf);
        self.fire_as(time, event, changed, old, new, orconf).await
    }

    /// `fire`, the programs' statements inheriting `orconf` (an
    /// upsert's DO UPDATE fires its UPDATE triggers under ABORT, as
    /// the pin runs that update).
    pub(super) async fn fire_as(
        &mut self,
        time: TrigTime,
        event: TrigEvent,
        changed: Option<(&[bool], bool)>,
        old: Row<'_>,
        new: Row<'_>,
        orconf: Option<ConflictAction>,
    ) -> Result<bool, S::Error> {
        let Some(tg) = self.trig.clone() else {
            return Ok(true);
        };
        let bind = crate::sql::trig::Binding {
            names: tg.names.as_slice(),
            colls: tg.colls.as_slice(),
            alias: tg.alias,
            view: tg.view,
            new: new.map(|(vals, rowid)| crate::sql::trig::RowRef { vals, rowid }),
            old: old.map(|(vals, rowid)| crate::sql::trig::RowRef { vals, rowid }),
        };
        for t in tg.trigs.iter() {
            if t.time != time || t.event != event {
                continue;
            }
            if let Some((cols, rowid_set)) = changed
                && !t.of_cols.is_empty()
                && !t.of_cols.iter().any(|c| {
                    match tg
                        .names
                        .iter()
                        .position(|n| eq_ci(n.as_slice(), c.as_slice()))
                    {
                        Some(i) if Some(i) == tg.alias => {
                            rowid_set || cols.get(i).copied().unwrap_or(false)
                        }
                        Some(i) => cols.get(i).copied().unwrap_or(false),
                        None => false,
                    }
                })
            {
                continue;
            }
            if !self.conn.prag.recursive_triggers
                && self
                    .conn
                    .trig
                    .stack
                    .iter()
                    .any(|n| n.as_slice() == t.name.as_slice())
            {
                continue;
            }
            // The pin counts the program before its WHEN clause.
            if self.conn.trig.stack.len() >= self.conn.cfg.max_trigger_depth {
                let e = err(SchemaKind::TriggerDepth);
                self.conn.note_failure(&e, None, false);
                return Err(e);
            }
            if !self.conn.run_program(orconf, t, &bind).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// One victim of a REPLACE conflict (`rowid` in this write's
    /// table): under `recursive_triggers`, with DELETE triggers,
    /// deleted between them, as on the pin. Whether it was handled
    /// here (otherwise the caller deletes it without triggers). A
    /// BEFORE DELETE trigger's `RAISE(IGNORE)` keeps the row, and the
    /// conflict then fails the write (`kind`), as on the pin.
    pub(super) async fn replace_victim(
        &mut self,
        rowid: i64,
        kind: SchemaKind,
    ) -> Result<bool, S::Error> {
        let before = self.fires(TrigTime::Before, TrigEvent::Delete);
        let after = self.fires(TrigTime::After, TrigEvent::Delete);
        if !self.conn.prag.recursive_triggers || !(before || after) {
            return Ok(false);
        }
        let (root, alias, defaults) = match &self.plan {
            Plan::Insert {
                root,
                rowid_alias,
                cols,
                ..
            } => {
                let mut d = AVec::new();
                for c in cols.iter() {
                    d.push(c.default.clone());
                }
                (*root, *rowid_alias, d)
            }
            Plan::Update {
                root,
                rowid_alias,
                defaults,
                ..
            } => (*root, *rowid_alias, defaults.clone()),
            _ => return Ok(false),
        };
        let ncols = defaults.len();
        let Some(row) = self
            .reload_write_row(root, ncols, defaults.as_slice(), rowid)
            .await?
        else {
            return Ok(true);
        };
        if before
            && !self
                .fire(
                    TrigTime::Before,
                    TrigEvent::Delete,
                    None,
                    Some((row.rec.as_slice(), rowid)),
                    None,
                )
                .await?
        {
            return Err(err(kind));
        }
        self.delete_row_completely(root, alias, ncols, defaults.as_slice(), rowid)
            .await?;
        if after {
            self.fire(
                TrigTime::After,
                TrigEvent::Delete,
                None,
                Some((row.rec.as_slice(), rowid)),
                None,
            )
            .await?;
        }
        Ok(true)
    }

    /// The end of a top-level write whose trigger program failed with
    /// `e`: resolved as the program's failure says (`Resolve`).
    /// Whether it was such a failure.
    pub(super) async fn trigger_failed(&mut self, e: &Error<S::Error>) -> bool {
        if self.conn.in_trigger() {
            return false;
        }
        let Some(r) = self.conn.trig.resolve.take() else {
            return false;
        };
        match r {
            Resolve::Fail => {
                let committed = if self.conn.explicit_txn {
                    Ok(())
                } else {
                    self.conn.pager.drive_commit().await
                };
                if committed.is_ok() {
                    self.conn.env.finish(true);
                    self.keep_stmt();
                    return true;
                }
                self.conn.env.finish(false);
                abort_write!(self, e);
            }
            Resolve::Rollback => {
                self.conn.env.finish(false);
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
            }
            Resolve::Abort => {
                self.conn.env.finish(false);
                abort_write!(self, e);
            }
        }
        true
    }

    /// An INSERT / UPDATE / DELETE of a view through its INSTEAD OF
    /// triggers: they fire for each row; the view changes nothing
    /// (`changes()` is 0).
    pub(super) async fn step_instead(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_instead().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                if !self.trigger_failed(&e).await {
                    self.conn.env.finish(false);
                    abort_write!(self, &e);
                }
                Err(e)
            }
        }
    }

    async fn exec_instead(&mut self) -> Result<(), S::Error> {
        self.conn.env.pending.set(0);
        self.load_triggers()?;
        let Plan::Instead(ip) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let ip = ip.clone();
        let rows = self.exec_subplan(&ip.source, &RowData::empty()).await?;
        for row in rows.iter() {
            let r = row.as_slice();
            match ip.event {
                TrigEvent::Insert => {
                    let mut new = AVec::new();
                    for _ in 0..ip.ncols {
                        new.push(OwnedValue::Null);
                    }
                    for (j, c) in ip.map.iter().enumerate() {
                        if let (Some(slot), Some(v)) = (new.as_mut_slice().get_mut(*c), r.get(j)) {
                            *slot = v.clone();
                        }
                    }
                    self.fire(
                        TrigTime::Instead,
                        TrigEvent::Insert,
                        None,
                        None,
                        Some((new.as_slice(), 0)),
                    )
                    .await?;
                }
                TrigEvent::Update => {
                    let old = r.get(..ip.ncols).unwrap_or(r);
                    let mut new = AVec::new();
                    new.extend_from_slice(old);
                    for (j, c) in ip.map.iter().enumerate() {
                        if let (Some(slot), Some(v)) =
                            (new.as_mut_slice().get_mut(*c), r.get(ip.ncols + j))
                        {
                            *slot = v.clone();
                        }
                    }
                    self.fire(
                        TrigTime::Instead,
                        TrigEvent::Update,
                        Some((ip.assigned.as_slice(), false)),
                        Some((old, 0)),
                        Some((new.as_slice(), 0)),
                    )
                    .await?;
                }
                TrigEvent::Delete => {
                    let old = r.get(..ip.ncols).unwrap_or(r);
                    self.fire(
                        TrigTime::Instead,
                        TrigEvent::Delete,
                        None,
                        Some((old, 0)),
                        None,
                    )
                    .await?;
                }
            }
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        self.conn.env.finish(true);
        Ok(())
    }

    /// `CREATE TRIGGER`: its schema row.
    pub(super) async fn step_create_trigger(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_create_trigger().await;
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

    async fn exec_create_trigger(&mut self) -> Result<(), S::Error> {
        let Plan::CreateTrigger {
            span,
            name,
            table,
            skip,
        } = &self.plan
        else {
            return Err(Error::Unsupported);
        };
        if *skip {
            return Ok(());
        }
        let (name, table) = (name.clone(), table.clone());
        let mut sql = copy_vec(b"CREATE TRIGGER ");
        sql.extend_from_slice(
            handle_bytes(&self.arena, self.sql_text)
                .get(span.0..span.1)
                .unwrap_or(&[]),
        );
        self.insert_schema_row(
            b"trigger",
            name.as_slice(),
            table.as_slice(),
            0,
            Some(sql.as_slice()),
        )
        .await?;
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// `DROP TRIGGER`: its schema row goes.
    pub(super) async fn step_drop_trigger(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_drop_trigger().await;
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

    async fn exec_drop_trigger(&mut self) -> Result<(), S::Error> {
        let Plan::DropTrigger { name } = &self.plan else {
            return Err(Error::Unsupported);
        };
        let Some(name) = name.clone() else {
            return Ok(());
        };
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        let mut found = None;
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 2)?;
            if matches!(values.as_slice().first(), Some(Value::Text(t)) if eq_ci(t, b"trigger"))
                && matches!(values.as_slice().get(1), Some(Value::Text(n)) if eq_ci(n, name.as_slice()))
            {
                found = Some(rowid);
                break;
            }
        }
        let Some(rowid) = found else {
            return Err(err(SchemaKind::UnknownTrigger));
        };
        btree::btree_delete(&mut self.conn.pager, 1, rowid).await?;
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }
}
