//! The TEMP database (v0.16, `v16`).
//!
//! A connection's temp schema lives in a second, private
//! connection over a `MemStorage` (`Connection::temp`), created the
//! first time a statement names it and dropped with the connection:
//! nothing of it ever reaches the database file. A statement is
//! routed before it is planned, by the pin's name resolution:
//!
//! - `CREATE TEMP | TEMPORARY TABLE / VIEW`, `CREATE TABLE temp.t`
//!   and any `temp.`-qualified name go to the temp database;
//! - an unqualified name means the temp database's object of that
//!   name when it has one, else the main database's (temp first);
//!   `sqlite_temp_schema` / `sqlite_temp_master` are the temp
//!   database's schema table;
//! - CREATE INDEX goes where its table is; a CREATE VIEW over temp
//!   objects only is a temp view, as on the pin;
//! - a statement whose names all resolve to the temp database runs
//!   there, with `TEMP` and the `temp.` qualifiers of its names
//!   removed from its text (the temp database is the private
//!   connection's main) — a TEMP view's text is stored as written,
//!   as on the pin; a `temp.a` naming a column of a table or alias
//!   called `temp` is left alone;
//! - a TEMP view may read main tables: a statement reading such
//!   views (and no temp table) runs on main with the views written
//!   in as common table expressions (`inline_temp_views`); an ALTER
//!   TABLE of a main table such a view reads is refused (the pin
//!   rewrites the view);
//! - (v0.16 honest-gap sweep) a statement of the main database may
//!   read temp tables (`plan_temp_source`: an unqualified name is
//!   temp first), and a temp INSERT ... SELECT / CREATE TABLE ... AS
//!   SELECT that reads main is fed the SELECT's rows from main
//!   (`Route::TempFed`); a temp UPDATE / DELETE that reads main is
//!   still refused (`Unsupported`).
//!
//! The connection-wide pragmas (`foreign_keys`, `case_sensitive_like`,
//! `ignore_check_constraints`, ...) are the main connection's, copied
//! to the temp database before each statement there (`sync_temp`);
//! `PRAGMA temp.foreign_keys` sets the connection's, as on the pin.
//!
//! BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO are
//! applied to both databases (a temp database created inside a
//! transaction first replays the open transaction and savepoints),
//! so the temp schema commits and rolls back with the main one. The
//! connection's `last_insert_rowid()` / `changes()` /
//! `total_changes()` follow writes to either.

use super::*;
use crate::arena::ABytes;
use crate::sql::lexer::{Tok, Token};
use crate::storage::{MemError, MemStorage};

/// Where a statement runs.
pub(super) enum Route {
    Main,
    /// On the temp database, as this text.
    Temp(AVec<u8>),
    /// On the main database, as this text: a statement reading TEMP
    /// views over main tables, with those views written in as
    /// common table expressions (`Connection::inline_temp_views`).
    MainAs(AVec<u8>),
    /// A write of the temp database fed by a SELECT that reads the
    /// main one (v0.16; `Connection::prepare_temp_fed`).
    TempFed(Box<FedRoute>),
}

/// How a temp write reads main (v0.16, `Route::TempFed`).
pub(super) struct FedRoute {
    /// The SELECT, as main's text.
    feed: AVec<u8>,
    /// The temp INSERT, its source replaced by a VALUES row of
    /// NULLs (`None` for CREATE TABLE ... AS, whose INSERT is made
    /// from the table's columns).
    insert: Option<AVec<u8>>,
    /// CREATE TABLE ... AS: the new table's name.
    ctas: Option<AVec<u8>>,
    /// The INSERT's VALUES width.
    width: usize,
}

/// The planned main half of a temp write that reads main (v0.16,
/// `Plan::Temp::feed`): the SELECT, and for CREATE TABLE ... AS the
/// temp database's CREATE TABLE text (run first; the statement's own
/// text is then the INSERT of the rows).
pub(super) struct TempFeed {
    select: Box<crate::query::SubPlan>,
    create: Option<AVec<u8>>,
}

/// The temp database's errors under the connection's storage type
/// (its storage is memory: it fails only as memory does).
pub(super) fn mem_err<E>(e: Error<MemError>) -> Error<E> {
    e.map_storage(|_| Error::Unsupported)
}

/// A boxed future (the temp database is a `Connection` too, so its
/// calls recurse at the type level).
type Boxed<'x, T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + 'x>>;

/// What one name in a statement refers to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A table or view read or written.
    Object,
    /// An index named by DROP INDEX.
    Index,
    /// The table qualifier of a column (`q.col`, `q.*`): no
    /// object; a `temp` here is a table or alias called so.
    Qual,
    /// A table-valued function's name (`FROM f(...)`): no object of
    /// a database, though a view or trigger may not qualify it with
    /// another's name (v0.16, `attach`).
    #[cfg(feature = "attach")]
    Func,
}

/// Every table / view name a SELECT reads (handles), and its CTE
/// names (never routed).
fn select_names(s: &SelectStmt, out: &mut AVec<(ABytes, Kind)>, ctes: &mut AVec<ABytes>) {
    if let Some(w) = &s.with {
        for c in w.ctes.iter() {
            ctes.push(c.name);
            select_names(&c.select, out, ctes);
        }
    }
    core_names(s, out, ctes);
    for (_, c) in s.compound.iter() {
        core_names(c, out, ctes);
    }
    for (e, _) in s.order_by.iter() {
        expr_names(e, out, ctes);
    }
    if let Some(e) = &s.limit {
        expr_names(e, out, ctes);
    }
    if let Some(e) = &s.offset {
        expr_names(e, out, ctes);
    }
}

fn core_names(s: &SelectStmt, out: &mut AVec<(ABytes, Kind)>, ctes: &mut AVec<ABytes>) {
    from_names(s.from.as_slice(), out, ctes);
    for it in s.items.iter() {
        match it {
            SelectItem::Expr(e, _, _) => expr_names(e, out, ctes),
            SelectItem::TableStar(h) => out.push((*h, Kind::Qual)),
            SelectItem::Star => {}
        }
    }
    for e in s
        .filter
        .iter()
        .chain(s.having.iter())
        .chain(s.group_by.iter())
    {
        expr_names(e, out, ctes);
    }
    #[cfg(feature = "window")]
    for (_, w) in s.windows.iter() {
        w.exprs(&mut |x| expr_names(x, out, ctes));
    }
    for row in s.values.iter() {
        for e in row.iter() {
            expr_names(e, out, ctes);
        }
    }
}

fn from_names(
    from: &[crate::sql::FromSource],
    out: &mut AVec<(ABytes, Kind)>,
    ctes: &mut AVec<ABytes>,
) {
    for f in from {
        match &f.derived {
            Some(d) => select_names(d, out, ctes),
            None if f.args.is_none() => out.push((f.table, Kind::Object)),
            #[cfg(feature = "attach")]
            None => out.push((f.table, Kind::Func)),
            #[cfg(not(feature = "attach"))]
            None => {}
        }
        if let Some(args) = &f.args {
            for e in args.iter() {
                expr_names(e, out, ctes);
            }
        }
        if let Some(e) = &f.on {
            expr_names(e, out, ctes);
        }
    }
}

fn expr_names(e: &Expr, out: &mut AVec<(ABytes, Kind)>, ctes: &mut AVec<ABytes>) {
    use crate::sql::Expr as X;
    match e {
        X::Column { table: Some(h), .. } => out.push((*h, Kind::Qual)),
        X::InSubquery(a, s, _) => {
            expr_names(a, out, ctes);
            select_names(s, out, ctes);
        }
        X::ScalarSubquery(s) | X::ExistsSubquery(s) => select_names(s, out, ctes),
        X::Binary(_, a, b) | X::Like(a, b, _) => {
            expr_names(a, out, ctes);
            expr_names(b, out, ctes);
        }
        X::Neg(a) | X::Pos(a) | X::Not(a) | X::IsNull(a, _) | X::Cast(a, _) => {
            expr_names(a, out, ctes)
        }
        X::Collate(a, _) | X::BitNot(a) | X::RowItem(a, _, _) => expr_names(a, out, ctes),
        X::InList(a, l, _) => {
            expr_names(a, out, ctes);
            for x in l.iter() {
                expr_names(x, out, ctes);
            }
        }
        X::Vector(l) => {
            for x in l.iter() {
                expr_names(x, out, ctes);
            }
        }
        X::Between(a, b, c, _) => {
            expr_names(a, out, ctes);
            expr_names(b, out, ctes);
            expr_names(c, out, ctes);
        }
        X::Case {
            base,
            whens,
            otherwise,
        } => {
            for b in base.iter() {
                expr_names(b, out, ctes);
            }
            for (w, t) in whens.iter() {
                expr_names(w, out, ctes);
                expr_names(t, out, ctes);
            }
            for o in otherwise.iter() {
                expr_names(o, out, ctes);
            }
        }
        X::Func { args, clause, .. } => {
            for a in args.iter() {
                expr_names(a, out, ctes);
            }
            if let Some(c) = clause {
                for (x, _) in c.order_by.iter() {
                    expr_names(x, out, ctes);
                }
                for f in c.filter.iter() {
                    expr_names(f, out, ctes);
                }
                #[cfg(feature = "window")]
                if let Some(crate::sql::ast::Over::Spec(w)) = &c.over {
                    w.exprs(&mut |x| expr_names(x, out, ctes));
                }
            }
        }
        _ => {}
    }
}

fn returning_names(items: &[SelectItem], out: &mut AVec<(ABytes, Kind)>, ctes: &mut AVec<ABytes>) {
    for it in items {
        match it {
            SelectItem::Expr(e, _, _) => expr_names(e, out, ctes),
            SelectItem::TableStar(h) => out.push((*h, Kind::Qual)),
            SelectItem::Star => {}
        }
    }
}

/// The names a statement refers to, the name it creates (with
/// whether it is an index), and its CTE names.
pub(super) fn ast_names(
    ast: &Ast,
    out: &mut AVec<(ABytes, Kind)>,
    ctes: &mut AVec<ABytes>,
) -> Option<(ABytes, bool)> {
    match ast {
        Ast::Select(s) => select_names(s, out, ctes),
        Ast::With { with, body } => {
            for c in with.ctes.iter() {
                ctes.push(c.name);
                select_names(&c.select, out, ctes);
            }
            return ast_names(body, out, ctes);
        }
        Ast::Insert {
            table,
            rows,
            select,
            upsert,
            returning,
            ..
        } => {
            out.push((*table, Kind::Object));
            for r in rows.iter() {
                for e in r.iter() {
                    expr_names(e, out, ctes);
                }
            }
            if let Some(s) = select {
                select_names(s, out, ctes);
            }
            if let Some(u) = upsert {
                for e in u.target_filter.iter() {
                    expr_names(e, out, ctes);
                }
                if let crate::sql::UpsertAction::Update {
                    assignments,
                    filter,
                } = &u.action
                {
                    for (_, e) in assignments.iter() {
                        expr_names(e, out, ctes);
                    }
                    for e in filter.iter() {
                        expr_names(e, out, ctes);
                    }
                }
            }
            returning_names(returning.as_slice(), out, ctes);
        }
        Ast::Update {
            table,
            assignments,
            from,
            filter,
            returning,
            ..
        } => {
            out.push((*table, Kind::Object));
            for (_, e) in assignments.iter() {
                expr_names(e, out, ctes);
            }
            from_names(from.as_slice(), out, ctes);
            for e in filter.iter() {
                expr_names(e, out, ctes);
            }
            returning_names(returning.as_slice(), out, ctes);
        }
        Ast::Delete {
            table,
            filter,
            returning,
            ..
        } => {
            out.push((*table, Kind::Object));
            for e in filter.iter() {
                expr_names(e, out, ctes);
            }
            returning_names(returning.as_slice(), out, ctes);
        }
        Ast::DropTable { name, .. } | Ast::DropView { name, .. } => out.push((*name, Kind::Object)),
        Ast::DropIndex { name, .. } => out.push((*name, Kind::Index)),
        Ast::AlterAddColumn { table, .. }
        | Ast::AlterRename { table, .. }
        | Ast::AlterRenameColumn { table, .. }
        | Ast::AlterDropColumn { table, .. } => out.push((*table, Kind::Object)),
        Ast::CreateTable {
            name, as_select, ..
        } => {
            if let Some(s) = as_select {
                select_names(s, out, ctes);
            }
            return Some((*name, false));
        }
        Ast::CreateView { name, select, .. } => {
            select_names(select, out, ctes);
            return Some((*name, false));
        }
        Ast::CreateIndex { name, table, .. } => {
            out.push((*table, Kind::Object));
            return Some((*name, true));
        }
        _ => {}
    }
    None
}

/// The token whose handle is `h`, by index.
pub(super) fn tok_index(toks: &[Token], h: ABytes) -> Option<usize> {
    if h.len == 0 {
        return None;
    }
    toks.iter()
        .position(|t| matches!(t.tok, Tok::Ident(x, _) | Tok::Text(x) if x == h))
}

/// The schema qualifier written before token `i`: `Some(true)` for
/// `temp.`, `Some(false)` for `main.`.
fn qualifier(toks: &[Token], i: usize, arena: &Arena) -> Option<bool> {
    let dot = i.checked_sub(1)?;
    if toks.get(dot)?.tok != Tok::Dot {
        return None;
    }
    match toks.get(dot.checked_sub(1)?)?.tok {
        Tok::Ident(h, _) if eq_ci(handle_bytes(arena, h), b"temp") => Some(true),
        Tok::Ident(h, _) if eq_ci(handle_bytes(arena, h), b"main") => Some(false),
        _ => None,
    }
}

/// The bytes of the `temp.` qualifier before token `i` (its start
/// to the name's): what the temp database's text leaves out.
pub(super) fn qual_cut(toks: &[Token], i: usize) -> Option<(usize, usize)> {
    Some((toks.get(i.checked_sub(2)?)?.offset, toks.get(i)?.offset))
}

/// `src` with `cuts` applied: a `(a, b)` cut removes bytes `a..b`
/// (a `temp.` qualifier); an empty `(a, a)` one renames the
/// `sqlite_temp_schema` / `sqlite_temp_master` token at `a`, bare
/// or quoted, to `sqlite_schema`.
pub(super) fn apply_cuts(src: &[u8], cuts: &mut AVec<(usize, usize)>) -> AVec<u8> {
    cuts.as_mut_slice().sort_by_key(|c| c.0);
    let mut out = AVec::new();
    let mut pos = 0usize;
    for &(a, b) in cuts.iter() {
        if a < pos || b > src.len() {
            continue;
        }
        out.extend_from_slice(src.get(pos..a).unwrap_or_default());
        if a == b {
            out.extend_from_slice(b"sqlite_schema");
            pos = token_end(src, a);
        } else {
            pos = b;
        }
    }
    out.extend_from_slice(src.get(pos.min(src.len())..).unwrap_or_default());
    out
}

/// The end of the name token starting at byte `a` of `src`: a
/// quoted name (`"x"`, `` `x` ``, `[x]`, `'x'`) through its closing
/// quote, a bare one through its last name byte.
fn token_end(src: &[u8], a: usize) -> usize {
    let close = match src.get(a) {
        Some(b'"') => b'"',
        Some(b'`') => b'`',
        Some(b'\'') => b'\'',
        Some(b'[') => b']',
        _ => {
            let mut e = a;
            while src
                .get(e)
                .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80)
            {
                e += 1;
            }
            return e;
        }
    };
    let mut e = a + 1;
    while let Some(&b) = src.get(e) {
        e += 1;
        if b == close {
            // A doubled quote is one quote character.
            if close != b']' && src.get(e) == Some(&close) {
                e += 1;
                continue;
            }
            return e;
        }
    }
    e
}

pub(super) fn is_temp_schema_name(n: &[u8]) -> bool {
    eq_ci(n, b"sqlite_temp_schema") || eq_ci(n, b"sqlite_temp_master")
}

impl<S: Storage> Connection<S> {
    /// Route `sql` (see the module docs).
    pub(super) fn route(&self, sql: &[u8], arena: &Arena) -> Result<Route, S::Error> {
        if self.is_temp {
            return Ok(Route::Main);
        }
        let Ok(toks) = crate::sql::parser::tokens::<S::Error>(sql, arena) else {
            return Ok(Route::Main);
        };
        let t = toks.as_slice();
        let word = |i: usize, w: &[u8]| matches!(t.get(i).map(|x| x.tok), Some(Tok::Ident(h, false)) if eq_ci(handle_bytes(arena, h), w));
        let temp_kw = word(0, b"CREATE") && (word(1, b"TEMP") || word(1, b"TEMPORARY"));
        // The pin's grammar has no TEMP INDEX: a syntax error at
        // INDEX / UNIQUE (hipp's parse would take it without TEMP).
        if temp_kw && (word(2, b"INDEX") || word(2, b"UNIQUE")) {
            return Err(Error::Parse {
                offset: t.get(2).map_or(0, |x| x.offset),
                kind: crate::error::ParseKind::UnexpectedToken,
            });
        }
        let mentions_temp = t.iter().any(|x| match x.tok {
            Tok::Ident(h, _) => {
                let n = handle_bytes(arena, h);
                eq_ci(n, b"temp") || is_temp_schema_name(n)
            }
            _ => false,
        });
        let temp_objects = self
            .temp
            .as_ref()
            .is_some_and(|c| !c.catalog.tables.is_empty() || !c.catalog.views.is_empty());
        if !temp_kw && !mentions_temp && !temp_objects {
            return Ok(Route::Main);
        }
        // The statement without its TEMP keyword.
        let mut text = AVec::new();
        match (temp_kw, t.get(1), t.get(2)) {
            (true, Some(a), Some(b)) => {
                text.extend_from_slice(&sql[..a.offset]);
                text.extend_from_slice(&sql[b.offset.min(sql.len())..]);
            }
            _ => text.extend_from_slice(sql),
        }
        let parsed = crate::sql::parser::parse_with_tokens::<S::Error>(
            text.as_slice(),
            arena,
            self.cfg.limits(),
        );
        let (ast, toks) = match parsed {
            Ok(a) => a,
            // A TEMP trigger may write a schema-qualified table (the
            // pin's rule against them is for the other databases'
            // triggers): hipp runs a trigger program in its own
            // database only, so that is refused (v0.16 review fix: it
            // was reported as the pin's error).
            #[cfg(feature = "triggers")]
            Err(Error::Schema {
                kind: SchemaKind::TriggerQualifiedTarget,
            }) if temp_kw => return Err(Error::Unsupported),
            // The main database's parse reports the error (or the
            // TEMP keyword is not followed by TABLE / VIEW).
            Err(e) if temp_kw => return Err(e),
            Err(_) => return Ok(Route::Main),
        };
        let t = toks.as_slice();
        // A TEMP object may not take a name a main virtual table's
        // module reserves (v0.16 review fix, `vtab`).
        #[cfg(feature = "vtab")]
        if let (true, Ast::CreateTable { name, .. } | Ast::CreateView { name, .. }) =
            (temp_kw, &ast)
        {
            self.shadow_name_free(handle_bytes(arena, *name))?;
        }
        // Triggers (v0.16, `triggers`): one on a TEMP table (TEMP or
        // not, as on the pin) lives in the temp database; a TEMP
        // trigger on a main table is refused (hipp runs a statement
        // in one database only); a main trigger may not name a
        // `temp.` object ("cannot reference objects in database
        // temp").
        #[cfg(feature = "triggers")]
        match &ast {
            Ast::CreateTrigger(ct) => {
                let tn = handle_bytes(arena, ct.table);
                let tq = tok_index(t, ct.table).and_then(|i| qualifier(t, i, arena));
                let nq = tok_index(t, ct.name).and_then(|i| qualifier(t, i, arena));
                // A `main.` trigger name puts the trigger, and its
                // table, in main (v0.16 review fix: the pin's "no such
                // table: main.tt" over a temp-only table).
                if nq == Some(false) && !temp_kw {
                    // Nor may its WHEN clause or body name a `temp.`
                    // object (v0.16 review fix: only the ON table was
                    // checked on this path).
                    if tq == Some(true)
                        || (0..t.len()).any(|i| qualifier(t, i, arena) == Some(true))
                    {
                        return Err(Error::Schema {
                            kind: SchemaKind::TriggerTempReference,
                        });
                    }
                    return Ok(Route::Main);
                }
                let on_temp = tq == Some(true) || (tq.is_none() && self.temp_has(tn, false));
                let named_temp = nq.is_some();
                if temp_kw && named_temp {
                    return Err(Error::Schema {
                        kind: SchemaKind::TriggerTempReference,
                    });
                }
                if !on_temp {
                    if temp_kw {
                        return Err(Error::Unsupported);
                    }
                    if (0..t.len()).any(|i| qualifier(t, i, arena) == Some(true)) {
                        return Err(Error::Schema {
                            kind: SchemaKind::TriggerTempReference,
                        });
                    }
                    return Ok(Route::Main);
                }
                // Stored as written (the temp database reads its own
                // `temp.` names).
                return Ok(Route::Temp(text));
            }
            Ast::DropTrigger { name, .. } => {
                let n = handle_bytes(arena, *name);
                let q = tok_index(t, *name).and_then(|i| qualifier(t, i, arena));
                let in_temp = self
                    .temp
                    .as_ref()
                    .is_some_and(|c| c.catalog.find_trigger(&c.catalog_arena, n).is_some());
                let in_main = self.catalog.find_trigger(&self.catalog_arena, n).is_some();
                let to_temp = q == Some(true) || (q.is_none() && in_temp && !in_main);
                if !to_temp {
                    return Ok(Route::Main);
                }
                let mut cuts = AVec::new();
                if let Some(c) = tok_index(t, *name).and_then(|i| qual_cut(t, i))
                    && q == Some(true)
                {
                    cuts.push(c);
                }
                return Ok(Route::Temp(apply_cuts(text.as_slice(), &mut cuts)));
            }
            _ => {}
        }
        // Transaction statements run on both (`mirror_temp`).
        if matches!(
            ast,
            Ast::Begin
                | Ast::BeginImmediate
                | Ast::Commit
                | Ast::Rollback
                | Ast::Savepoint { .. }
                | Ast::Release { .. }
                | Ast::RollbackTo { .. }
        ) {
            return Ok(Route::Main);
        }
        // PRAGMA temp.x runs there; an unqualified table pragma
        // follows its table.
        if let Ast::Pragma {
            schema,
            name,
            value,
            ..
        } = &ast
        {
            // A connection-wide setting is the main connection's
            // (the temp database runs with a copy, `sync_temp`).
            if super::pragma::lookup::<S::Error>(handle_bytes(arena, *name))
                .ok()
                .flatten()
                .is_some_and(|p| p.connection_wide())
            {
                return Ok(Route::Main);
            }
            let mut out = AVec::new();
            match schema {
                Some(h) if eq_ci(handle_bytes(arena, *h), b"temp") => {
                    let i = tok_index(t, *h).unwrap_or(0);
                    let (a, b) = match (t.get(i), t.get(i + 2)) {
                        (Some(a), Some(b)) => (a.offset, b.offset),
                        _ => return Err(Error::Unsupported),
                    };
                    out.extend_from_slice(&text.as_slice()[..a]);
                    out.extend_from_slice(&text.as_slice()[b.min(text.len())..]);
                }
                None if value.is_some_and(|v| {
                    let n = handle_bytes(arena, v);
                    self.temp_has(n, false) || self.temp_has(n, true)
                }) =>
                {
                    out.extend_from_slice(text.as_slice());
                }
                _ => return Ok(Route::Main),
            }
            return Ok(Route::Temp(out));
        }
        // REINDEX / ANALYZE: `temp.x` and a temp object's name run
        // there; `ANALYZE temp` analyzes the whole temp database
        // (creating it, as on the pin). A REINDEX name is a
        // collation's first.
        if let Ast::Reindex { schema, name } | Ast::Analyze { schema, name } = &ast {
            let analyze = matches!(ast, Ast::Analyze { .. });
            let n = name.map(|h| handle_bytes(arena, h));
            if analyze && schema.is_none() && n.is_some_and(|n| eq_ci(n, b"temp")) {
                return Ok(Route::Temp(copy_vec(b"ANALYZE")));
            }
            let mut out = AVec::new();
            match (schema, n) {
                (Some(h), _) if eq_ci(handle_bytes(arena, *h), b"temp") => {
                    let i = tok_index(t, *h).unwrap_or(0);
                    let (a, b) = match (t.get(i), t.get(i + 2)) {
                        (Some(a), Some(b)) => (a.offset, b.offset),
                        _ => return Err(Error::Unsupported),
                    };
                    out.extend_from_slice(&text.as_slice()[..a]);
                    out.extend_from_slice(&text.as_slice()[b.min(text.len())..]);
                }
                (None, Some(n))
                    if (analyze || crate::record::Coll::named(n).is_none())
                        && (self.temp_has(n, false) || self.temp_has(n, true)) =>
                {
                    out.extend_from_slice(text.as_slice());
                }
                _ => return Ok(Route::Main),
            }
            return Ok(Route::Temp(out));
        }
        let mut names = AVec::new();
        let mut ctes = AVec::new();
        let created = ast_names(&ast, &mut names, &mut ctes);
        let mut temp = 0usize;
        let mut main = 0usize;
        let mut cuts: AVec<(usize, usize)> = AVec::new();
        // Tokens of the routed names (their `temp.` is cut), and the
        // temp schema table's names, however quoted (renamed).
        let mut routed: AVec<usize> = AVec::new();
        let mut renames: AVec<usize> = AVec::new();
        // The names that are TEMP views.
        let mut views: AVec<AVec<u8>> = AVec::new();
        // The `temp` tokens that qualify a column, not a name.
        let mut col_quals: AVec<usize> = AVec::new();
        for (h, kind) in names.iter() {
            let n = handle_bytes(arena, *h);
            // A name the parser copied (`x IN [schema.]t`) is found
            // by its text among the qualified names not yet placed.
            let i = tok_index(t, *h).or_else(|| {
                (0..t.len()).find(|&k| {
                    matches!(t[k].tok, Tok::Ident(y, _) if eq_ci(handle_bytes(arena, y), n))
                        && qualifier(t, k, arena).is_some()
                        && !routed.as_slice().contains(&k)
                })
            });
            #[cfg(feature = "attach")]
            if *kind == Kind::Func {
                continue;
            }
            if *kind == Kind::Qual {
                if let Some(i) = i {
                    col_quals.push(i);
                }
                continue;
            }
            let q = i.and_then(|i| qualifier(t, i, arena));
            if q.is_none() && ctes.iter().any(|c| eq_ci(handle_bytes(arena, *c), n)) {
                continue;
            }
            if let Some(i) = i {
                routed.push(i);
                if q != Some(false) && is_temp_schema_name(n) {
                    renames.push(i);
                }
            }
            // The temp schema table is never main's.
            if q == Some(false) && is_temp_schema_name(n) {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
            let in_temp = match q {
                Some(true) => true,
                Some(false) => false,
                None => is_temp_schema_name(n) || self.temp_has(n, *kind == Kind::Index),
            };
            if let (Some(true), Some(c)) = (q, i.and_then(|i| qual_cut(t, i))) {
                cuts.push(c);
            }
            if in_temp && *kind == Kind::Object && self.temp_view(n).is_some() {
                views.push(copy_vec(n));
            }
            if in_temp {
                temp += 1;
            } else if q.is_some() || self.main_has(n, *kind == Kind::Index) {
                main += 1;
            }
            // A name in neither database binds to neither: the
            // statement fails (or, for a view, is stored) wherever
            // the rest of it runs, as on the pin.
        }
        // The created object: TEMP, `temp.`, or where it reads.
        let mut create_temp = false;
        let mut created_cut = None;
        if let Some((h, is_index)) = created {
            let q = tok_index(t, h).and_then(|i| qualifier(t, i, arena));
            // `CREATE INDEX main.i ON t` looks `t` up in main only.
            if is_index && q == Some(false) {
                return Ok(Route::Main);
            }
            if let Some(i) = tok_index(t, h) {
                routed.push(i);
            }
            if q == Some(true) {
                if let Some(c) = tok_index(t, h).and_then(|i| qual_cut(t, i)) {
                    cuts.push(c);
                    created_cut = Some(c);
                }
                create_temp = true;
            } else if temp_kw {
                if q == Some(false) {
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownDatabase,
                    });
                }
                create_temp = true;
            } else if is_index {
                create_temp = temp > 0 && main == 0;
            } else if matches!(ast, Ast::CreateView { .. }) {
                // A main view may name temp objects (stored, read
                // later), but not `temp.` ones ("view v cannot
                // reference objects in database temp").
                if cuts.iter().any(|_| true) {
                    return Err(Error::Unsupported);
                }
                return Ok(Route::Main);
            }
            if create_temp && is_index && main > 0 {
                return Err(Error::Unsupported);
            }
        }
        // A `temp.` the routing did not place (`temp.x.y`, a
        // three-part name; `temp.f(...)`; `x IN temp.t`), which hipp
        // does not resolve. A `temp.a` whose `temp` qualifies a
        // column (a table or alias called `temp`) is no schema and
        // stays as written.
        // v0.16: the `temp.` of a three-part column name
        // `temp.x.col` (its table token `x` qualifies a column) is
        // placed: it binds where the statement runs (the temp
        // database's resolver takes `temp.`, main's `main.`, and each
        // refuses the other's, as on the pin).
        let col3 = |i: usize| {
            col_quals.as_slice().contains(&i)
                && matches!(t.get(i + 1).map(|x| x.tok), Some(Tok::Dot))
        };
        // A CHECK constraint's or partial index's three-part name
        // (`temp.t.a`), kept as text by the parser: the pin ignores
        // its schema part (`query::DB_ANY`), and elsewhere in a table
        // or index definition (generated columns, key expressions)
        // the main resolver refuses it as the pin does (v0.16 review
        // fix: such definitions were refused).
        let def = matches!(
            ast,
            Ast::CreateTable {
                as_select: None,
                ..
            } | Ast::CreateIndex { .. }
        );
        let def3 = |i: usize| def && matches!(t.get(i + 1).map(|x| x.tok), Some(Tok::Dot));
        let unplaced = (0..t.len()).any(|i| {
            qualifier(t, i, arena) == Some(true)
                && !routed.as_slice().contains(&i)
                && !col3(i)
                && !def3(i)
                && !(col_quals.as_slice().contains(&(i.wrapping_sub(2)))
                    && !matches!(t.get(i + 1).map(|x| x.tok), Some(Tok::Dot | Tok::LParen)))
        });
        // TEMP views over main tables (and no temp table): the
        // statement runs on main with the views written in.
        // A main CREATE TABLE ... AS SELECT reading them gets them
        // after its `AS` (v0.16 review fix: "no such table").
        let ctas = match (&ast, created) {
            (
                Ast::CreateTable {
                    as_select: Some(_), ..
                },
                Some((h, false)),
            ) if !create_temp => tok_index(t, h).map(|i| i + 2),
            _ => None,
        };
        if (created.is_none() || ctas.is_some()) && !unplaced && !views.is_empty() {
            let target = match &ast {
                Ast::Insert { table, .. }
                | Ast::Update { table, .. }
                | Ast::Delete { table, .. } => Some(handle_bytes(arena, *table)),
                Ast::Select(_) | Ast::With { .. } => None,
                _ if ctas.is_some() => None,
                _ => Some(&b""[..]),
            };
            let dml_on_view = target.is_some_and(|n| n.is_empty() || self.temp_view(n).is_some());
            if !dml_on_view
                && self.target_in_main(&ast, t, arena)
                && renames.is_empty()
                && let Some(out) = self.inline_temp_views(
                    &text,
                    t,
                    views.as_slice(),
                    ctes.as_slice(),
                    cuts.as_slice(),
                    (temp > views.len(), main > 0 || ctas.is_some()),
                    ctas.unwrap_or(0),
                    arena,
                )?
            {
                return Ok(Route::MainAs(out));
            }
        }
        // v0.16: a temp write fed by a SELECT reading main (INSERT
        // INTO temp.x SELECT ..., CREATE TEMP TABLE x AS SELECT ...).
        // TEMP views over main count as main's (v0.16 review fix: their
        // feed is written in as CTEs, `prepare_temp_fed`).
        let mut fed_main = main > 0;
        if !fed_main && (temp > 0 || create_temp) {
            let (mut order, mut refs) = (AVec::new(), AVec::new());
            for v in views.iter() {
                fed_main |= self
                    .temp_view_closure(v.as_slice(), &mut order, &mut refs, 0, arena)?
                    .1;
            }
        }
        if (temp > 0 || create_temp) && fed_main && !unplaced && renames.is_empty() {
            // `CREATE TEMP TABLE IF NOT EXISTS x AS ...` over an
            // existing `x` does nothing, reading nothing.
            if let Ast::CreateTable {
                name,
                if_not_exists: true,
                as_select: Some(_),
                ..
            } = &ast
                && create_temp
                && self.temp_has(handle_bytes(arena, *name), false)
            {
                let mut noop = copy_vec(b"CREATE TABLE IF NOT EXISTS \"");
                for &b in handle_bytes(arena, *name) {
                    noop.push(b);
                    if b == b'"' {
                        noop.push(b'"');
                    }
                }
                noop.extend_from_slice(b"\" AS SELECT 1");
                return Ok(Route::Temp(noop));
            }
            if let Some(f) = self.fed_route(&ast, &text, t, create_temp, arena)? {
                return Ok(Route::TempFed(Box::new(f)));
            }
        }
        let to_temp = create_temp || (temp > 0 && main == 0 && created.is_none());
        if !to_temp {
            if unplaced {
                return Err(Error::Unsupported);
            }
            // v0.16: a statement of the main database may read TEMP
            // tables (`plan_temp_source`) — a query, a write of a main
            // table, CREATE TABLE ... AS SELECT in main — but not
            // write one, nor read the temp schema table.
            if temp > 0
                && (!renames.is_empty()
                    || !self.target_in_main(&ast, t, arena)
                    || !matches!(
                        ast,
                        Ast::Select(_)
                            | Ast::With { .. }
                            | Ast::Insert { .. }
                            | Ast::Update { .. }
                            | Ast::Delete { .. }
                            | Ast::CreateTable { .. }
                    ))
            {
                return Err(Error::Unsupported);
            }
            return Ok(Route::Main);
        }
        // A TEMP view may read main tables (stored; read through
        // `inline_temp_views`).
        let temp_view = create_temp && matches!(ast, Ast::CreateView { .. });
        if (main > 0 && !temp_view) || unplaced {
            return Err(Error::Unsupported);
        }
        // A view's text is stored as written but for its name's
        // `temp.`, as on the pin (the temp database reads `temp.x`
        // and `sqlite_temp_schema` as its own).
        if temp_view {
            cuts.clear();
            if let Some(c) = created_cut {
                cuts.push(c);
            }
            renames.clear();
        }
        // The text the temp database runs: no `temp.`, and its
        // schema table under its own name.
        for &i in renames.iter() {
            if let Some(x) = t.get(i) {
                cuts.push((x.offset, x.offset));
            }
        }
        Ok(Route::Temp(apply_cuts(text.as_slice(), &mut cuts)))
    }

    /// Some TEMP view reads the main table `table` (directly or
    /// through other TEMP views). The pin's ALTER TABLE of a main
    /// table rewrites those views too; hipp's refuses instead
    /// (`Unsupported`), so the view never reads a table the ALTER
    /// renamed away.
    pub(super) fn temp_views_read(&self, table: &[u8], arena: &Arena) -> bool {
        let Some(c) = self.temp.as_deref() else {
            return false;
        };
        c.catalog.views.iter().any(|v| {
            let name = copy_vec(handle_bytes(&c.catalog_arena, v.name));
            let ckpt = arena.checkpoint();
            let mut order = AVec::new();
            let mut refs = AVec::new();
            let r = self.temp_view_closure(name.as_slice(), &mut order, &mut refs, 0, arena);
            arena.rewind(ckpt);
            // A view hipp cannot read through counts as reading it.
            r.is_err() || refs.iter().any(|n| eq_ci(n.as_slice(), table))
        })
    }

    /// The temp database's view named so.
    pub(super) fn temp_view(&self, name: &[u8]) -> Option<&crate::schema::ViewDef> {
        let c = self.temp.as_deref()?;
        c.catalog.find_view(&c.catalog_arena, name)
    }

    /// Add the TEMP view `name` and the TEMP views it reads to
    /// `order` (each after the ones it reads), with its text as a
    /// common table expression, and the names they read to `refs`.
    /// Whether they read (a temp table, main objects).
    fn temp_view_closure(
        &self,
        name: &[u8],
        order: &mut AVec<(AVec<u8>, AVec<u8>)>,
        refs: &mut AVec<AVec<u8>>,
        depth: usize,
        arena: &Arena,
    ) -> Result<(bool, bool), S::Error> {
        if order.iter().any(|(n, _)| eq_ci(n.as_slice(), name)) {
            return Ok((false, false));
        }
        let (Some(v), Some(c)) = (self.temp_view(name), self.temp.as_deref()) else {
            return Err(Error::Unsupported);
        };
        if depth >= 32 {
            return Err(Error::Unsupported);
        }
        let sql = copy_vec(handle_bytes(&c.catalog_arena, v.sql));
        let sql = sql.as_slice();
        let (
            Ast::CreateView {
                name: vh, select, ..
            },
            toks,
        ) = crate::sql::parser::parse_with_tokens::<S::Error>(sql, arena, self.cfg.limits())?
        else {
            return Err(Error::Unsupported);
        };
        let t = toks.as_slice();
        // `name [(columns)] AS body`.
        let Some(ni) = tok_index(t, vh) else {
            return Err(Error::Unsupported);
        };
        let mut j = ni + 1;
        let mut depth_p = 0usize;
        let as_at = loop {
            match t.get(j).map(|x| x.tok) {
                None => return Err(Error::Unsupported),
                Some(Tok::LParen) => depth_p += 1,
                Some(Tok::RParen) => depth_p = depth_p.saturating_sub(1),
                Some(Tok::Ident(h, false))
                    if depth_p == 0 && eq_ci(handle_bytes(arena, h), b"AS") =>
                {
                    break j;
                }
                _ => {}
            }
            j += 1;
        };
        let (Some(cols_from), Some(cols_to), Some(body)) = (
            t.get(ni + 1).map(|x| x.offset),
            t.get(as_at).map(|x| x.offset),
            t.get(as_at + 1).map(|x| x.offset),
        ) else {
            return Err(Error::Unsupported);
        };
        let mut names = AVec::new();
        let mut ctes = AVec::new();
        select_names(&select, &mut names, &mut ctes);
        let (mut reads_temp, mut reads_main) = (false, false);
        for (h, kind) in names.iter() {
            #[cfg(feature = "attach")]
            if *kind == Kind::Func {
                continue;
            }
            if *kind == Kind::Qual {
                continue;
            }
            let n = handle_bytes(arena, *h);
            let q = tok_index(t, *h).and_then(|i| qualifier(t, i, arena));
            if q.is_none() && ctes.iter().any(|c| eq_ci(handle_bytes(arena, *c), n)) {
                continue;
            }
            refs.push(copy_vec(n));
            match q {
                Some(false) => reads_main = true,
                _ if is_temp_schema_name(n) => reads_temp = true,
                _ if self.temp_view(n).is_some() => {
                    let (a, b) = self.temp_view_closure(n, order, refs, depth + 1, arena)?;
                    reads_temp |= a;
                    reads_main |= b;
                }
                Some(true) => reads_temp = true,
                None if self.temp_has(n, false) => reads_temp = true,
                None => reads_main = true,
            }
        }
        // `"name"(columns) AS (body)`; the body may end in a `--`
        // comment.
        let mut cte = AVec::new();
        cte.push(b'"');
        for &b in name {
            cte.push(b);
            if b == b'"' {
                cte.push(b'"');
            }
        }
        cte.push(b'"');
        cte.extend_from_slice(sql.get(cols_from..cols_to).unwrap_or_default());
        cte.extend_from_slice(b"AS (");
        cte.extend_from_slice(sql.get(body..).unwrap_or_default());
        cte.extend_from_slice(b"\n)");
        order.push((copy_vec(name), cte));
        Ok((reads_temp, reads_main))
    }

    /// A statement reading TEMP `views` (and other temp objects,
    /// `temp`, and main objects, `main`)
    /// as main's text, the views (and those they read) written in
    /// as common table expressions before its own — when something
    /// reads main and none of them reads a temp table (refused when
    /// both). `None` when nothing reads main (the temp database
    /// runs it). `cuts` are its `temp.` qualifiers.
    #[allow(clippy::too_many_arguments)]
    fn inline_temp_views(
        &self,
        text: &AVec<u8>,
        t: &[Token],
        views: &[AVec<u8>],
        ctes: &[ABytes],
        cuts: &[(usize, usize)],
        (temp, main): (bool, bool),
        start: usize,
        arena: &Arena,
    ) -> Result<Option<AVec<u8>>, S::Error> {
        let mut order = AVec::new();
        let mut refs = AVec::new();
        let _ = temp;
        let mut reads_main = main;
        for v in views {
            let (_, b) = self.temp_view_closure(v.as_slice(), &mut order, &mut refs, 0, arena)?;
            reads_main |= b;
        }
        // Temp objects only: the temp database runs it (v0.16: with
        // main ones too, main runs it, reading the temp tables there).
        if !reads_main {
            return Ok(None);
        }
        // A CTE of the statement's own named like a view or a name
        // a view reads would take its place there.
        let clash = ctes.iter().any(|c| {
            let c = handle_bytes(arena, *c);
            order.iter().any(|(n, _)| eq_ci(n.as_slice(), c))
                || refs.iter().any(|r| eq_ci(r.as_slice(), c))
        });
        if clash {
            return Err(Error::Unsupported);
        }
        // The query starts at token `start` (after `CREATE TABLE x AS`
        // for CREATE TABLE ... AS SELECT; v0.16 review fix): the WITH
        // list goes in there, after the text before it as written.
        let head = t.get(start).map_or(0, |x| x.offset);
        if cuts.iter().any(|c| c.0 < head) {
            return Err(Error::Unsupported);
        }
        let mut cuts_v: AVec<(usize, usize)> = AVec::new();
        for &c in cuts {
            cuts_v.push((c.0.saturating_sub(head), c.1.saturating_sub(head)));
        }
        let body = apply_cuts(text.as_slice().get(head..).unwrap_or_default(), &mut cuts_v);
        let word = |i: usize, w: &[u8]| matches!(t.get(start + i).map(|x| x.tok), Some(Tok::Ident(h, false)) if eq_ci(handle_bytes(arena, h), w));
        let mut out = copy_vec(text.as_slice().get(..head).unwrap_or_default());
        out.extend_from_slice(b"WITH ");
        // The statement's own WITH [RECURSIVE] continues the list.
        let rest = if word(0, b"WITH") {
            let k = if word(1, b"RECURSIVE") {
                out.extend_from_slice(b"RECURSIVE ");
                2
            } else {
                1
            };
            let Some(at) = t.get(start + k).map(|x| x.offset.saturating_sub(head)) else {
                return Err(Error::Unsupported);
            };
            Some(at)
        } else {
            None
        };
        for (i, (_, cte)) in order.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b", ");
            }
            out.extend_from_slice(cte.as_slice());
        }
        match rest {
            Some(at) => {
                out.extend_from_slice(b", ");
                out.extend_from_slice(body.as_slice().get(at..).unwrap_or_default());
            }
            None => {
                out.push(b' ');
                out.extend_from_slice(body.as_slice());
            }
        }
        Ok(Some(out))
    }

    /// The main database may have an object (`index`: an index)
    /// named so: one in its catalog, or a name the planner resolves
    /// itself (`sqlite_*` tables, `pragma_*` functions).
    pub(super) fn main_has(&self, name: &[u8], index: bool) -> bool {
        let ca = &self.catalog_arena;
        let pre = |p: &[u8]| name.get(..p.len()).is_some_and(|h| eq_ci(h, p));
        pre(b"sqlite_")
            || pre(b"pragma_")
            || if index {
                self.catalog.find_index(ca, name).is_some()
            } else {
                self.catalog.taken(ca, name) == Some(false)
            }
    }

    /// Whether a write statement's target (an INSERT / UPDATE /
    /// DELETE table, a CREATE TABLE's new table) is a main table, or
    /// the statement writes none (v0.16): `main.x`, or an unqualified
    /// name the temp database does not have.
    fn target_in_main(&self, ast: &Ast, t: &[Token], arena: &Arena) -> bool {
        let target = match ast {
            Ast::Insert { table, .. }
            | Ast::Update { table, .. }
            | Ast::Delete { table, .. }
            | Ast::CreateTable { name: table, .. } => *table,
            Ast::With { body, .. } => return self.target_in_main(body, t, arena),
            _ => return true,
        };
        match tok_index(t, target).and_then(|i| qualifier(t, i, arena)) {
            Some(q) => !q,
            None => !self.temp_has(handle_bytes(arena, target), false),
        }
    }

    /// The temp database has an object (`index`: an index) named so.
    pub(super) fn temp_has(&self, name: &[u8], index: bool) -> bool {
        self.temp.as_ref().is_some_and(|c| {
            let ca = &c.catalog_arena;
            if index {
                c.catalog.find_index(ca, name).is_some()
            } else {
                c.catalog.taken(ca, name) == Some(false)
            }
        })
    }

    /// The temp database, created (and brought into the open
    /// transaction and savepoints) on first use, with the
    /// connection's settings (`sync_temp`).
    pub(super) async fn temp_conn(&mut self) -> Result<&mut Connection<MemStorage>, S::Error> {
        if self.temp.is_none() {
            let mut cfg = self.cfg.clone();
            cfg.vacuum_into = None;
            let open: Boxed<'_, Result<Connection<MemStorage>, MemError>> =
                Box::pin(Connection::open(MemStorage::new(), cfg));
            let mut c = open.await.map_err(mem_err)?;
            c.is_temp = true;
            // The TEMP database stores text as main does (v0.16,
            // `utf16`).
            #[cfg(feature = "utf16")]
            c.set_text_enc(self.pager.text_enc());
            // Replay the open transaction.
            let mut script: AVec<AVec<u8>> = AVec::new();
            if self.explicit_txn {
                let marks = self.pager.sp_marks();
                if !marks.as_slice().first().is_some_and(|m| m.1) {
                    script.push(copy_vec(b"BEGIN"));
                }
                for (name, _) in marks.iter() {
                    if let Some(n) = name {
                        let mut s = copy_vec(b"SAVEPOINT \"");
                        for &b in n.as_slice() {
                            s.push(b);
                            if b == b'"' {
                                s.push(b'"');
                            }
                        }
                        s.push(b'"');
                        script.push(s);
                    }
                }
            }
            for s in script.iter() {
                run_temp(&mut c, s.as_slice()).await.map_err(mem_err)?;
            }
            self.temp = Some(Box::new(c));
        }
        self.sync_temp();
        match self.temp.as_deref_mut() {
            Some(c) => Ok(c),
            None => Err(Error::Unsupported),
        }
    }

    /// Give the temp database the connection's settings: the
    /// connection-wide pragmas (`Prag::connection_wide`) hold for
    /// both databases on the pin, wherever they were set.
    pub(super) fn sync_temp(&mut self) {
        let (p, like) = (&self.prag, self.env.case_like.get());
        if let Some(c) = self.temp.as_deref_mut() {
            c.prag.foreign_keys = p.foreign_keys;
            c.prag.recursive_triggers = p.recursive_triggers;
            c.prag.ignore_checks = p.ignore_checks;
            c.prag.busy_timeout = p.busy_timeout;
            c.prag.like_redefined = p.like_redefined;
            c.env.case_like.set(like);
            // The encoding is the connection's: main's, chosen while
            // the TEMP schema is still unwritten (v0.16, `utf16`).
            #[cfg(feature = "utf16")]
            if c.pager.text_enc() != self.pager.text_enc() {
                c.set_text_enc(self.pager.text_enc());
            }
        }
    }

    /// Apply a transaction statement that succeeded on the main
    /// database to the temp database too.
    pub(super) async fn mirror_temp(&mut self, sql: &[u8]) {
        if let Some(c) = self.temp.as_deref_mut() {
            let _ = run_temp(c, sql).await;
        }
    }
}

/// Run one statement on the temp database to completion.
pub(super) async fn run_temp(c: &mut Connection<MemStorage>, sql: &[u8]) -> Result<(), MemError> {
    let Ok(text) = core::str::from_utf8(sql) else {
        return Err(Error::Unsupported);
    };
    let prep: Boxed<'_, Result<Statement<'_, MemStorage>, MemError>> = Box::pin(c.prepare(text));
    let mut st = prep.await?;
    loop {
        let step: Boxed<'_, Result<Step, MemError>> = Box::pin(st.step());
        if step.await? == Step::Done {
            return Ok(());
        }
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    /// Run a statement routed to the temp database (see the module
    /// docs): all of it at the first step, its rows kept to serve.
    pub(super) async fn step_temp(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.query_loaded {
            let Plan::Temp { sql, feed, .. } = &self.plan else {
                return Err(Error::Unsupported);
            };
            let sql = sql.clone();
            // A temp write reading main (v0.16): its rows first, read
            // here; a CTAS's table is created before they go in, and
            // it changes no counter (as on the pin).
            let (fed, create) = match feed {
                Some(f) => {
                    let (select, create) = (f.select.clone(), f.create.clone());
                    let rows = self.exec_subplan(&select, &RowData::empty()).await?;
                    (Some(rows), create)
                }
                None => (None, None),
            };
            #[cfg(feature = "params")]
            let binds = self.binds.values.clone();
            let env = &self.conn.env;
            let (last, changes, total) = (
                env.last_rowid.get(),
                env.changes.get(),
                env.total_changes.get(),
            );
            let c = self.conn.temp_conn().await?;
            let in_txn = c.explicit_txn;
            c.env.last_rowid.set(last);
            c.env.changes.set(changes);
            let before = c.env.total_changes.get();
            let Ok(text) = core::str::from_utf8(sql.as_slice()) else {
                return Err(Error::Unsupported);
            };
            let mut rows = AVec::new();
            let mut rowids = AVec::new();
            let r = async {
                if let Some(ct) = &create {
                    run_temp(c, ct.as_slice()).await?;
                }
                let prep: Boxed<'_, Result<Statement<'_, MemStorage>, MemError>> =
                    Box::pin(c.prepare(text));
                let mut st = prep.await?;
                if let (
                    Some(rows),
                    Plan::Insert {
                        rows_v,
                        rows_r,
                        select,
                        ..
                    },
                ) = (&fed, &mut st.plan)
                {
                    *rows_v = rows.clone();
                    *rows_r = None;
                    *select = None;
                }
                #[cfg(feature = "params")]
                for (i, v) in binds.iter().enumerate() {
                    st.bind_value(i + 1, v.as_value())?;
                }
                loop {
                    let step: Boxed<'_, Result<Step, MemError>> = Box::pin(st.step());
                    if step.await? == Step::Done {
                        return Ok::<(), Error<MemError>>(());
                    }
                    rows.push(st.current.clone());
                    rowids.push(st.current_rowid);
                }
            }
            .await;
            let (l2, c2, t2) = (
                c.env.last_rowid.get(),
                c.env.changes.get(),
                c.env.total_changes.get(),
            );
            let ended = in_txn && !c.explicit_txn;
            let env = &self.conn.env;
            if create.is_none() {
                env.last_rowid.set(l2);
                env.changes.set(c2);
                env.total_changes
                    .set(total.saturating_add(t2.saturating_sub(before)));
            }
            // A failure that ended the temp database's transaction
            // (OR ROLLBACK, an I/O-class error) ends the main one.
            if r.is_err() && ended && self.conn.explicit_txn {
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
            }
            r.map_err(mem_err)?;
            self.query_rows = rows;
            self.query_rowids = rowids;
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
}

impl<S: Storage> Connection<S> {
    /// A FROM source naming a TEMP table, read by a statement of the
    /// main database (v0.16): its columns as the temp catalog has
    /// them, its rows read from the temp database when the statement
    /// runs (`Statement::scan_temp`). The rows come in rowid order
    /// (no index access path: a temp index is not planned here).
    pub(super) fn plan_temp_source(
        &self,
        s: &crate::sql::FromSource,
        tc: &Connection<MemStorage>,
        t: &crate::schema::TableDef,
        arena: &Arena,
        descs: &mut AVec<crate::query::SourceDesc>,
        plans: &mut AVec<super::SourcePlan>,
    ) -> Result<usize, S::Error> {
        let ca = &tc.catalog_arena;
        let t = t.usable::<MemError>().map_err(mem_err)?;
        // `INDEXED BY` is checked against the temp table's indexes.
        tc.index_hint(arena, t, s.indexed).map_err(mem_err)?;
        let mut cols = AVec::new();
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        for c in t.columns.iter() {
            cols.push((copy_vec(handle_bytes(ca, c.name)), c.affinity));
            affs.push(c.affinity);
            defaults.push(super::read_default(ca, c));
        }
        let qual = match s.alias {
            Some(a) => copy_vec(handle_bytes(arena, a)),
            None => copy_vec(handle_bytes(arena, s.table)),
        };
        let mut desc = crate::query::SourceDesc::new(qual, cols, t.rowid_alias, t.has_rowid());
        desc.kind = s.kind;
        desc.colls = super::table_colls(t);
        desc.db = 2;
        descs.push(desc);
        let gcols = if t.has_generated() {
            tc.table_rules(arena, t)
                .map_err(mem_err)?
                .and_then(|r| r.gcols)
                .map(Box::new)
        } else {
            None
        };
        // A WITHOUT ROWID temp table is read through its own key
        // order, which this scan does not model.
        if !t.has_rowid() {
            return Err(Error::Unsupported);
        }
        plans.push(super::SourcePlan {
            #[cfg(feature = "explain")]
            eqp: Default::default(),
            root: t.root,
            kind: s.kind,
            on: None,
            rowid_alias: t.rowid_alias,
            ncols: t.columns.len(),
            affs,
            defaults,
            derived: None,
            gcols,
            rec: 0,
            #[cfg(any(feature = "json", feature = "vtab"))]
            tvf: 0,
            #[cfg(any(feature = "json", feature = "vtab"))]
            tvf_dead: AVec::new(),
            #[cfg(feature = "vtab")]
            vt: None,
            #[cfg(feature = "expr_index")]
            access: None,
            #[cfg(feature = "expr_index")]
            hint: crate::query::Hint::Any,
            temp: true,
            #[cfg(feature = "attach")]
            ext: 0,
        });
        Ok(t.columns.len())
    }
}

impl<S: Storage> Statement<'_, S> {
    /// The rows of a TEMP table source of a main statement (v0.16;
    /// `Connection::plan_temp_source`), read from the temp database
    /// in rowid order as a query reads them (defaults materialised,
    /// VIRTUAL columns computed, REAL columns forced).
    pub(super) async fn scan_temp(
        &mut self,
        sp: &super::SourcePlan,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        // Another database's table (v0.16, `attach`).
        #[cfg(feature = "attach")]
        if sp.ext != 0 {
            return self.scan_ext(sp).await;
        }
        let Some(tc) = self.conn.temp.as_deref_mut() else {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        };
        // The table must still be there as planned (a statement
        // prepared before a temp DROP / re-CREATE is not read from a
        // freed page).
        if !tc
            .catalog
            .tables
            .iter()
            .any(|t| t.root == sp.root && t.columns.len() == sp.ncols)
        {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        }
        let mut rows = AVec::new();
        let mut cursor = crate::btree::ScanCursor::new(sp.root);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor
            .next_row_into(&mut tc.pager, &mut buf)
            .await
            .map_err(mem_err)?
        {
            let virt = sp.gcols.as_ref().map(|g| g.virt.as_slice());
            let mut rec = crate::rules::decode_rec::<S::Error>(
                buf.as_slice(),
                sp.ncols,
                sp.defaults.as_slice(),
                virt,
            )?;
            if let Some(g) = &sp.gcols {
                g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
            }
            let rd =
                crate::rules::eval_row(sp.rowid_alias, sp.affs.as_slice(), rec.as_slice(), rowid);
            rows.push((rd.vals, Some(rowid)));
        }
        Ok(rows)
    }
}

impl<S: Storage> Connection<S> {
    /// The two halves of a temp write that reads main (v0.16), or
    /// `None` when the statement is not one: an `INSERT [OR ...] INTO
    /// <temp table> [(cols)] SELECT ...` with no upsert, RETURNING or
    /// leading WITH, or a `CREATE TEMP TABLE x AS SELECT ...`. The
    /// SELECT is the text after the target (its own WITH included);
    /// the temp half is the INSERT with a `VALUES` row in its place.
    pub(super) fn fed_route(
        &self,
        ast: &Ast,
        text: &AVec<u8>,
        t: &[Token],
        create_temp: bool,
        arena: &Arena,
    ) -> Result<Option<FedRoute>, S::Error> {
        let src = text.as_slice();
        match ast {
            Ast::Insert {
                table,
                columns,
                select: Some(_),
                upsert: None,
                returning,
                ..
            } if returning.is_empty() && !self.target_in_main(ast, t, arena) => {
                let Some(ti) = tok_index(t, *table) else {
                    return Ok(None);
                };
                let mut k = ti + 1;
                if matches!(t.get(k).map(|x| x.tok), Some(Tok::LParen)) {
                    while k < t.len() && !matches!(t.get(k).map(|x| x.tok), Some(Tok::RParen)) {
                        k += 1;
                    }
                    k += 1;
                }
                let Some(at) = t.get(k).map(|x| x.offset) else {
                    return Ok(None);
                };
                let width = match columns {
                    Some(c) => c.len(),
                    None => {
                        let Some(tc) = self.temp.as_deref() else {
                            return Ok(None);
                        };
                        let ca = &tc.catalog_arena;
                        let Some(tt) = tc.catalog.find(ca, handle_bytes(arena, *table)) else {
                            return Ok(None);
                        };
                        tt.columns.iter().filter(|c| c.gen_expr.is_none()).count()
                    }
                };
                let mut cuts = AVec::new();
                if qualifier(t, ti, arena) == Some(true)
                    && let Some(c) = qual_cut(t, ti)
                {
                    cuts.push(c);
                }
                let head = src.get(..at).unwrap_or_default();
                let mut insert = apply_cuts(head, &mut cuts);
                insert.extend_from_slice(b"VALUES (");
                for i in 0..width {
                    if i > 0 {
                        insert.push(b',');
                    }
                    insert.extend_from_slice(b"NULL");
                }
                insert.push(b')');
                Ok(Some(FedRoute {
                    feed: copy_vec(src.get(at..).unwrap_or_default()),
                    insert: Some(insert),
                    ctas: None,
                    width,
                }))
            }
            Ast::CreateTable {
                name,
                as_select: Some(_),
                ..
            } if create_temp => {
                // The SELECT follows the AS after the name.
                let Some(ni) = tok_index(t, *name) else {
                    return Ok(None);
                };
                let as_at = (ni + 1..t.len()).find(|&i| {
                    matches!(t[i].tok, Tok::Ident(h, false) if eq_ci(handle_bytes(arena, h), b"AS"))
                });
                let Some(at) = as_at.and_then(|i| t.get(i + 1)).map(|x| x.offset) else {
                    return Ok(None);
                };
                Ok(Some(FedRoute {
                    feed: copy_vec(src.get(at..).unwrap_or_default()),
                    insert: None,
                    ctas: Some(copy_vec(handle_bytes(arena, *name))),
                    width: 0,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Prepare a temp write that reads main (v0.16; `fed_route`):
    /// the SELECT is planned here, on main (it may read temp tables
    /// too), and the temp half prepared there.
    pub(super) async fn prepare_temp_fed(
        &mut self,
        f: FedRoute,
        arena: &Arena,
    ) -> Result<super::Plan, S::Error> {
        // TEMP views over main the SELECT reads are written in as
        // CTEs, as for a main statement (`inline_temp_views`).
        let routed = match self.route(f.feed.as_slice(), arena)? {
            Route::MainAs(text) => Some(text),
            _ => None,
        };
        let feed = routed.as_ref().unwrap_or(&f.feed);
        let ast = crate::sql::parse::<S::Error>(feed.as_slice(), arena, self.cfg.limits())?;
        // Its views and CTEs are expanded as a main statement's are
        // (v0.16 review fix: a view read there was "no such table").
        let expanded = self.expand_ast(arena, &ast)?;
        let Ast::Select(sel) = expanded.as_ref().unwrap_or(&ast) else {
            return Err(Error::Unsupported);
        };
        // Parameters would need binding on both sides.
        #[cfg(feature = "params")]
        if crate::sql::parse_params::<S::Error>(f.feed.as_slice(), arena, self.cfg.limits())?
            .1
            .count
            > 0
        {
            return Err(Error::Unsupported);
        }
        let mut width = f.width;
        let (select, create, insert) = match &f.ctas {
            Some(name) => {
                if self.temp_has(name.as_slice(), false) {
                    return Err(Error::Schema {
                        kind: SchemaKind::DuplicateTable,
                    });
                }
                let super::Plan::CreateTableAs {
                    select: Some(select),
                    sql,
                    affs,
                } = self.plan_create_as(arena, name.as_slice(), sel)?
                else {
                    return Err(Error::Unsupported);
                };
                let mut insert = copy_vec(b"INSERT INTO \"");
                for &b in name.iter() {
                    insert.push(b);
                    if b == b'"' {
                        insert.push(b'"');
                    }
                }
                insert.extend_from_slice(b"\" VALUES (");
                for i in 0..affs.len() {
                    if i > 0 {
                        insert.push(b',');
                    }
                    insert.extend_from_slice(b"NULL");
                }
                insert.push(b')');
                width = affs.len();
                (select, Some(sql), insert)
            }
            None => {
                let select = self.plan_subquery(sel, arena, &[])?;
                (Box::new(select), None, f.insert.unwrap_or_default())
            }
        };
        let c = self.temp_conn().await?;
        // The temp half's own prepare errors are the statement's (a
        // CTAS's INSERT names a table that does not exist yet).
        if create.is_none() {
            let Ok(t) = core::str::from_utf8(insert.as_slice()) else {
                return Err(Error::Unsupported);
            };
            let prep: Boxed<'_, Result<Statement<'_, MemStorage>, MemError>> =
                Box::pin(c.prepare(t));
            prep.await.map_err(mem_err)?;
        }
        // Each SELECT row is one VALUES row of the INSERT's width.
        if select.width() != width {
            return Err(Error::Schema {
                kind: SchemaKind::ColumnCountMismatch,
            });
        }
        Ok(super::Plan::Temp {
            sql: insert,
            width: 0,
            feed: Some(Box::new(TempFeed { select, create })),
        })
    }
}
