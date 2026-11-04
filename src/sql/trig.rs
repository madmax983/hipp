//! A trigger program's `NEW.` / `OLD.` references (v0.16, feature
//! `triggers`): each statement of a firing trigger is parsed again
//! and every `NEW.col` / `OLD.col` in it replaced by the firing row's
//! value (`Expr::TrigValue`) before it is planned.
//!
//! The pin resolves `new` / `old` as the outermost scope of the
//! statement: a FROM source (or UPDATE / DELETE target) of that name
//! wins, in the select it belongs to and in that select's
//! subqueries — so such a reference is left alone. A reference to a
//! column the table does not have, or to `OLD` in an INSERT trigger
//! (`NEW` in a DELETE one), is the pin's "no such column: new.x". A
//! column carries no comparison affinity and the column's collating
//! sequence; the rowid (and the INTEGER PRIMARY KEY column) carries
//! INTEGER affinity and no collation — as the pin's `TK_TRIGGER`
//! expressions do (probed: `new.a = '5'` over an INT column holding
//! 5 is false, `new.rowid = '1'` true, `'B' = new.b` over a NOCASE
//! column holding 'b' true).

use super::ast::{Ast, Expr, Literal, SelectItem, SelectStmt, UpsertAction};
use crate::arena::{ABytes, AVec, Arena};
use crate::error::{Error, Result, SchemaKind};
use crate::record::{Affinity, Coll, OwnedValue};
use crate::sql::{eq_ci, handle_bytes};

/// One row a trigger fires for: its column values (table order)
/// and rowid.
#[derive(Clone, Copy)]
pub struct RowRef<'x> {
    pub vals: &'x [OwnedValue],
    pub rowid: i64,
}

/// What `NEW.` / `OLD.` mean for one firing.
pub struct Binding<'x> {
    /// The table's (or view's) column names, in order.
    pub names: &'x [AVec<u8>],
    /// Each column's collating sequence (none for a view's).
    pub colls: &'x [Option<Coll>],
    /// The INTEGER PRIMARY KEY column, which reads as the rowid.
    pub alias: Option<usize>,
    /// The trigger is on a view (no rowid).
    pub view: bool,
    pub new: Option<RowRef<'x>>,
    pub old: Option<RowRef<'x>>,
}

/// Replace every `NEW.` / `OLD.` reference of `ast` (see the module
/// docs). Text and blob values are copied into `arena`.
pub fn bind_trigger<E>(ast: &mut Ast, b: &Binding<'_>, arena: &Arena) -> Result<(), E> {
    let mut w = Walker {
        b,
        arena,
        scope: AVec::new(),
        found: None,
    };
    w.ast::<E>(ast)
}

/// The `NEW.` / `OLD.` references of `ast` (qualifier and column
/// name tokens), as `bind_trigger` would bind them: what ALTER TABLE
/// RENAME COLUMN edits in a trigger on the renamed table.
pub fn trigger_refs<E>(ast: &Ast, arena: &Arena) -> Result<AVec<(ABytes, ABytes)>, E> {
    let names: [AVec<u8>; 0] = [];
    let b = Binding {
        names: &names,
        colls: &[],
        alias: None,
        view: false,
        new: None,
        old: None,
    };
    let mut copy = ast.clone();
    let mut w = Walker {
        b: &b,
        arena,
        scope: AVec::new(),
        found: Some(AVec::new()),
    };
    w.ast::<E>(&mut copy)?;
    Ok(w.found.unwrap_or_default())
}

struct Walker<'x> {
    b: &'x Binding<'x>,
    arena: &'x Arena,
    /// The source names in scope (shadowing `new` / `old`).
    scope: AVec<ABytes>,
    /// Collecting the references instead of binding them.
    found: Option<AVec<(ABytes, ABytes)>>,
}

fn unknown<E>() -> Error<E> {
    Error::Schema {
        kind: SchemaKind::UnknownColumn,
    }
}

impl Walker<'_> {
    fn shadowed(&self, q: &[u8]) -> bool {
        self.scope
            .iter()
            .any(|h| eq_ci(handle_bytes(self.arena, *h), q))
    }

    fn literal<E>(&self, v: &OwnedValue) -> Result<Literal, E> {
        Ok(match v {
            OwnedValue::Null => Literal::Null,
            OwnedValue::Integer(i) => Literal::Integer(*i),
            OwnedValue::Real(f) => Literal::Real(*f),
            OwnedValue::Text(t) => Literal::Text(self.arena.alloc_copy_handle(t.as_slice())?),
            OwnedValue::Blob(t) => Literal::Blob(self.arena.alloc_copy_handle(t.as_slice())?),
        })
    }

    /// The value `q.name` stands for, when `q` is `new` / `old` and
    /// no source shadows it; `Ok(None)` for any other reference.
    fn value<E>(&self, q: &[u8], name: &[u8]) -> Result<Option<Expr>, E> {
        let row = if eq_ci(q, b"new") {
            self.b.new
        } else if eq_ci(q, b"old") {
            self.b.old
        } else {
            return Ok(None);
        };
        if self.shadowed(q) {
            return Ok(None);
        }
        let Some(row) = row else {
            return Err(unknown());
        };
        let col = self.b.names.iter().position(|n| eq_ci(n.as_slice(), name));
        let rowid = Expr::TrigValue(Literal::Integer(row.rowid), Some(Affinity::Integer), None);
        match col {
            Some(i) if Some(i) == self.b.alias => Ok(Some(rowid)),
            Some(i) => {
                let v = row.vals.get(i).unwrap_or(&OwnedValue::Null);
                let coll = self.b.colls.get(i).copied().flatten();
                Ok(Some(Expr::TrigValue(self.literal::<E>(v)?, None, coll)))
            }
            None if !self.b.view && crate::query::is_rowid_name(name) => Ok(Some(rowid)),
            None => Err(unknown()),
        }
    }

    fn ast<E>(&mut self, ast: &mut Ast) -> Result<(), E> {
        match ast {
            Ast::Select(s) => self.select::<E>(s),
            Ast::Insert {
                table,
                rows,
                select,
                upsert,
                ..
            } => {
                for row in rows.as_mut_slice() {
                    for e in row.as_mut_slice() {
                        self.expr::<E>(e)?;
                    }
                }
                if let Some(s) = select {
                    self.select::<E>(s)?;
                }
                if let Some(u) = upsert {
                    let n = self.scope.len();
                    self.scope.push(*table);
                    if let Some(f) = &mut u.target_filter {
                        self.expr::<E>(f)?;
                    }
                    if let UpsertAction::Update {
                        assignments,
                        filter,
                    } = &mut u.action
                    {
                        for (_, e) in assignments.as_mut_slice() {
                            self.expr::<E>(e)?;
                        }
                        if let Some(f) = filter {
                            self.expr::<E>(f)?;
                        }
                    }
                    self.scope.truncate(n);
                }
                Ok(())
            }
            Ast::Update {
                table,
                alias,
                assignments,
                from,
                filter,
                ..
            } => {
                let n = self.scope.len();
                self.scope.push(alias.unwrap_or(*table));
                self.scope_from::<E>(from.as_mut_slice())?;
                for f in from.as_mut_slice() {
                    if let Some(on) = &mut f.on {
                        self.expr::<E>(on)?;
                    }
                }
                let r = (|| {
                    for (_, e) in assignments.as_mut_slice() {
                        self.expr::<E>(e)?;
                    }
                    if let Some(f) = filter {
                        self.expr::<E>(f)?;
                    }
                    Ok(())
                })();
                self.scope.truncate(n);
                // A derived FROM source sees only the outer scopes.
                for f in from.as_mut_slice() {
                    if let Some(d) = &mut f.derived {
                        self.select::<E>(d)?;
                    }
                }
                r
            }
            Ast::Delete { table, filter, .. } => {
                let n = self.scope.len();
                self.scope.push(*table);
                let r = match filter {
                    Some(f) => self.expr::<E>(f),
                    None => Ok(()),
                };
                self.scope.truncate(n);
                r
            }
            Ast::With { with, body } => {
                for c in with.ctes.as_mut_slice() {
                    self.select::<E>(&mut c.select)?;
                }
                self.ast::<E>(body)
            }
            _ => Ok(()),
        }
    }

    fn expr<E>(&mut self, e: &mut Expr) -> Result<(), E> {
        match e {
            Expr::Column {
                table: Some(q),
                name,
                ..
            } => {
                let qb = handle_bytes(self.arena, *q);
                let nb = handle_bytes(self.arena, *name);
                if let Some(f) = &mut self.found {
                    if (eq_ci(qb, b"new") || eq_ci(qb, b"old"))
                        && !self
                            .scope
                            .iter()
                            .any(|h| eq_ci(handle_bytes(self.arena, *h), qb))
                    {
                        f.push((*q, *name));
                    }
                } else if let Some(v) = self.value::<E>(qb, nb)? {
                    *e = v;
                }
            }
            Expr::Literal(_) | Expr::Column { .. } | Expr::HexTooBig(_) | Expr::TrigValue(..) => {}
            #[cfg(feature = "params")]
            Expr::Param(..) => {}
            Expr::Raise(_, m, _) => {
                if let Some(m) = m {
                    self.expr::<E>(m)?;
                }
            }
            Expr::Binary(_, a, c) | Expr::Like(a, c, _) => {
                self.expr::<E>(a)?;
                self.expr::<E>(c)?;
            }
            Expr::Neg(a)
            | Expr::Pos(a)
            | Expr::Not(a)
            | Expr::IsNull(a, _)
            | Expr::Cast(a, _)
            | Expr::Collate(a, _)
            | Expr::BitNot(a)
            | Expr::RowItem(a, ..) => self.expr::<E>(a)?,
            Expr::Vector(l) => {
                for x in l.as_mut_slice() {
                    self.expr::<E>(x)?;
                }
            }
            Expr::InList(a, l, _) => {
                self.expr::<E>(a)?;
                for x in l.as_mut_slice() {
                    self.expr::<E>(x)?;
                }
            }
            Expr::InSubquery(a, s, _) => {
                self.expr::<E>(a)?;
                self.select::<E>(s)?;
            }
            Expr::ScalarSubquery(s) | Expr::ExistsSubquery(s) => self.select::<E>(s)?,
            Expr::Between(a, c, d, _) => {
                self.expr::<E>(a)?;
                self.expr::<E>(c)?;
                self.expr::<E>(d)?;
            }
            Expr::Case {
                base,
                whens,
                otherwise,
            } => {
                if let Some(x) = base {
                    self.expr::<E>(x)?;
                }
                for (w, t) in whens.as_mut_slice() {
                    self.expr::<E>(w)?;
                    self.expr::<E>(t)?;
                }
                if let Some(x) = otherwise {
                    self.expr::<E>(x)?;
                }
            }
            Expr::Func { args, clause, .. } => {
                for x in args.as_mut_slice() {
                    self.expr::<E>(x)?;
                }
                if let Some(c) = clause {
                    for (x, _) in c.order_by.as_mut_slice() {
                        self.expr::<E>(x)?;
                    }
                    if let Some(x) = &mut c.filter {
                        self.expr::<E>(x)?;
                    }
                    #[cfg(feature = "window")]
                    if let Some(super::ast::Over::Spec(w)) = &mut c.over {
                        self.window::<E>(w)?;
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "window")]
    fn window<E>(&mut self, w: &mut super::ast::WindowDef) -> Result<(), E> {
        let mut r = Ok(());
        w.exprs_mut(&mut |x| {
            if r.is_ok() {
                r = self.expr::<E>(x);
            }
        });
        r
    }

    fn select<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        if let Some(w) = &mut s.with {
            for c in w.ctes.as_mut_slice() {
                self.select::<E>(&mut c.select)?;
            }
        }
        // The statement's ORDER BY / LIMIT see the first core's
        // sources.
        let n = self.scope.len();
        self.core::<E>(s)?;
        for (_, c) in s.compound.as_mut_slice() {
            self.core::<E>(c)?;
        }
        for f in s.from.iter() {
            self.scope.push(f.alias.unwrap_or(f.table));
        }
        let r = (|| {
            for (t, _) in s.order_by.as_mut_slice() {
                self.expr::<E>(t)?;
            }
            if let Some(x) = &mut s.limit {
                self.expr::<E>(x)?;
            }
            if let Some(x) = &mut s.offset {
                self.expr::<E>(x)?;
            }
            Ok(())
        })();
        self.scope.truncate(n);
        r
    }

    /// One core's own clauses, with its FROM sources in scope.
    fn core<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        // Derived tables and their ON clauses first: a derived
        // table sees only the enclosing scopes.
        for f in s.from.as_mut_slice() {
            if let Some(d) = &mut f.derived {
                self.select::<E>(d)?;
            }
        }
        let n = self.scope.len();
        let r = self.scope_from::<E>(s.from.as_mut_slice());
        let r = r.and_then(|()| self.core_exprs::<E>(s));
        self.scope.truncate(n);
        r
    }

    /// Bring FROM sources `from` into scope in order, binding each
    /// table-valued function's arguments (`json_each(new.a)`; v0.16
    /// review fix: they were left unbound) with only the sources
    /// before it in scope, as they resolve.
    fn scope_from<E>(&mut self, from: &mut [super::ast::FromSource]) -> Result<(), E> {
        for f in from {
            #[cfg(feature = "v16")]
            for a in f.args.iter_mut().flat_map(|a| a.as_mut_slice().iter_mut()) {
                self.expr::<E>(a)?;
            }
            self.scope.push(f.alias.unwrap_or(f.table));
        }
        Ok(())
    }

    fn core_exprs<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        for it in s.items.as_mut_slice() {
            if let SelectItem::Expr(e, _, _) = it {
                self.expr::<E>(e)?;
            }
        }
        for f in s.from.as_mut_slice() {
            if let Some(on) = &mut f.on {
                self.expr::<E>(on)?;
            }
        }
        if let Some(f) = &mut s.filter {
            self.expr::<E>(f)?;
        }
        for g in s.group_by.as_mut_slice() {
            self.expr::<E>(g)?;
        }
        if let Some(h) = &mut s.having {
            self.expr::<E>(h)?;
        }
        #[cfg(feature = "window")]
        for (_, w) in s.windows.as_mut_slice() {
            self.window::<E>(w)?;
        }
        for row in s.values.as_mut_slice() {
            for e in row.as_mut_slice() {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }
}
