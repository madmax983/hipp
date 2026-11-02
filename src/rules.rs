//! Table-definition rules (v0.16, the SQLite-parity push): CHECK
//! constraints, generated columns, STRICT typing, expression
//! DEFAULTs and the wall-clock keywords — everything a table
//! definition asks of a write beyond affinity, NOT NULL and
//! uniqueness. Built only with the v0.4 expression evaluator;
//! without it the parser refuses such tables
//! (`ParseKind::RequiresEvaluator`), so the tiny profile never
//! ignores one.
//!
//! The catalog keeps each rule as SQL text (the inside of a
//! `CHECK (...)` / `AS (...)`, a DEFAULT expression); a write's
//! planner re-parses and resolves it against the table's own
//! single-source scope into a `TableRules`, which the executor
//! applies to every proposed row in the pin's order (probed on
//! the oracle before implementation):
//!
//! 1. NOT NULL on the ordinary columns (OR REPLACE substitutes
//!    defaults here);
//! 2. generated columns, computed in dependency order from the
//!    row as resolved so far (the rowid is already known — an
//!    `INTEGER PRIMARY KEY` reads as the new rowid);
//! 3. NOT NULL on the generated columns (no default exists, so
//!    OR REPLACE cannot resolve one);
//! 4. STRICT column types — an error under every conflict action;
//! 5. CHECK constraints — a NULL result passes; OR IGNORE skips
//!    the row, every other action (OR REPLACE included) errors.
//!
//! Uniqueness / rowid conflicts are considered only after all of
//! that, by the caller.
//!
//! Record layout: a VIRTUAL generated column is absent from the
//! stored record — the record holds the other columns in table
//! order — exactly as in the pin's file format
//! (`sqlite3TableColumnToStorage`), so files interoperate both
//! ways. STORED generated columns are stored in place.

use alloc::boxed::Box;

use crate::arena::{AVec, Arena};
use crate::error::{Error, Result, SchemaKind};
use crate::query::{
    Ctx, RExpr, RefRange, RowData, ScopeGroup, SourceDesc, SubPlan, eval, expr_refs_range, has_agg,
    resolve_scoped, truthy,
};
use crate::record::{Affinity, OwnedValue, Value, apply_affinity, encode_record};
use crate::sql::{ConflictAction, Expr, SelectStmt, eq_ci, parse_expr_text};

/// A STRICT table column's declared type (v0.16, `v16`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictType {
    /// `INT` or `INTEGER`: integers only (INTEGER affinity first
    /// converts `'12'`, `1.0`, `'1e2'`).
    Int,
    /// `REAL`: reals (integers convert).
    Real,
    /// `TEXT`: text (numbers convert).
    Text,
    /// `BLOB`: blobs only.
    Blob,
    /// `ANY`: any value, stored unconverted (no affinity).
    Any,
}

impl StrictType {
    /// The STRICT type a declared type names, if any of the six
    /// (case-insensitive, the whole declaration).
    pub fn of(decl: &[u8]) -> Option<StrictType> {
        Some(if eq_ci(decl, b"INT") || eq_ci(decl, b"INTEGER") {
            StrictType::Int
        } else if eq_ci(decl, b"REAL") {
            StrictType::Real
        } else if eq_ci(decl, b"TEXT") {
            StrictType::Text
        } else if eq_ci(decl, b"BLOB") {
            StrictType::Blob
        } else if eq_ci(decl, b"ANY") {
            StrictType::Any
        } else {
            return None;
        })
    }

    /// Whether a value, after the column's affinity conversion,
    /// may be stored in a column of this type. NULL always may
    /// (NOT NULL is its own rule).
    pub fn admits(self, v: &OwnedValue) -> bool {
        matches!(
            (self, v),
            (_, OwnedValue::Null)
                | (StrictType::Any, _)
                | (StrictType::Int, OwnedValue::Integer(_))
                | (
                    StrictType::Real,
                    OwnedValue::Real(_) | OwnedValue::Integer(_)
                )
                | (StrictType::Text, OwnedValue::Text(_))
                | (StrictType::Blob, OwnedValue::Blob(_))
        )
    }
}

/// A table's generated columns, resolved (v0.16).
#[derive(Debug, Clone)]
pub struct GenCols {
    /// Per table column: VIRTUAL generated, i.e. absent from the
    /// stored record.
    pub virt: AVec<bool>,
    /// Every generated column with its expression (resolved
    /// against the table's row: slot = column index, rowid
    /// source 0), in dependency order — a column follows every
    /// generated column it reads.
    pub order: AVec<(usize, RExpr)>,
    /// Column affinities (a computed value takes its column's
    /// affinity, as on the pin).
    pub affs: AVec<Affinity>,
    pub rowid_alias: Option<usize>,
}

impl GenCols {
    /// Whether column `i` is generated (VIRTUAL or STORED).
    pub fn is_gen(&self, i: usize) -> bool {
        self.order.iter().any(|(c, _)| *c == i)
    }

    /// Compute generated columns into a record-form row: all of
    /// them (a write), or only the VIRTUAL ones (a read — STORED
    /// values come from the record).
    pub fn fill<E>(&self, rec: &mut [OwnedValue], rowid: i64, virtual_only: bool) -> Result<(), E> {
        for (ci, e) in self.order.iter() {
            let ci = *ci;
            if virtual_only && !self.virt.as_slice().get(ci).copied().unwrap_or(false) {
                continue;
            }
            let rd = eval_row(self.rowid_alias, self.affs.as_slice(), rec, rowid);
            let v = eval::<E>(e, &Ctx::Row(&rd))?;
            let aff = self
                .affs
                .as_slice()
                .get(ci)
                .copied()
                .unwrap_or(Affinity::Blob);
            if let Some(slot) = rec.get_mut(ci) {
                *slot = apply_affinity(v, aff);
            }
        }
        Ok(())
    }
}

/// The resolved rules of one table (v0.16). `Default` is a table
/// with none — the common case, for which writes take the
/// pre-v0.16 path unchanged.
#[derive(Debug, Clone, Default)]
pub struct TableRules {
    pub gcols: Option<GenCols>,
    /// CHECK constraints, statement order.
    pub checks: AVec<RExpr>,
    /// STRICT column types, one per column; empty when the table
    /// is not STRICT.
    pub strict: AVec<StrictType>,
}

impl TableRules {
    /// No generated column, CHECK or STRICT typing.
    pub fn is_empty(&self) -> bool {
        self.gcols.is_none() && self.checks.is_empty() && self.strict.is_empty()
    }

    /// The VIRTUAL-column mask for record encode / decode.
    pub fn virt(&self) -> Option<&[bool]> {
        self.gcols.as_ref().map(|g| g.virt.as_slice())
    }

    /// Apply the table's rules to one fully-built proposed row
    /// in record form (see the module docs for the order).
    /// `Ok(false)` means OR IGNORE skips the row (a NOT NULL or
    /// CHECK failure); errors are the statement's.
    ///
    /// `changed` is an UPDATE's change set (`None` for an
    /// INSERT): per column, assigned by the statement, plus
    /// whether the rowid is. As on the pin, an UPDATE checks NOT
    /// NULL only on assigned columns (generated ones always) and
    /// evaluates a CHECK only when it reads an assigned column, a
    /// generated column computed from one, or a changed rowid —
    /// probed: rows that violate a CHECK on `b` (written with
    /// `ignore_check_constraints`) update freely while `b` is not
    /// assigned, and fail once `SET b = b` is.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve<E>(
        &self,
        row: &mut [OwnedValue],
        rowid: i64,
        not_null: &[bool],
        defaults: &[OwnedValue],
        affs: &[Affinity],
        rowid_alias: Option<usize>,
        action: ConflictAction,
        changed: Option<(&[bool], bool)>,
    ) -> Result<bool, E> {
        let gcols = self.gcols.as_ref();
        let assigned = |i: usize| changed.is_none_or(|(m, _)| m.get(i).copied().unwrap_or(false));
        let mut nn: AVec<bool> = AVec::new();
        for (i, f) in not_null.iter().enumerate() {
            nn.push(*f && !gcols.is_some_and(|g| g.is_gen(i)) && assigned(i));
        }
        if !crate::session::resolve_not_null::<E>(
            row,
            nn.as_slice(),
            defaults,
            affs,
            rowid_alias,
            action,
        )? {
            return Ok(false);
        }
        if let Some(g) = gcols {
            g.fill::<E>(row, rowid, false)?;
            let mut nn: AVec<bool> = AVec::new();
            for (i, f) in not_null.iter().enumerate() {
                nn.push(*f && g.is_gen(i));
            }
            if !crate::session::resolve_not_null::<E>(
                row,
                nn.as_slice(),
                defaults,
                affs,
                rowid_alias,
                action,
            )? {
                return Ok(false);
            }
        }
        for (i, ty) in self.strict.iter().enumerate() {
            if Some(i) != rowid_alias && !row.get(i).is_none_or(|v| ty.admits(v)) {
                return Err(Error::Schema {
                    kind: SchemaKind::StrictTypeViolation,
                });
            }
        }
        if !self.checks.is_empty() {
            // The effective change set: assigned columns, then
            // generated columns reading any of them (dependency
            // order makes one pass enough).
            let mut eff: AVec<bool> = AVec::new();
            let mut rowid_changed = true;
            if let Some((m, r)) = changed {
                rowid_changed = r;
                eff.extend_from_slice(m);
                if let Some(g) = gcols {
                    for (ci, e) in g.order.iter() {
                        let dep = reads_any(e, eff.as_slice(), rowid_changed);
                        if let Some(slot) = eff.as_mut_slice().get_mut(*ci) {
                            *slot = *slot || dep;
                        }
                    }
                }
            }
            let rd = eval_row(rowid_alias, affs, row, rowid);
            for c in self.checks.iter() {
                if changed.is_some() && !reads_any(c, eff.as_slice(), rowid_changed) {
                    continue;
                }
                if truthy(&eval::<E>(c, &Ctx::Row(&rd))?) == Some(false) {
                    if action == ConflictAction::Ignore {
                        return Ok(false);
                    }
                    return Err(Error::Schema {
                        kind: SchemaKind::CheckViolation,
                    });
                }
            }
        }
        Ok(true)
    }
}

/// Whether a rule expression over a table row reads any column
/// flagged in `cols`, or the rowid when `rowid` is set.
fn reads_any(e: &RExpr, cols: &[bool], rowid: bool) -> bool {
    (rowid
        && expr_refs_range(
            e,
            &RefRange {
                w: (0, 0),
                s: (0, 1),
            },
        ))
        || cols.iter().enumerate().any(|(i, c)| {
            *c && expr_refs_range(
                e,
                &RefRange {
                    w: (i, i.saturating_add(1)),
                    s: (0, 0),
                },
            )
        })
}

/// The evaluation row for one table row in record form: the
/// rowid-alias column reads as the rowid and REAL columns force
/// reals, exactly as the query executor builds its rows (§5.5).
pub fn eval_row(
    rowid_alias: Option<usize>,
    affs: &[Affinity],
    rec: &[OwnedValue],
    rowid: i64,
) -> RowData {
    let mut vals = AVec::new();
    for (i, r) in rec.iter().enumerate() {
        let mut v = if Some(i) == rowid_alias {
            OwnedValue::Integer(rowid)
        } else {
            r.clone()
        };
        if affs.get(i) == Some(&Affinity::Real)
            && let OwnedValue::Integer(x) = v
        {
            v = OwnedValue::Real(x as f64);
        }
        vals.push(v);
    }
    let mut rowids = AVec::new();
    rowids.push(Some(rowid));
    RowData { vals, rowids }
}

/// Decode a stored record into record form: one value per table
/// column, in table order; a missing trailing field (a column
/// added by ALTER TABLE) takes its default; a VIRTUAL column
/// (`virt`) is not in the record and reads as NULL here — the
/// caller computes it (`GenCols::fill`).
pub fn decode_rec<E>(
    payload: &[u8],
    ncols: usize,
    defaults: &[OwnedValue],
    virt: Option<&[bool]>,
) -> Result<AVec<OwnedValue>, E> {
    let mut dec: AVec<Value<'_>> = AVec::new();
    crate::record::decode_record_into::<E>(payload, ncols, &mut dec)?;
    let mut out = AVec::new();
    let mut k = 0usize;
    for i in 0..ncols {
        if virt.is_some_and(|m| m.get(i).copied().unwrap_or(false)) {
            out.push(OwnedValue::Null);
            continue;
        }
        out.push(match dec.as_slice().get(k) {
            Some(v) => OwnedValue::from_value(*v),
            None => defaults.get(i).cloned().unwrap_or(OwnedValue::Null),
        });
        k += 1;
    }
    Ok(out)
}

/// Encode a record-form row as its stored record: the rowid
/// alias column's field is NULL (the rowid carries it) and VIRTUAL
/// columns are left out.
pub fn encode_rec(
    rec: &[OwnedValue],
    rowid_alias: Option<usize>,
    virt: Option<&[bool]>,
) -> AVec<u8> {
    let mut vals: AVec<Value<'_>> = AVec::new();
    for (i, v) in rec.iter().enumerate() {
        if virt.is_some_and(|m| m.get(i).copied().unwrap_or(false)) {
            continue;
        }
        if Some(i) == rowid_alias {
            vals.push(Value::Null);
        } else {
            vals.push(v.as_value());
        }
    }
    let mut out = AVec::new();
    encode_record(vals.as_slice(), &mut out);
    out
}

/// One column of a table definition, as the rule planner needs
/// it (v0.16): built from the catalog for writes, from the AST
/// for CREATE TABLE validation.
#[derive(Debug, Clone, Copy)]
pub struct ColSpec<'a> {
    pub name: &'a [u8],
    pub affinity: Affinity,
    /// Declared type text (STRICT validation).
    pub decl: &'a [u8],
    /// Generation expression text, for a generated column.
    pub gen_text: Option<&'a [u8]>,
    /// A STORED generated column.
    pub stored: bool,
    /// The column's collating sequence (v0.16): CHECK and
    /// generation expressions compare by it.
    pub coll: crate::record::Coll,
}

fn subquery_refused<E>(
    kind: SchemaKind,
) -> impl FnMut(&SelectStmt, &ScopeGroup, &[ScopeGroup]) -> Result<SubPlan, E> {
    move |_: &SelectStmt, _: &ScopeGroup, _: &[ScopeGroup]| Err(Error::Schema { kind })
}

fn copy(b: &[u8]) -> AVec<u8> {
    let mut v = AVec::new();
    v.extend_from_slice(b);
    v
}

/// Re-parse one stored rule text into `arena`. The text is copied
/// first: it may live in `arena` itself (CREATE TABLE validation).
fn parse_rule<E>(text: &[u8], arena: &Arena) -> Result<Expr, E> {
    let owned = copy(text);
    parse_expr_text::<E>(owned.as_slice(), arena)
}

/// Resolve a table's rules (v0.16): STRICT types, generated
/// columns (validated and ordered), CHECK constraints — with the
/// pin's definition-time errors, so CREATE TABLE refuses what the
/// pin refuses.
pub fn plan_rules<E>(
    arena: &Arena,
    tname: &[u8],
    cols: &[ColSpec<'_>],
    rowid_alias: Option<usize>,
    checks: &[&[u8]],
    strict: bool,
) -> Result<TableRules, E> {
    let mut rules = TableRules::default();
    if strict {
        for c in cols.iter() {
            match StrictType::of(c.decl) {
                Some(t) => rules.strict.push(t),
                None => {
                    return Err(Error::Schema {
                        kind: SchemaKind::StrictColumnType,
                    });
                }
            }
        }
    }
    let mut descs: AVec<(AVec<u8>, Affinity)> = AVec::new();
    let mut affs = AVec::new();
    let mut colls = AVec::new();
    for c in cols.iter() {
        descs.push((copy(c.name), c.affinity));
        affs.push(c.affinity);
        colls.push(c.coll);
    }
    let mut offsets = AVec::new();
    offsets.push(0);
    if cols.iter().any(|c| c.gen_text.is_some()) {
        if cols.iter().all(|c| c.gen_text.is_some()) {
            return Err(Error::Schema {
                kind: SchemaKind::InvalidGeneratedColumn,
            });
        }
        // The pin's generated-column scope: the table's columns
        // by bare name only — no `rowid` (a rowid alias column
        // still reads the rowid) and no qualifier ("the '.'
        // operator prohibited"), so the source carries an empty
        // qualifier no written reference can match.
        let mut srcs = AVec::new();
        let mut src = SourceDesc::new(AVec::new(), descs.clone(), rowid_alias, false);
        src.colls = colls.clone();
        srcs.push(src);
        let cur = ScopeGroup::new(srcs, offsets.clone(), 0);
        let mut pending: AVec<(usize, RExpr)> = AVec::new();
        let mut virt = AVec::new();
        for (i, c) in cols.iter().enumerate() {
            virt.push(c.gen_text.is_some() && !c.stored);
            let Some(text) = c.gen_text else { continue };
            let e = parse_rule::<E>(text, arena)?;
            if expr_any(&e, &mut |x| {
                matches!(x, Expr::Column { table: Some(_), .. })
            }) {
                return Err(Error::Schema {
                    kind: SchemaKind::InvalidGeneratedColumn,
                });
            }
            let mut planner = subquery_refused::<E>(SchemaKind::SubqueryInConstraint);
            let r = resolve_scoped::<E>(&e, arena, &cur, &[], &mut planner)?;
            if has_agg(&r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedAggregate,
                });
            }
            pending.push((i, r));
        }
        // Dependency order (iterative, so no recursion depth on
        // SQL-derived data): repeatedly emit every pending column
        // whose generated inputs are all emitted; a pass that
        // emits nothing is a loop (self-reference included).
        let mut order: AVec<(usize, RExpr)> = AVec::new();
        while !pending.is_empty() {
            let mut next: AVec<(usize, RExpr)> = AVec::new();
            let mut progressed = false;
            for (ci, e) in pending.iter() {
                let blocked = pending.iter().any(|(cj, _)| {
                    expr_refs_range(
                        e,
                        &RefRange {
                            w: (*cj, cj.saturating_add(1)),
                            s: (0, 0),
                        },
                    )
                }) || next.iter().any(|(cj, _)| {
                    expr_refs_range(
                        e,
                        &RefRange {
                            w: (*cj, cj.saturating_add(1)),
                            s: (0, 0),
                        },
                    )
                });
                if blocked {
                    next.push((*ci, e.clone()));
                } else {
                    order.push((*ci, e.clone()));
                    progressed = true;
                }
            }
            if !progressed {
                return Err(Error::Schema {
                    kind: SchemaKind::GeneratedColumnLoop,
                });
            }
            pending = next;
        }
        rules.gcols = Some(GenCols {
            virt,
            order,
            affs: affs.clone(),
            rowid_alias,
        });
    }
    if !checks.is_empty() {
        let mut srcs = AVec::new();
        let mut src = SourceDesc::new(copy(tname), descs, rowid_alias, true);
        src.colls = colls;
        srcs.push(src);
        let cur = ScopeGroup::new(srcs, offsets, 0);
        for text in checks.iter() {
            let e = parse_rule::<E>(text, arena)?;
            let mut planner = subquery_refused::<E>(SchemaKind::SubqueryInConstraint);
            let r = resolve_scoped::<E>(&e, arena, &cur, &[], &mut planner)?;
            if has_agg(&r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedAggregate,
                });
            }
            rules.checks.push(r);
        }
    }
    Ok(rules)
}

/// Validate a CREATE TABLE definition against the pin's
/// definition-time rules (v0.16) and return its resolved rules:
/// a generated column takes no DEFAULT and is no PRIMARY KEY
/// column, an expression DEFAULT is constant, and every CHECK /
/// generation expression resolves (see `plan_rules`). ALTER TABLE
/// ADD COLUMN validates the rewritten definition the same way.
pub fn validate_def<E>(
    arena: &Arena,
    tname: &[u8],
    columns: &[crate::sql::ColumnDef],
    pk_columns: &[crate::arena::ABytes],
    checks: &[crate::arena::ABytes],
    strict: bool,
) -> Result<TableRules, E> {
    use crate::sql::handle_bytes;
    let mut specs: AVec<ColSpec<'_>> = AVec::new();
    let mut alias = None;
    for (i, c) in columns.iter().enumerate() {
        if c.generated
            && (c.default.is_some() || c.default_expr.is_some() || c.primary_key || c.pk_desc)
        {
            return Err(Error::Schema {
                kind: SchemaKind::InvalidGeneratedColumn,
            });
        }
        if let Some(h) = c.default_expr {
            check_default_const::<E>(handle_bytes(arena, h), arena)?;
        }
        let decl = handle_bytes(arena, c.decl);
        let name = handle_bytes(arena, c.name);
        if pk_columns.len() == 1
            && eq_ci(name, handle_bytes(arena, pk_columns[0]))
            && !c.pk_desc
            && eq_ci(decl, b"INTEGER")
        {
            alias = Some(i);
        }
        let mut affinity = crate::record::affinity_of(decl);
        if strict && eq_ci(decl, b"ANY") {
            affinity = Affinity::Blob;
        }
        specs.push(ColSpec {
            name,
            affinity,
            decl,
            gen_text: c.gen_expr.map(|h| handle_bytes(arena, h)),
            stored: c.stored,
            coll: c.coll.unwrap_or_default(),
        });
    }
    let mut texts: AVec<&[u8]> = AVec::new();
    for h in checks.iter() {
        texts.push(handle_bytes(arena, *h));
    }
    plan_rules::<E>(
        arena,
        tname,
        specs.as_slice(),
        alias,
        texts.as_slice(),
        strict,
    )
}

/// Whether `f` holds for any node of `e` (pre-order; subquery
/// bodies are not entered — the subquery node itself is visited).
/// The walk follows the expression's own nesting, which the
/// parser bounds (`MAX_DEPTH`).
pub fn expr_any(e: &Expr, f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if f(e) {
        return true;
    }
    match e {
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::ScalarSubquery(_)
        | Expr::ExistsSubquery(_) => false,
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) => expr_any(a, f) || expr_any(b, f),
        Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => expr_any(a, f),
        #[cfg(feature = "v16")]
        Expr::Collate(a, _) => expr_any(a, f),
        Expr::InSubquery(a, _, _) => expr_any(a, f),
        Expr::InList(a, l, _) => expr_any(a, f) || l.iter().any(|x| expr_any(x, f)),
        Expr::Between(a, b, c, _) => expr_any(a, f) || expr_any(b, f) || expr_any(c, f),
        Expr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_ref().is_some_and(|b| expr_any(b, f))
                || whens.iter().any(|(w, t)| expr_any(w, f) || expr_any(t, f))
                || otherwise.as_ref().is_some_and(|o| expr_any(o, f))
        }
        Expr::Func { args, .. } => args.iter().any(|x| expr_any(x, f)),
    }
}

/// The wall-clock keyword names, the pseudo-columns a DEFAULT
/// expression resolves them as.
const CTIME: [&[u8]; 3] = [b"current_time", b"current_date", b"current_timestamp"];

fn is_ctime(name: &[u8]) -> bool {
    CTIME.iter().any(|c| eq_ci(c, name))
}

/// CREATE TABLE's DEFAULT check (v0.16; the pin's "default value
/// of column is not constant"): no column reference other than
/// the clock keywords, no subquery. Function names are NOT
/// checked — the pin defers an unknown function to the INSERT
/// that needs the default.
pub fn check_default_const<E>(text: &[u8], arena: &Arena) -> Result<(), E> {
    let e = parse_rule::<E>(text, arena)?;
    let bad = expr_any(&e, &mut |x| match x {
        Expr::Column { table: None, name } => !is_ctime(crate::sql::handle_bytes(arena, *name)),
        Expr::Column { .. }
        | Expr::ScalarSubquery(_)
        | Expr::ExistsSubquery(_)
        | Expr::InSubquery(..) => true,
        _ => false,
    });
    if bad {
        return Err(Error::Schema {
            kind: SchemaKind::NotConstantDefault,
        });
    }
    Ok(())
}

/// ALTER TABLE ADD COLUMN's DEFAULT rule (v0.16; the pin's
/// `sqlite3ValueFromExpr` subset): a literal, possibly signed
/// (`-'3'`, `-x'01'`, `-NULL`) or CAST — never an operator,
/// function or clock keyword ("Cannot add a column with
/// non-constant default").
pub fn alter_default_ok<E>(text: &[u8], arena: &Arena) -> Result<bool, E> {
    fn ok(e: &Expr) -> bool {
        match e {
            Expr::Literal(_) => true,
            Expr::Neg(a) | Expr::Cast(a, _) => ok(a),
            _ => false,
        }
    }
    Ok(ok(&parse_rule::<E>(text, arena)?))
}

/// Evaluate an expression DEFAULT for one statement (v0.16): the
/// expression resolves against the clock pseudo-columns only; the
/// clock is read (once) only when the expression names one, and
/// is `Error::NoClock` without a `Config::clock`. An unknown
/// function or an aggregate errors here, when the default is
/// needed — as on the pin.
pub fn eval_default<E>(
    text: &[u8],
    arena: &Arena,
    clock: Option<fn() -> i64>,
) -> Result<OwnedValue, E> {
    let e = parse_rule::<E>(text, arena)?;
    let mut cols = AVec::new();
    for c in CTIME.iter() {
        cols.push((copy(c), Affinity::Blob));
    }
    let mut srcs = AVec::new();
    srcs.push(SourceDesc::new(AVec::new(), cols, None, false));
    let mut offsets = AVec::new();
    offsets.push(0);
    let cur = ScopeGroup::new(srcs, offsets, 0);
    let mut planner = subquery_refused::<E>(SchemaKind::NotConstantDefault);
    let r = match resolve_scoped::<E>(&e, arena, &cur, &[], &mut planner) {
        Err(Error::Schema {
            kind: SchemaKind::UnknownColumn | SchemaKind::AmbiguousColumn,
        }) => {
            return Err(Error::Schema {
                kind: SchemaKind::NotConstantDefault,
            });
        }
        other => other?,
    };
    if has_agg(&r) {
        return Err(Error::Unsupported);
    }
    let mut row = RowData::empty();
    if expr_refs_range(&r, &RefRange::below(CTIME.len(), 0)) {
        let ms = clock.ok_or(Error::NoClock)?();
        for v in clock_values(ms) {
            row.vals.push(v);
        }
    } else {
        for _ in CTIME.iter() {
            row.vals.push(OwnedValue::Null);
        }
    }
    eval::<E>(&r, &Ctx::Row(&row))
}

/// CURRENT_TIME, CURRENT_DATE and CURRENT_TIMESTAMP for a Unix
/// time in milliseconds (v0.16): UTC, `HH:MM:SS`, `YYYY-MM-DD`,
/// `YYYY-MM-DD HH:MM:SS`, seconds truncated — the pin's formats.
/// Outside years 0000..=9999 all three are NULL.
pub fn clock_values(ms: i64) -> [OwnedValue; 3] {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's
    // algorithm; every intermediate fits i64 for any i64 input
    // divided down to days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    if !(0..=9999).contains(&y) {
        return [OwnedValue::Null, OwnedValue::Null, OwnedValue::Null];
    }
    let mut date = AVec::new();
    push_digits(&mut date, y, 4);
    date.push(b'-');
    push_digits(&mut date, m, 2);
    date.push(b'-');
    push_digits(&mut date, d, 2);
    let mut time = AVec::new();
    push_digits(&mut time, sod / 3600, 2);
    time.push(b':');
    push_digits(&mut time, sod / 60 % 60, 2);
    time.push(b':');
    push_digits(&mut time, sod % 60, 2);
    let mut stamp = date.clone();
    stamp.push(b' ');
    stamp.extend_from_slice(time.as_slice());
    [
        OwnedValue::Text(time),
        OwnedValue::Text(date),
        OwnedValue::Text(stamp),
    ]
}

/// `v` (0 <= v < 10^width) as exactly `width` decimal digits.
fn push_digits(out: &mut AVec<u8>, v: i64, width: u32) {
    let mut div = 10i64.pow(width.saturating_sub(1));
    while div > 0 {
        out.push(b'0' + (v / div % 10) as u8);
        div /= 10;
    }
}

/// Box helper for plan fields that are usually absent.
pub fn boxed_gen(r: &TableRules) -> Option<Box<GenCols>> {
    r.gcols.clone().map(Box::new)
}

// ---------------- CREATE TABLE ... AS SELECT (v16) ----------------

/// The pin's keywords (147, as `sqlite3_keyword_name` lists them;
/// verified against the oracle): a CREATE TABLE ... AS SELECT
/// column or table name that is one of them is double-quoted in
/// the derived schema text.
#[cfg(feature = "v16")]
const KEYWORDS: &str = "ABORT ACTION ADD AFTER ALL ALTER ALWAYS ANALYZE AND AS ASC ATTACH \
AUTOINCREMENT BEFORE BEGIN BETWEEN BY CASCADE CASE CAST CHECK COLLATE COLUMN COMMIT CONFLICT \
CONSTRAINT CREATE CROSS CURRENT CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP DATABASE DEFAULT \
DEFERRABLE DEFERRED DELETE DESC DETACH DISTINCT DO DROP EACH ELSE END ESCAPE EXCEPT EXCLUDE \
EXCLUSIVE EXISTS EXPLAIN FAIL FILTER FIRST FOLLOWING FOR FOREIGN FROM FULL GENERATED GLOB GROUP \
GROUPS HAVING IF IGNORE IMMEDIATE IN INDEX INDEXED INITIALLY INNER INSERT INSTEAD INTERSECT \
INTO IS ISNULL JOIN KEY LAST LEFT LIKE LIMIT MATCH MATERIALIZED NATURAL NO NOT NOTHING NOTNULL \
NULL NULLS OF OFFSET ON OR ORDER OTHERS OUTER OVER PARTITION PLAN PRAGMA PRECEDING PRIMARY \
QUERY RAISE RANGE RECURSIVE REFERENCES REGEXP REINDEX RELEASE RENAME REPLACE RESTRICT \
RETURNING RIGHT ROLLBACK ROW ROWS SAVEPOINT SELECT SET TABLE TEMP TEMPORARY THEN TIES TO \
TRANSACTION TRIGGER UNBOUNDED UNION UNIQUE UPDATE USING VACUUM VALUES VIEW VIRTUAL WHEN WHERE \
WINDOW WITH WITHOUT";

#[cfg(feature = "v16")]
fn is_keyword(w: &[u8]) -> bool {
    KEYWORDS.split(' ').any(|k| eq_ci(k.as_bytes(), w))
}

/// The pin's `identLength`: the name's length with each `"`
/// doubled, plus two quotes.
#[cfg(feature = "v16")]
fn ident_len(z: &[u8]) -> usize {
    z.len()
        .saturating_add(z.iter().filter(|b| **b == b'"').count())
        .saturating_add(2)
}

/// The pin's `identPut`: the name, double-quoted (inner `"`
/// doubled) when it is empty, starts with a digit, holds any byte
/// but an ASCII letter, digit or `_`, or is a keyword.
#[cfg(feature = "v16")]
fn ident_put(out: &mut AVec<u8>, z: &[u8]) {
    let j = z
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .unwrap_or(z.len());
    let quote = z.first().is_none_or(|b| b.is_ascii_digit()) || j < z.len() || is_keyword(z);
    if quote {
        out.push(b'"');
    }
    for b in z.iter() {
        out.push(*b);
        if *b == b'"' {
            out.push(b'"');
        }
    }
    if quote {
        out.push(b'"');
    }
}

/// The declared type CREATE TABLE ... AS SELECT gives a column of
/// affinity `aff` (the pin's `azType`): none for BLOB, else
/// ` TEXT`, ` NUM`, ` INT`, ` REAL`.
#[cfg(feature = "v16")]
fn aff_type(aff: Affinity) -> &'static [u8] {
    match aff {
        Affinity::Blob => b"",
        Affinity::Text => b" TEXT",
        Affinity::Numeric => b" NUM",
        Affinity::Integer => b" INT",
        Affinity::Real => b" REAL",
    }
}

/// The `sqlite_schema.sql` text of a CREATE TABLE ... AS SELECT
/// table (the pin's `createTableStmt`, byte for byte): one line
/// when the names are short, else one column per line.
#[cfg(feature = "v16")]
pub fn ctas_sql(tname: &[u8], cols: &[(AVec<u8>, Affinity)]) -> AVec<u8> {
    let mut n = ident_len(tname);
    for (c, _) in cols.iter() {
        n = n.saturating_add(ident_len(c.as_slice())).saturating_add(5);
    }
    let (first, sep, end): (&[u8], &[u8], &[u8]) = if n < 50 {
        (b"", b",", b")")
    } else {
        (b"\n  ", b",\n  ", b"\n)")
    };
    let mut out = AVec::new();
    out.extend_from_slice(b"CREATE TABLE ");
    ident_put(&mut out, tname);
    out.push(b'(');
    for (i, (c, aff)) in cols.iter().enumerate() {
        out.extend_from_slice(if i == 0 { first } else { sep });
        ident_put(&mut out, c.as_slice());
        out.extend_from_slice(aff_type(*aff));
    }
    out.extend_from_slice(end);
    out
}

/// Make CREATE TABLE ... AS SELECT column names (since v0.16
/// also a derived table's and a view's) unique the pin's way
/// (`sqlite3ColumnsFromExprList`): a name equal (ignoring
/// case) to an earlier one loses any `:digits` suffix and takes
/// `:1`, `:2`, ... until unique. Past three attempts the pin
/// switches to a random suffix, which hipp cannot reproduce:
/// `Error::Unsupported` then.
#[cfg(feature = "v04")]
pub fn unique_names<E>(names: &mut [AVec<u8>]) -> Result<(), E> {
    for i in 0..names.len() {
        let mut cnt = 0u32;
        loop {
            let (done, rest) = names.split_at_mut(i);
            let Some(cur) = rest.first_mut() else { break };
            if !done.iter().any(|d| eq_ci(d.as_slice(), cur.as_slice())) {
                break;
            }
            if cnt >= 3 {
                return Err(Error::Unsupported);
            }
            cnt += 1;
            let mut stem = cur.as_slice();
            if !stem.is_empty() {
                let mut j = stem.len() - 1;
                while j > 0 && stem.get(j).is_some_and(|b| b.is_ascii_digit()) {
                    j -= 1;
                }
                if stem.get(j) == Some(&b':') {
                    stem = stem.get(..j).unwrap_or(&[]);
                }
            }
            let mut next = copy(stem);
            next.push(b':');
            let mut digits = AVec::new();
            crate::record::render_int(i64::from(cnt), &mut digits);
            next.extend_from_slice(digits.as_slice());
            *cur = next;
        }
    }
    Ok(())
}

/// The pin's `sqlite3ExprAffinity` of one resolved result item:
/// a column's affinity, a CAST's, INTEGER for a rowid, a scalar
/// subquery's first result item's; none (BLOB) for anything else.
#[cfg(feature = "v16")]
pub fn item_affinity(e: &RExpr) -> Affinity {
    match e {
        RExpr::Column(_, a, _) | RExpr::Cast(_, a) => *a,
        RExpr::Collate(x, _) => item_affinity(x),
        RExpr::Rowid(_) => Affinity::Integer,
        RExpr::SubScalar(p) => p
            .items
            .as_slice()
            .first()
            .map_or(Affinity::Blob, item_affinity),
        _ => Affinity::Blob,
    }
}

/// The pin's `sqlite3ExprDataType` bits of one later compound
/// core's result item (0x01 numeric-ish, 0x02 text, 0x04 blob),
/// which can demote the leftmost core's affinity.
#[cfg(feature = "v16")]
pub fn item_datatype(e: &RExpr) -> u8 {
    match e {
        RExpr::Literal(OwnedValue::Null) => 0x00,
        RExpr::Literal(OwnedValue::Text(_)) => 0x02,
        RExpr::Literal(OwnedValue::Blob(_)) => 0x04,
        RExpr::Binary(crate::sql::BinOp::Concat, ..) => 0x06,
        RExpr::Scalar(..) | RExpr::Agg { .. } => 0x07,
        RExpr::Column(..) | RExpr::Cast(..) | RExpr::Rowid(_) | RExpr::SubScalar(_) => {
            match item_affinity(e) {
                Affinity::Numeric | Affinity::Integer | Affinity::Real => 0x05,
                Affinity::Text => 0x06,
                Affinity::Blob => 0x07,
            }
        }
        RExpr::Case {
            whens, otherwise, ..
        } => {
            let mut m = whens.iter().fold(0u8, |m, (_, t)| m | item_datatype(t));
            if let Some(o) = otherwise {
                m |= item_datatype(o);
            }
            m
        }
        _ => 0x01,
    }
}
