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
//!   in a BEFORE UPDATE trigger are computed from the assigned
//!   columns and those some firing BEFORE UPDATE trigger reads as
//!   `NEW.x` (the pin's new-column mask); other columns count as
//!   NULL there. AFTER triggers see the
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
    /// Trigger bodies are being planned for validation: the
    /// triggers checked so far (each once, as the pin compiles each
    /// program once), and how deep the check has nested.
    pub validated: core::cell::RefCell<Option<AVec<AVec<u8>>>>,
    pub validate_depth: core::cell::Cell<usize>,
    /// A trigger program's statement is being planned on the main
    /// database (v0.16 review fix): its unqualified table names
    /// are main's, never a TEMP table's (`Expander::fix_main`).
    pub fix_main: core::cell::Cell<bool>,
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
    /// A BEFORE UPDATE trigger's `NEW.x` column names (`None`: not
    /// such a trigger, or its text did not parse — every column
    /// counts then).
    new_refs: Option<AVec<AVec<u8>>>,
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
    pub(super) event: TrigEvent,
    /// INSERT: the new rows' supplied values; UPDATE: the view's
    /// rows followed by the SET values; DELETE: the view's rows.
    source: Box<crate::query::SubPlan>,
    /// The view's width.
    ncols: usize,
    /// The view column each supplied value (INSERT) or SET value
    /// (UPDATE) goes to.
    map: AVec<usize>,
    /// UPDATE: the columns assigned.
    pub(super) assigned: AVec<bool>,
    /// The RETURNING list, over the view's columns (v0.16 review
    /// fix): evaluated for each row whose triggers ran to the end,
    /// after them, on NEW (INSERT, UPDATE) or OLD (DELETE), as on the
    /// pin.
    returning: AVec<RExpr>,
}

impl InsteadPlan {
    pub(super) fn has_returning(&self) -> bool {
        !self.returning.is_empty()
    }

    pub(super) fn returning_width(&self) -> usize {
        self.returning.len()
    }
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
pub(super) fn explicit_conflict<E>(sql: &[u8], arena: &Arena) -> bool {
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

/// A write statement's target, conflict action, event and whether it
/// has an upsert clause (`None`: no write).
fn write_shape(ast: &Ast) -> Option<(crate::arena::ABytes, ConflictAction, TrigEvent, bool)> {
    match ast {
        Ast::Insert {
            table,
            conflict,
            upsert,
            ..
        } => Some((*table, *conflict, TrigEvent::Insert, upsert.is_some())),
        Ast::Update {
            table, conflict, ..
        } => Some((*table, *conflict, TrigEvent::Update, false)),
        Ast::Delete { table, .. } => {
            Some((*table, ConflictAction::Abort, TrigEvent::Delete, false))
        }
        Ast::With { body, .. } => write_shape(body),
        _ => None,
    }
}

/// One write `nest_may_abort` walks: table, event, conflict action,
/// whether it has an upsert clause, an UPDATE's assigned column
/// names, whether it is a SET NULL action's (`fk_may_abort`).
type Work = (
    AVec<u8>,
    TrigEvent,
    ConflictAction,
    bool,
    Option<AVec<AVec<u8>>>,
    bool,
);

/// An UPDATE's assigned column names (`None`: no UPDATE).
fn update_names(ast: &Ast, arena: &Arena) -> Option<AVec<AVec<u8>>> {
    match ast {
        Ast::Update { assignments, .. } => {
            let mut out = AVec::new();
            for (n, _) in assignments.iter() {
                out.push(copy_vec(handle_bytes(arena, *n)));
            }
            Some(out)
        }
        Ast::With { body, .. } => update_names(body, arena),
        _ => None,
    }
}

/// Whether a statement's text calls a function the pin codes as a
/// call (an abort point: `nest_may_abort`), `RAISE(ABORT)`, or LIKE /
/// GLOB / REGEXP / MATCH. The inlined functions (coalesce, ifnull,
/// iif, if, likely, unlikely, likelihood) and the aggregates count,
/// sum, total, avg, group_concat, string_agg are none (probed).
/// Conservative: unlexable text counts as one.
fn lexical_may_abort<E>(sql: &[u8], arena: &Arena) -> bool {
    use crate::sql::lexer::Tok;
    let Ok(toks) = crate::sql::lexer::lex::<E>(sql, arena) else {
        return true;
    };
    let t = toks.as_slice();
    let word = |i: usize| match t.get(i).map(|x| x.tok) {
        Some(Tok::Ident(h, false)) => Some(handle_bytes(arena, h)),
        _ => None,
    };
    let is = |i: usize, w: &[u8]| word(i).is_some_and(|b| eq_ci(b, w));
    const NOT_CALLS: &[&[u8]] = &[
        b"IN",
        b"VALUES",
        b"EXISTS",
        b"CAST",
        b"AS",
        b"OVER",
        b"FILTER",
        b"USING",
        b"AND",
        b"OR",
        b"NOT",
        b"WHEN",
        b"THEN",
        b"ELSE",
        b"CASE",
        b"SELECT",
        b"WHERE",
        b"SET",
        b"ON",
        b"BY",
        b"IS",
        b"BETWEEN",
        b"FROM",
        b"JOIN",
        b"DISTINCT",
        b"ALL",
        b"UNION",
        b"INTERSECT",
        b"EXCEPT",
        b"LIMIT",
        b"OFFSET",
        b"HAVING",
        b"ESCAPE",
        b"DO",
        b"CONFLICT",
        b"WITH",
        b"RECURSIVE",
        b"coalesce",
        b"ifnull",
        b"iif",
        b"if",
        b"likely",
        b"unlikely",
        b"likelihood",
        b"count",
        b"sum",
        b"total",
        b"avg",
        b"group_concat",
        b"string_agg",
    ];
    for i in 0..t.len() {
        if ["LIKE", "GLOB", "REGEXP", "MATCH"]
            .iter()
            .any(|k| is(i, k.as_bytes()))
        {
            return true;
        }
        if !matches!(t.get(i + 1).map(|x| x.tok), Some(Tok::LParen)) {
            continue;
        }
        match t.get(i).map(|x| x.tok) {
            Some(Tok::Ident(..)) => {}
            _ => continue,
        }
        // `INTO t (cols)`: a column list.
        if i > 0 && is(i - 1, b"INTO") {
            continue;
        }
        if is(i, b"RAISE") {
            if is(i + 2, b"ABORT") {
                return true;
            }
            continue;
        }
        if word(i).is_some_and(|w| NOT_CALLS.iter().any(|k| eq_ci(w, k))) {
            continue;
        }
        return true;
    }
    false
}

/// Set the conflict action of a body statement (the inherited one).
pub(super) fn force_conflict(ast: &mut Ast, c: ConflictAction) {
    match ast {
        Ast::Insert { conflict, .. } | Ast::Update { conflict, .. } => *conflict = c,
        Ast::With { body, .. } => force_conflict(body, c),
        _ => {}
    }
}

/// A body statement's own conflict action.
pub(super) fn ast_conflict(ast: &Ast) -> Option<ConflictAction> {
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
        let ret_items;
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
                ret_items = Some(returning);
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
                if !from.is_empty() {
                    return Err(Error::Unsupported);
                }
                ret_items = Some(returning);
                for _ in 0..ncols {
                    assigned.push(false);
                }
                let mut extra = AVec::new();
                for (c, e) in assignments.iter() {
                    // The view's rowid (v0.16 review fix): the pin
                    // takes the assignment and evaluates its value,
                    // but NEW holds no rowid of a view — nothing
                    // changes (`usize::MAX`: no column).
                    let i = match col(*c) {
                        Err(_) if crate::query::is_rowid_name(handle_bytes(arena, *c)) => {
                            usize::MAX
                        }
                        r => r?,
                    };
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
                ret_items = Some(returning);
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
        // RETURNING reads the view's columns under its name (no
        // affinity, no rowid: the pin's "no such column: rowid").
        // Its subqueries read views as any statement's do.
        // (as the items of a FROM-less SELECT).
        let exp_ret = match ret_items {
            Some(items) if !items.is_empty() => {
                let mut s = crate::sql::parser::star_select(table, None, None, AVec::new(), arena);
                s.from.clear();
                s.items = items.clone();
                self.expand_ast(arena, &Ast::Select(s))?
            }
            _ => None,
        };
        let ret_items = match exp_ret.as_ref() {
            Some(Ast::Select(s)) => Some(&s.items),
            _ => ret_items,
        };
        let returning = match ret_items {
            Some(items) if !items.is_empty() => {
                let mut cols = AVec::new();
                for n in names.iter() {
                    cols.push((n.clone(), crate::record::Affinity::Blob));
                }
                let mut sources = AVec::new();
                sources.push(crate::query::SourceDesc::new(
                    copy_vec(name),
                    cols,
                    None,
                    false,
                ));
                let mut offsets = AVec::new();
                offsets.push(0);
                self.plan_returning_in(arena, sources, offsets, items)?
            }
            _ => AVec::new(),
        };
        Ok(Some(Plan::Instead(Box::new(InsteadPlan {
            event,
            source: Box::new(source),
            ncols,
            map,
            assigned,
            returning,
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
            let sql = handle_bytes(ca, t.sql);
            let new_refs = if t.time == TrigTime::Before && t.event == TrigEvent::Update {
                let ckpt = arena.checkpoint();
                let r = self.new_refs(sql, t.when, t.steps.as_slice(), arena);
                arena.rewind(ckpt);
                r
            } else {
                None
            };
            trigs.push(Trig {
                name: copy_vec(handle_bytes(ca, t.name)),
                time: t.time,
                event: t.event,
                of_cols,
                sql: copy_vec(sql),
                when: t.when,
                steps: t.steps.clone(),
                new_refs,
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

    /// The column names a trigger's WHEN clause and body read as
    /// `NEW.x` (`None`: its text does not parse).
    fn new_refs(
        &self,
        sql: &[u8],
        when: Option<(usize, usize)>,
        steps: &[(usize, usize)],
        arena: &Arena,
    ) -> Option<AVec<AVec<u8>>> {
        let mut out = AVec::new();
        for (w, (a, b)) in when
            .iter()
            .map(|x| (true, *x))
            .chain(steps.iter().map(|x| (false, *x)))
        {
            let mut text = AVec::new();
            if w {
                text.extend_from_slice(b"SELECT 1 WHERE ");
            }
            text.extend_from_slice(sql.get(a..b)?);
            let ast = crate::sql::parser::parse_trigger_step::<S::Error>(
                text.as_slice(),
                arena,
                self.cfg.limits(),
            )
            .ok()?;
            let refs = crate::sql::trig::trigger_refs::<S::Error>(&ast, arena).ok()?;
            for (q, n) in refs.iter() {
                if eq_ci(handle_bytes(arena, *q), b"new") {
                    out.push(copy_vec(handle_bytes(arena, *n)));
                }
            }
        }
        Some(out)
    }

    /// The column names, collations and rowid alias `NEW` / `OLD`
    /// read for a table or view.
    #[allow(clippy::type_complexity)]
    pub(super) fn trigger_columns(
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
            // A WITHOUT ROWID table's NEW / OLD hold no rowid, as a
            // view's do not (v0.16).
            return Ok((names, colls, t.rowid_alias, !t.has_rowid()));
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
    /// pin. Errors only the values could cause are not reported.
    /// Nested (v0.16 review fix): planning a body statement checks
    /// the triggers it may fire in turn, whatever their WHEN clauses
    /// — each trigger once, at most `max_trigger_depth` deep — as the
    /// pin compiles the whole nest when the statement is prepared.
    /// Not done for a statement of a trigger program (the nest was
    /// checked when its top statement was prepared).
    pub(super) fn validate_triggers(
        &self,
        table: &[u8],
        event: TrigEvent,
        assigned: Option<&[AVec<u8>]>,
        arena: &Arena,
    ) -> Result<(), S::Error> {
        if self.in_trigger() {
            return Ok(());
        }
        let ca = &self.catalog_arena;
        if self.catalog.triggers_on(ca, table).next().is_none() {
            return Ok(());
        }
        let top = self.trig.validated.borrow().is_none();
        if top {
            *self.trig.validated.borrow_mut() = Some(AVec::new());
        }
        let depth = self.trig.validate_depth.get();
        let r = if depth >= self.cfg.max_trigger_depth {
            Ok(())
        } else {
            self.trig.validate_depth.set(depth + 1);
            let r = self.validate_inner(table, event, assigned, arena);
            self.trig.validate_depth.set(depth);
            r
        };
        if top {
            *self.trig.validated.borrow_mut() = None;
        }
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
            let tname = handle_bytes(ca, t.name);
            {
                let mut seen = self.trig.validated.borrow_mut();
                let Some(seen) = seen.as_mut() else {
                    continue;
                };
                if seen.iter().any(|n| n.as_slice() == tname) {
                    continue;
                }
                seen.push(copy_vec(tname));
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
    /// (`when`: the WHEN clause, as `SELECT 1 WHERE <expr>`).
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
            // As a WHERE (v0.16 review fix): the pin resolves a WHEN
            // clause like one, so an aggregate or window call there
            // is its "misuse of aggregate / window function". A row
            // comes back exactly when the condition is true.
            text.extend_from_slice(b"SELECT 1 WHERE ");
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
        let fixed = self.trig.fix_main.replace(!self.is_temp);
        let plan = self.plan(arena, &ast);
        self.trig.fix_main.set(fixed);
        Ok((ast, plan?))
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
        for (k, (a, b)) in t.steps.iter().enumerate() {
            self.env.pending.set(0);
            // The keys the statement does not look up (`fkey::Sim`).
            #[cfg(feature = "foreign_keys")]
            {
                self.fk.next = self.fk.trig_site(t.name.as_slice(), orconf, k);
            }
            let r = self
                .run_step(t.sql.as_slice(), false, *a, *b, bind, orconf)
                .await;
            #[cfg(feature = "foreign_keys")]
            {
                self.fk.next = [None; 2];
            }
            let _ = k;
            match r {
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
                    // The failing constraint's own action (v0.16).
                    let c = st.fail_act.get().or(conflict);
                    st.conn.note_failure(&e, c, rb);
                    return Err(e);
                }
            }
        }
        Ok(first)
    }

    /// Whether a statement (`sql`) and every trigger program it may
    /// run hold an *abort point* (v0.16 review fix): the pin opens a
    /// statement journal only for a statement that may halt with
    /// ABORT — a constraint checked under ABORT, a call of a
    /// non-inlined function (LIKE / GLOB included), `RAISE(ABORT)`,
    /// foreign keys. Without one, a failure that is no constraint's
    /// (the trigger depth limit) inside an explicit transaction
    /// leaves the statement's changes in place. Conservative: what
    /// it cannot tell counts as an abort point (hipp's undo).
    pub(super) fn nest_may_abort(&self, sql: &[u8], arena: &Arena) -> bool {
        // Without the `foreign_keys` leaf such writes are refused.
        #[cfg(not(feature = "foreign_keys"))]
        if self.prag.foreign_keys {
            return true;
        }
        if lexical_may_abort::<S::Error>(sql, arena) {
            return true;
        }
        let Ok(ast) = crate::sql::parser::parse::<S::Error>(sql, arena, self.cfg.limits()) else {
            return true;
        };
        let Some(top) = write_shape(&ast) else {
            return false;
        };
        let orconf = explicit_conflict::<S::Error>(sql, arena).then_some(top.1);
        let mut work: AVec<Work> = AVec::new();
        work.push((
            copy_vec(handle_bytes(arena, top.0)),
            top.2,
            top.1,
            top.3,
            update_names(&ast, arena),
            false,
        ));
        let mut seen: AVec<AVec<u8>> = AVec::new();
        // The action programs walked (child table, event).
        #[cfg(feature = "foreign_keys")]
        let mut acted: AVec<(AVec<u8>, TrigEvent)> = AVec::new();
        let ca = &self.catalog_arena;
        while let Some((table, event, conflict, upsert, names, set_null)) = work.pop() {
            if self.table_may_abort(table.as_slice(), event, conflict, upsert) {
                return true;
            }
            #[cfg(feature = "foreign_keys")]
            if self.prag.foreign_keys {
                let mut follow = AVec::new();
                let cols = names.as_ref().map(|n| n.as_slice());
                if self.fk_may_abort(table.as_slice(), event, cols, set_null, &mut follow) {
                    return true;
                }
                for (t, ev, cols, null) in follow.iter() {
                    if acted
                        .iter()
                        .any(|(n, e)| e == ev && eq_ci(n.as_slice(), t.as_slice()))
                    {
                        continue;
                    }
                    acted.push((t.clone(), *ev));
                    work.push((
                        t.clone(),
                        *ev,
                        ConflictAction::Abort,
                        false,
                        cols.clone(),
                        *null,
                    ));
                }
            }
            let _ = (&names, set_null);
            let replace = self.table_replaces(table.as_slice(), conflict)
                && event != TrigEvent::Delete
                && self.prag.recursive_triggers;
            for t in self.catalog.triggers_on(ca, table.as_slice()) {
                if t.event != event && !(replace && t.event == TrigEvent::Delete) {
                    continue;
                }
                let name = handle_bytes(ca, t.name);
                if seen.iter().any(|n| n.as_slice() == name) {
                    continue;
                }
                seen.push(copy_vec(name));
                let text = handle_bytes(ca, t.sql);
                let mut spans: AVec<(usize, usize)> = AVec::new();
                if let Some(w) = t.when {
                    spans.push(w);
                }
                spans.extend_from_slice(t.steps.as_slice());
                for (a, b) in spans.iter() {
                    let Some(step) = text.get(*a..*b) else {
                        return true;
                    };
                    if lexical_may_abort::<S::Error>(step, arena) {
                        return true;
                    }
                }
                for (a, b) in t.steps.iter() {
                    let step = text.get(*a..*b).unwrap_or(&[]);
                    let Ok(ast) = crate::sql::parser::parse_trigger_step::<S::Error>(
                        step,
                        arena,
                        self.cfg.limits(),
                    ) else {
                        return true;
                    };
                    if let Some((tb, c, ev, up)) = write_shape(&ast) {
                        work.push((
                            copy_vec(handle_bytes(arena, tb)),
                            ev,
                            orconf.unwrap_or(c),
                            up,
                            update_names(&ast, arena),
                            false,
                        ));
                    }
                }
            }
        }
        false
    }

    /// Whether a write of `table` checks a constraint that may halt
    /// with ABORT under `conflict` (`nest_may_abort`; conservative).
    fn table_may_abort(
        &self,
        table: &[u8],
        event: TrigEvent,
        conflict: ConflictAction,
        upsert: bool,
    ) -> bool {
        let ca = &self.catalog_arena;
        let Some(t) = self.catalog.find(ca, table) else {
            // A view (its INSTEAD OF triggers are walked) or unknown.
            return self.catalog.find_view(ca, table).is_none();
        };
        if event == TrigEvent::Delete {
            return false;
        }
        let not_null = t.columns.iter().any(|c| c.not_null);
        let other = !t.checks.is_empty()
            || t.strict
            || t.has_generated()
            || t.rowid_alias.is_some()
            || upsert
            || (event == TrigEvent::Insert && t.columns.iter().any(|c| c.default_expr.is_some()))
            || self
                .catalog
                .indexes_for(ca, table)
                .any(|i| i.unique || i.opaque);
        match conflict {
            // No OR clause (v0.16): conservatively, as ABORT.
            ConflictAction::Abort | ConflictAction::Unset => not_null || other,
            // REPLACE: a NOT NULL without default and CHECK abort.
            ConflictAction::Replace => not_null || !t.checks.is_empty() || t.strict,
            _ => false,
        }
    }

    /// Record how a failure of a program's statement resolves the
    /// firing statement (the innermost failure's, which is the first
    /// recorded).
    pub(super) fn note_failure(
        &mut self,
        e: &Error<S::Error>,
        conflict: Option<ConflictAction>,
        rb: bool,
    ) {
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
        // DROP TABLE's implicit DELETE fires none (`foreign_keys`).
        #[cfg(feature = "foreign_keys")]
        if core::mem::take(&mut self.conn.fk.silent) {
            return Ok(());
        }
        let name = copy_vec(handle_bytes(&self.arena, table));
        let sql = copy_vec(handle_bytes(&self.arena, self.sql_text));
        self.trig =
            self.conn
                .trigger_target(name.as_slice(), sql.as_slice(), conflict, &self.arena)?;
        Ok(())
    }

    /// The columns a BEFORE UPDATE trigger's `NEW` holds for an
    /// UPDATE assigning `cols` (and the rowid when `rowid_set`): the
    /// pin loads an unassigned column into `NEW` only when one of the
    /// BEFORE UPDATE triggers that fire reads it as `NEW.x` (or it
    /// is past the 32nd column), and computes the generated columns
    /// from that row — the other columns read as NULL. Per column:
    /// whether it is loaded.
    pub(super) fn before_update_loaded(&self, cols: &[bool], rowid_set: bool) -> AVec<bool> {
        let mut loaded = AVec::new();
        let Some(tg) = self.trig.as_ref() else {
            return loaded;
        };
        for i in 0..tg.names.len() {
            loaded.push(i > 31 || cols.get(i).copied().unwrap_or(false));
        }
        for t in tg.trigs.iter() {
            if t.time != TrigTime::Before || t.event != TrigEvent::Update {
                continue;
            }
            if !Self::of_overlap(tg, t, cols, rowid_set) {
                continue;
            }
            let Some(refs) = t.new_refs.as_ref() else {
                for l in loaded.as_mut_slice() {
                    *l = true;
                }
                continue;
            };
            for r in refs.iter() {
                match tg
                    .names
                    .iter()
                    .position(|n| eq_ci(n.as_slice(), r.as_slice()))
                {
                    // A column past the 32nd sets every bit of the pin's mask.
                    Some(i) if i > 31 => {
                        for l in loaded.as_mut_slice() {
                            *l = true;
                        }
                    }
                    Some(i) => {
                        if let Some(l) = loaded.as_mut_slice().get_mut(i) {
                            *l = true;
                        }
                    }
                    None => {}
                }
            }
        }
        loaded
    }

    /// Whether an UPDATE assigning `cols` fires `t` by its `UPDATE OF`
    /// list (always, without one).
    fn of_overlap(tg: &Target, t: &Trig, cols: &[bool], rowid_set: bool) -> bool {
        t.of_cols.is_empty()
            || t.of_cols.iter().any(|c| {
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
    }

    /// The rows an INSERT ... SELECT of a trigger program had
    /// written before its SELECT failed with `e` (v0.16 review fix;
    /// `None`: none, the statement fails as it is). The pin streams
    /// such a SELECT into the target row by row — unless the target
    /// has INSERT triggers or the SELECT reads it (a temporary table
    /// then holds every row first), or it sorts, groups, aggregates
    /// or compounds — so a `RAISE(FAIL | IGNORE)` reached on a later
    /// row keeps the rows before it. hipp computes the SELECT first:
    /// for such a lone scan of one table it is run again row by row
    /// to the failing row, and those rows are inserted. The flag:
    /// whether that run failed too (it may not: the pin stops at a
    /// LIMIT before the row hipp's whole-SELECT run failed on).
    pub(super) async fn insert_prefix(
        &mut self,
    ) -> Result<Option<(AVec<AVec<OwnedValue>>, bool)>, S::Error> {
        if !self.conn.in_trigger()
            || self.fires(TrigTime::Before, TrigEvent::Insert)
            || self.fires(TrigTime::After, TrigEvent::Insert)
        {
            return Ok(None);
        }
        let (plan, target) = match &self.plan {
            Plan::Insert {
                select: Some(p),
                root,
                ..
            } => (p.clone(), *root),
            _ => return Ok(None),
        };
        let Some(src) = plan.sources.as_slice().first() else {
            return Ok(None);
        };
        #[cfg(feature = "window")]
        let windowed =
            !crate::window::calls(plan.items.as_slice(), plan.order_by.as_slice()).is_empty();
        #[cfg(not(feature = "window"))]
        let windowed = false;
        if plan.sources.len() != 1
            || src.derived.is_some()
            || src.rec != 0
            || src.on.is_some()
            || src.root == target
            || plan.rec.is_some()
            || !plan.compound.is_empty()
            || !plan.values.is_empty()
            || !plan.group_by.is_empty()
            || plan.has_agg
            || !plan.order_by.is_empty()
            || windowed
            || (plan.distinct && (plan.limit.is_some() || plan.offset.is_some()))
            || plan.items.iter().any(crate::query::has_sub)
            || plan.filter.as_ref().is_some_and(crate::query::has_sub)
        {
            return Ok(None);
        }
        let (skip, take) = self.limit_window(&plan.limit, &plan.offset).await?;
        let empty = RowData::empty();
        let src_rows = self
            .materialise_sources_sub(plan.sources.as_slice(), &empty, None)
            .await?;
        let mut produced = AVec::new();
        let mut failed = false;
        for (vals, rowid) in src_rows
            .as_slice()
            .first()
            .map_or(&[][..], |r| r.as_slice())
        {
            if produced.len() >= skip.saturating_add(take) {
                break;
            }
            let mut acc = RowData::empty();
            for v in vals.iter() {
                acc.vals.push(v.clone());
            }
            acc.rowids.push(*rowid);
            if let Some(f) = &plan.filter {
                match self.eval_row_sub(f, &acc).await {
                    Ok(v) if crate::query::truthy(&v) == Some(true) => {}
                    Ok(_) => continue,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            match self
                .sub_project(&plan, &empty, core::slice::from_ref(&acc), None)
                .await
            {
                Ok(p) => produced.extend_from_slice(p.as_slice()),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if plan.distinct {
            produced = dedup_produced(
                produced,
                crate::query::exprs_colls(plan.items.as_slice()).as_slice(),
            );
        }
        let mut rows = AVec::new();
        for p in produced.iter().skip(skip).take(take) {
            rows.push(p.out.clone());
        }
        Ok(Some((rows, failed)))
    }

    /// The end of a trigger program's write that failed part-way
    /// with `e` after changing rows (`insert_prefix`, `one_pass`): a
    /// `RAISE(IGNORE)` ends the program normally, so the rows count
    /// in `total_changes()`, as on the pin.
    pub(super) fn failed_part_way(&mut self, e: Error<S::Error>) -> Error<S::Error> {
        if matches!(
            e,
            Error::Schema {
                kind: SchemaKind::RaiseIgnore
            }
        ) {
            self.conn.env.finish(true);
        }
        e
    }

    /// Whether an UPDATE / DELETE of a trigger program whose WHERE
    /// failed part-way keeps the rows it had changed before the
    /// failing one (v0.16 review fix): the pin's one-pass write —
    /// no triggers for the change on the target, no foreign keys to
    /// check, no subquery in the WHERE, and (UPDATE) no rowid change
    /// or REPLACE — changes each row as the scan passes it.
    pub(super) fn one_pass(
        &self,
        event: TrigEvent,
        changed: Option<(&[bool], bool)>,
        filter: Option<&RExpr>,
        replace: bool,
    ) -> bool {
        if !self.conn.in_trigger() || replace || filter.is_some_and(crate::query::has_sub) {
            return false;
        }
        #[cfg(feature = "foreign_keys")]
        if self.fk.is_some() {
            return false;
        }
        if changed.is_some_and(|(_, rowid)| rowid) {
            return false;
        }
        match (self.trig.as_ref(), changed) {
            (None, _) => true,
            (Some(tg), Some((cols, rowid))) => !tg.trigs.iter().any(|t| {
                t.event == event
                    && t.time != TrigTime::Instead
                    && Self::of_overlap(tg, t, cols, rowid)
            }),
            (Some(tg), None) => !tg
                .trigs
                .iter()
                .any(|t| t.event == event && t.time != TrigTime::Instead),
        }
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
                && !Self::of_overlap(&tg, t, cols, rowid_set)
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
    /// Whether it was such a failure. A FAIL halt with foreign-key
    /// violations outstanding becomes their failure (`e` replaced).
    pub(super) async fn trigger_failed(&mut self, e: &mut Error<S::Error>) -> bool {
        if self.conn.in_trigger() {
            return false;
        }
        let Some(r) = self.conn.trig.resolve.take() else {
            return false;
        };
        // The pin's FAIL halt still checks the statement's foreign-key
        // violations (`foreign_keys`): with any, it aborts with their
        // error (v0.16 review fix: hipp kept the program's own).
        #[cfg(feature = "foreign_keys")]
        let r = if r == Resolve::Fail
            && let Err(fe) = self.fk_end()
        {
            *e = fe;
            Resolve::Abort
        } else {
            r
        };
        let e = &*e;
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
            // A depth failure in an explicit transaction, where no
            // program of the statement holds an abort point, keeps
            // the changes: the pin opened no statement journal
            // (`nest_may_abort`, v0.16 review fix).
            Resolve::Abort
                if self.conn.explicit_txn
                    && matches!(
                        e,
                        Error::Schema {
                            kind: SchemaKind::TriggerDepth
                        }
                    )
                    && !self.conn.nest_may_abort(
                        copy_vec(handle_bytes(&self.arena, self.sql_text)).as_slice(),
                        &self.arena,
                    ) =>
            {
                self.conn.env.finish(true);
                self.keep_stmt();
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
        if self.ret_loaded {
            return self.serve_returning_fk().await;
        }
        let r = self.exec_instead().await;
        match r {
            Ok(()) if self.has_returning() => {
                self.ret_loaded = true;
                self.ret_pos = 0;
                self.serve_returning_fk().await
            }
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(mut e) => {
                if !self.trigger_failed(&mut e).await {
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
                    if self
                        .fire(
                            TrigTime::Instead,
                            TrigEvent::Insert,
                            None,
                            None,
                            Some((new.as_slice(), 0)),
                        )
                        .await?
                    {
                        self.instead_returning(&ip, new.as_slice()).await?;
                    }
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
                    if self
                        .fire(
                            TrigTime::Instead,
                            TrigEvent::Update,
                            Some((ip.assigned.as_slice(), false)),
                            Some((old, 0)),
                            Some((new.as_slice(), 0)),
                        )
                        .await?
                    {
                        self.instead_returning(&ip, new.as_slice()).await?;
                    }
                }
                TrigEvent::Delete => {
                    let old = r.get(..ip.ncols).unwrap_or(r);
                    if self
                        .fire(
                            TrigTime::Instead,
                            TrigEvent::Delete,
                            None,
                            Some((old, 0)),
                            None,
                        )
                        .await?
                    {
                        self.instead_returning(&ip, old).await?;
                    }
                }
            }
        }
        // The trigger bodies' writes counted foreign-key violations
        // into the statement (`foreign_keys`): the pin checks them
        // when the view's statement halts, as for a table's.
        #[cfg(feature = "foreign_keys")]
        self.fk_end_commit().await?;
        #[cfg(not(feature = "foreign_keys"))]
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        self.conn.env.finish(true);
        Ok(())
    }

    /// One RETURNING row of a write of a view (`InsteadPlan::returning`).
    async fn instead_returning(
        &mut self,
        ip: &InsteadPlan,
        row: &[OwnedValue],
    ) -> Result<(), S::Error> {
        if ip.returning.is_empty() {
            return Ok(());
        }
        let mut rd = RowData::empty();
        rd.vals.extend_from_slice(row);
        rd.rowids.push(None);
        let vals = self.eval_returning(ip.returning.as_slice(), &rd).await?;
        self.ret_rows.push(vals);
        self.ret_rowids.push(None);
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
