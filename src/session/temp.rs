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
//!   there, with `TEMP` and every `temp.` qualifier removed from its
//!   text (the temp database is the private connection's main);
//!   one that names both databases is refused (`Unsupported`): the
//!   two never meet inside one statement in hipp.
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
enum Kind {
    /// A table or view read or written.
    Object,
    /// An index named by DROP INDEX.
    Index,
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
        if let SelectItem::Expr(e, _, _) = it {
            expr_names(e, out, ctes);
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
            }
        }
        _ => {}
    }
}

fn returning_names(items: &[SelectItem], out: &mut AVec<(ABytes, Kind)>, ctes: &mut AVec<ABytes>) {
    for it in items {
        if let SelectItem::Expr(e, _, _) = it {
            expr_names(e, out, ctes);
        }
    }
}

/// The names a statement refers to, the name it creates (with
/// whether it is an index), and its CTE names.
fn ast_names(
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
fn tok_index(toks: &[Token], h: ABytes) -> Option<usize> {
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
fn qual_cut(toks: &[Token], i: usize) -> Option<(usize, usize)> {
    Some((toks.get(i.checked_sub(2)?)?.offset, toks.get(i)?.offset))
}

fn is_temp_schema_name(n: &[u8]) -> bool {
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
        let parsed = crate::sql::parser::parse_with_tokens::<S::Error>(text.as_slice(), arena);
        let (ast, toks) = match parsed {
            Ok(a) => a,
            // The main database's parse reports the error (or the
            // TEMP keyword is not followed by TABLE / VIEW).
            Err(e) if temp_kw => return Err(e),
            Err(_) => return Ok(Route::Main),
        };
        let t = toks.as_slice();
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
        if let Ast::Pragma { schema, value, .. } = &ast {
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
        let mut quals = 0usize;
        for (h, kind) in names.iter() {
            let n = handle_bytes(arena, *h);
            let i = tok_index(t, *h);
            let q = i.and_then(|i| qualifier(t, i, arena));
            if q.is_none() && ctes.iter().any(|c| eq_ci(handle_bytes(arena, *c), n)) {
                continue;
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
                quals += 1;
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
        if let Some((h, is_index)) = created {
            let q = tok_index(t, h).and_then(|i| qualifier(t, i, arena));
            // `CREATE INDEX main.i ON t` looks `t` up in main only.
            if is_index && q == Some(false) {
                return Ok(Route::Main);
            }
            if q == Some(true) {
                if let Some(c) = tok_index(t, h).and_then(|i| qual_cut(t, i)) {
                    cuts.push(c);
                    quals += 1;
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
        // Every `temp.` the statement holds must be one of these.
        let written = (0..t.len())
            .filter(|&i| qualifier(t, i, arena) == Some(true))
            .count();
        let to_temp = create_temp || (temp > 0 && main == 0 && created.is_none());
        if !to_temp {
            if temp > 0 || written > 0 {
                return Err(Error::Unsupported);
            }
            return Ok(Route::Main);
        }
        if main > 0 || written != quals {
            return Err(Error::Unsupported);
        }
        // The text the temp database runs: no `temp.`, and its
        // schema table under its own name.
        for x in t.iter() {
            if let Tok::Ident(h, false) = x.tok
                && is_temp_schema_name(handle_bytes(arena, h))
            {
                cuts.push((x.offset, x.offset));
            }
        }
        cuts.as_mut_slice().sort_by_key(|c| c.0);
        let src = text.as_slice();
        let mut out = AVec::new();
        let mut pos = 0usize;
        for &(a, b) in cuts.iter() {
            if a < pos || b > src.len() {
                continue;
            }
            out.extend_from_slice(src.get(pos..a).unwrap_or_default());
            if a == b {
                // `sqlite_temp_schema` / `sqlite_temp_master`.
                out.extend_from_slice(b"sqlite_schema");
                let mut e = a;
                while e < src.len() && (src[e].is_ascii_alphanumeric() || src[e] == b'_') {
                    e += 1;
                }
                pos = e;
            } else {
                pos = b;
            }
        }
        out.extend_from_slice(&src[pos.min(src.len())..]);
        Ok(Route::Temp(out))
    }

    /// The main database may have an object (`index`: an index)
    /// named so: one in its catalog, or a name the planner resolves
    /// itself (`sqlite_*` tables, `pragma_*` functions).
    fn main_has(&self, name: &[u8], index: bool) -> bool {
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

    /// The temp database has an object (`index`: an index) named so.
    fn temp_has(&self, name: &[u8], index: bool) -> bool {
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
    /// transaction and savepoints) on first use.
    pub(super) async fn temp_conn(&mut self) -> Result<&mut Connection<MemStorage>, S::Error> {
        if self.temp.is_none() {
            let mut cfg = self.cfg.clone();
            cfg.vacuum_into = None;
            let open: Boxed<'_, Result<Connection<MemStorage>, MemError>> =
                Box::pin(Connection::open(MemStorage::new(), cfg));
            let mut c = open.await.map_err(mem_err)?;
            c.is_temp = true;
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
        match self.temp.as_deref_mut() {
            Some(c) => Ok(c),
            None => Err(Error::Unsupported),
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
async fn run_temp(c: &mut Connection<MemStorage>, sql: &[u8]) -> Result<(), MemError> {
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
            let Plan::Temp { sql, .. } = &self.plan else {
                return Err(Error::Unsupported);
            };
            let sql = sql.clone();
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
                let prep: Boxed<'_, Result<Statement<'_, MemStorage>, MemError>> =
                    Box::pin(c.prepare(text));
                let mut st = prep.await?;
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
            env.last_rowid.set(l2);
            env.changes.set(c2);
            env.total_changes
                .set(total.saturating_add(t2.saturating_sub(before)));
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
