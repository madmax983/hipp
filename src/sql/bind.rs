//! Parameter binding (v0.16, feature `params`): a prepared
//! statement's tree with every `Expr::Param` replaced by its bound
//! value, written as the literal of the same storage class.
//!
//! The pin gives a bound value no affinity and no collation, as it
//! gives a literal, so everything downstream of the parser — the
//! planner's constant folding, affinity, comparison, the tiny
//! profile's literal-only INSERT and `WHERE rowid = ?` paths —
//! treats the substituted literal exactly as the pin treats the
//! parameter. The one place the pin tells them apart is an ORDER
//! BY / GROUP BY term: an integer *literal* there is a result-column
//! ordinal, a parameter is a constant expression (or, in a compound
//! ORDER BY, a match for a result item that is the same parameter).
//! `bind_ast` encodes that: such a term becomes `+literal` (a
//! constant that is not an ordinal), or the matched item's ordinal.

use alloc::boxed::Box;

use super::ast::{Ast, Expr, Literal, SelectItem, SelectStmt, UpsertAction};
use crate::arena::{ABytes, AVec, Arena};
use crate::error::Result;
use crate::record::OwnedValue;

/// The bound tree of `src` (see the module docs). `values[i]` is
/// parameter `i + 1`'s value; a missing entry is NULL. Bound text
/// and blob bytes are copied into `arena`.
pub fn bind_ast<E>(src: &Ast, values: &[OwnedValue], arena: &Arena) -> Result<Ast, E> {
    let mut ast = src.clone();
    let b = Binder { values, arena };
    b.ast::<E>(&mut ast)?;
    Ok(ast)
}

impl Binder<'_> {
    fn ast<E>(&self, ast: &mut Ast) -> Result<(), E> {
        let b = self;
        match ast {
            Ast::Select(s) => b.select::<E>(s)?,
            Ast::Insert {
                rows,
                select,
                upsert,
                returning,
                ..
            } => {
                for row in rows.as_mut_slice() {
                    for e in row.as_mut_slice() {
                        b.expr::<E>(e)?;
                    }
                }
                if let Some(s) = select {
                    b.select::<E>(s)?;
                }
                if let Some(u) = upsert {
                    if let Some(f) = &mut u.target_filter {
                        b.expr::<E>(f)?;
                    }
                    if let UpsertAction::Update {
                        assignments,
                        filter,
                    } = &mut u.action
                    {
                        for (_, e) in assignments.as_mut_slice() {
                            b.expr::<E>(e)?;
                        }
                        if let Some(f) = filter {
                            b.expr::<E>(f)?;
                        }
                    }
                }
                b.items::<E>(returning.as_mut_slice())?;
            }
            Ast::Update {
                assignments,
                from,
                filter,
                returning,
                ..
            } => {
                for (_, e) in assignments.as_mut_slice() {
                    b.expr::<E>(e)?;
                }
                for f in from.as_mut_slice() {
                    if let Some(on) = &mut f.on {
                        b.expr::<E>(on)?;
                    }
                    if let Some(d) = &mut f.derived {
                        b.select::<E>(d)?;
                    }
                }
                if let Some(f) = filter {
                    b.expr::<E>(f)?;
                }
                b.items::<E>(returning.as_mut_slice())?;
            }
            Ast::Delete {
                filter, returning, ..
            } => {
                if let Some(f) = filter {
                    b.expr::<E>(f)?;
                }
                b.items::<E>(returning.as_mut_slice())?;
            }
            #[cfg(feature = "v16")]
            Ast::CreateTable {
                as_select: Some(s), ..
            } => b.select::<E>(s)?,
            // `WITH ... INSERT / UPDATE / DELETE` (v0.16): the CTE
            // bodies, then the statement.
            #[cfg(feature = "v16")]
            Ast::With { with, body } => {
                b.with::<E>(with)?;
                b.ast::<E>(body)?;
            }
            // Every other statement holds no parameters: the parser
            // refuses them in stored definitions (CREATE TABLE / VIEW,
            // ALTER TABLE) and the rest have no expressions.
            _ => {}
        }
        Ok(())
    }

    /// A WITH clause's CTE bodies (v0.16, `v16`).
    #[cfg(feature = "v16")]
    fn with<E>(&self, w: &mut super::ast::With) -> Result<(), E> {
        for c in w.ctes.as_mut_slice() {
            self.select::<E>(&mut c.select)?;
        }
        Ok(())
    }
}

struct Binder<'v> {
    values: &'v [OwnedValue],
    arena: &'v Arena,
}

impl Binder<'_> {
    /// Parameter `idx`'s value as a literal.
    fn literal<E>(&self, idx: u16) -> Result<Literal, E> {
        let v = usize::from(idx)
            .checked_sub(1)
            .and_then(|i| self.values.get(i));
        Ok(match v {
            None | Some(OwnedValue::Null) => Literal::Null,
            Some(OwnedValue::Integer(i)) => Literal::Integer(*i),
            Some(OwnedValue::Real(f)) => Literal::Real(*f),
            Some(OwnedValue::Text(t)) => Literal::Text(self.arena.alloc_copy_handle(t.as_slice())?),
            Some(OwnedValue::Blob(t)) => Literal::Blob(self.arena.alloc_copy_handle(t.as_slice())?),
        })
    }

    fn expr<E>(&self, e: &mut Expr) -> Result<(), E> {
        match e {
            Expr::Param(idx, _) => {
                *e = Expr::Literal(self.literal::<E>(*idx)?);
            }
            Expr::Literal(_) | Expr::Column { .. } => {}
            #[cfg(feature = "v04")]
            Expr::HexTooBig(_) => {}
            Expr::Binary(_, a, c) | Expr::Like(a, c, _) => {
                self.expr::<E>(a)?;
                self.expr::<E>(c)?;
            }
            Expr::Neg(a) | Expr::Pos(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => {
                self.expr::<E>(a)?
            }
            #[cfg(feature = "v16")]
            Expr::Collate(a, _) | Expr::BitNot(a) | Expr::RowItem(a, ..) => self.expr::<E>(a)?,
            #[cfg(feature = "v16")]
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
            Expr::Func {
                args,
                #[cfg(feature = "v16")]
                clause,
                ..
            } => {
                for x in args.as_mut_slice() {
                    self.expr::<E>(x)?;
                }
                #[cfg(feature = "v16")]
                if let Some(c) = clause {
                    for (x, _) in c.order_by.as_mut_slice() {
                        self.expr::<E>(x)?;
                    }
                    if let Some(x) = &mut c.filter {
                        self.expr::<E>(x)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn items<E>(&self, items: &mut [SelectItem]) -> Result<(), E> {
        for it in items {
            if let SelectItem::Expr(e, _, _) = it {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }

    /// One ORDER BY / GROUP BY term: a bare parameter (under any
    /// COLLATE) becomes `ordinal` when given, else `+value` — a
    /// constant the term rules never read as an ordinal.
    fn term<E>(&self, e: &mut Expr, ordinal: Option<i64>) -> Result<(), E> {
        #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
        let mut core = &mut *e;
        #[cfg(feature = "v16")]
        while let Expr::Collate(inner, _) = core {
            core = inner;
        }
        if let Expr::Param(idx, _) = core {
            *core = match ordinal {
                Some(k) => Expr::Literal(Literal::Integer(k)),
                None => Expr::Pos(Box::new(Expr::Literal(self.literal::<E>(*idx)?))),
            };
            return Ok(());
        }
        self.expr::<E>(e)
    }

    fn select<E>(&self, s: &mut SelectStmt) -> Result<(), E> {
        #[cfg(feature = "v16")]
        if let Some(w) = &mut s.with {
            self.with::<E>(w)?;
        }
        // Compound ORDER BY terms that are a bare parameter match
        // the first item, scanning the cores from the leftmost,
        // that is the same parameter (`compound_match`). Found
        // before the items are bound.
        let mut ordinals: AVec<Option<i64>> = AVec::new();
        if !s.compound.is_empty() {
            for (t, _) in s.order_by.iter() {
                ordinals.push(compound_match(s, t, self.arena));
            }
        }
        self.core::<E>(s)?;
        for (_, c) in s.compound.as_mut_slice() {
            self.core::<E>(c)?;
        }
        for (k, (t, _)) in s.order_by.as_mut_slice().iter_mut().enumerate() {
            self.term::<E>(t, ordinals.as_slice().get(k).copied().flatten())?;
        }
        if let Some(x) = &mut s.limit {
            self.expr::<E>(x)?;
        }
        if let Some(x) = &mut s.offset {
            self.expr::<E>(x)?;
        }
        Ok(())
    }

    /// One core's own clauses (not the statement's ORDER BY /
    /// LIMIT, which `select` binds).
    fn core<E>(&self, s: &mut SelectStmt) -> Result<(), E> {
        self.items::<E>(s.items.as_mut_slice())?;
        for f in s.from.as_mut_slice() {
            if let Some(on) = &mut f.on {
                self.expr::<E>(on)?;
            }
            if let Some(d) = &mut f.derived {
                self.select::<E>(d)?;
            }
        }
        if let Some(f) = &mut s.filter {
            self.expr::<E>(f)?;
        }
        for g in s.group_by.as_mut_slice() {
            self.term::<E>(g, None)?;
        }
        if let Some(h) = &mut s.having {
            self.expr::<E>(h)?;
        }
        for row in s.values.as_mut_slice() {
            for e in row.as_mut_slice() {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }
}

/// The parameter a term is (under any COLLATE), if it is one.
fn bare_param(e: &Expr) -> Option<(u16, ABytes)> {
    #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
    let mut core = e;
    #[cfg(feature = "v16")]
    while let Expr::Collate(inner, _) = core {
        core = inner;
    }
    match core {
        Expr::Param(i, h) => Some((*i, *h)),
        _ => None,
    }
}

/// The 1-based result column a compound ORDER BY term matches by
/// being the same parameter as one core's item (the pin's
/// `resolveCompoundOrderBy`: it walks back to the leftmost core and
/// resolves the terms moving rightwards, so the leftmost core that
/// holds the parameter decides; `sqlite3ExprCompare` asks for the
/// same index and the same spelling).
///
/// A multi-row VALUES core is laid out as the pin's
/// `sqlite3MultiValues` builds it: rows that may share a co-routine
/// become one `SELECT * FROM (co-routine)` core, whose items are
/// `column1..N` and never match a parameter; a row that may not
/// (see `starts_coroutine`) stays a core of its own. A multi-row
/// VALUES right of a compound operator is wrapped as a subquery by
/// the pin's grammar, so none of its rows match.
fn compound_match(s: &SelectStmt, term: &Expr, arena: &Arena) -> Option<i64> {
    let (idx, h) = bare_param(term)?;
    let text = crate::sql::handle_bytes(arena, h);
    let same = |e: &Expr| match e {
        Expr::Param(j, h2) => *j == idx && crate::sql::handle_bytes(arena, *h2) == text,
        _ => false,
    };
    let pos = |row: &[Expr]| -> Option<i64> {
        let k = row.iter().position(&same)?;
        i64::try_from(k).ok().map(|k| k + 1)
    };
    let in_core = |c: &SelectStmt, leftmost: bool| -> Option<i64> {
        let rows = c.values.as_slice();
        match rows {
            [] => {
                let k = c.items.iter().position(|it| match it {
                    SelectItem::Expr(e, _, _) => same(e),
                    _ => false,
                })?;
                i64::try_from(k).ok().map(|k| k + 1)
            }
            [row] => pos(row.as_slice()),
            _ if !leftmost => None,
            [first, rest @ ..] => {
                // The last core so far, when it is a plain row
                // (None: the co-routine).
                let mut plain: Option<&[Expr]> = Some(first.as_slice());
                for r in rest {
                    let joins = constant_row(r.as_slice()) && plain.is_none_or(starts_coroutine);
                    if joins {
                        plain = None;
                    } else {
                        if let Some(k) = plain.and_then(pos) {
                            return Some(k);
                        }
                        plain = Some(r.as_slice());
                    }
                }
                plain.and_then(pos)
            }
        }
    };
    in_core(s, true).or_else(|| s.compound.iter().find_map(|(_, c)| in_core(c, false)))
}

/// Whether a VALUES row is constant for the pin's
/// `sqlite3MultiValues` (oracle-probed): no subquery anywhere in
/// it. Every built-in function hipp has (LIKE and GLOB too) is
/// constant there.
fn constant_row(row: &[Expr]) -> bool {
    row.iter().all(constant)
}

fn constant(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::ExistsSubquery(_) | Expr::InSubquery(..) => false,
        Expr::Literal(_) | Expr::Column { .. } | Expr::Param(..) => true,
        #[cfg(feature = "v04")]
        Expr::HexTooBig(_) => true,
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) => constant(a) && constant(b),
        Expr::Neg(a) | Expr::Pos(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => {
            constant(a)
        }
        #[cfg(feature = "v16")]
        Expr::Collate(a, _) | Expr::BitNot(a) | Expr::RowItem(a, ..) => constant(a),
        #[cfg(feature = "v16")]
        Expr::Vector(l) => l.iter().all(constant),
        Expr::InList(a, l, _) => constant(a) && l.iter().all(constant),
        Expr::Between(a, b, c, _) => constant(a) && constant(b) && constant(c),
        Expr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(constant)
                && whens.iter().all(|(w, t)| constant(w) && constant(t))
                && otherwise.as_deref().is_none_or(constant)
        }
        // An aggregate clause makes no call constant (it only
        // goes with aggregates, which are not).
        #[cfg(feature = "v16")]
        Expr::Func {
            clause: Some(_), ..
        } => false,
        Expr::Func { args, .. } => args.iter().all(constant),
    }
}

/// Whether a plain VALUES row may become the first row of a
/// co-routine when a constant row follows it (the pin's
/// `sqlite3MultiValues`, oracle-probed): it is constant and none
/// of its items has an affinity — a CAST, under any COLLATE, has
/// one; `+CAST(..)`, `CAST(..) + 0` and a COLLATE'd literal do not.
fn starts_coroutine(row: &[Expr]) -> bool {
    constant_row(row)
        && row.iter().all(|e| {
            #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
            let mut core = e;
            #[cfg(feature = "v16")]
            while let Expr::Collate(inner, _) = core {
                core = inner;
            }
            !matches!(core, Expr::Cast(..))
        })
}
