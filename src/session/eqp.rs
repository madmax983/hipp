//! EXPLAIN QUERY PLAN (v0.16, `explain`).
//!
//! `EXPLAIN QUERY PLAN <statement>` prepares the statement as usual,
//! never runs it, and returns one row per step of the plan hipp made
//! for it: the pin's four columns `id`, `parent`, `notused` and
//! `detail`. `parent` links the rows into the pin's tree (0: the
//! top); `id` numbers the rows in order (the pin's are bytecode
//! addresses, which hipp has none of) and `notused` is 0. The CLI
//! prints the pin shell's tree (`QUERY PLAN`, `|--`, `` `-- ``).
//!
//! The rows describe what hipp executes, in the pin's words and in
//! the pin's order where the strategies agree:
//!
//! - one loop per FROM source in join order: `SCAN t`, `SCAN t USING
//!   [COVERING] INDEX i`, `SEARCH t USING [COVERING] INDEX i (a=? AND
//!   b>?)`, `SEARCH t USING INTEGER PRIMARY KEY (rowid=?)`, `SEARCH w
//!   USING PRIMARY KEY (k=?)` for a WITHOUT ROWID table, with
//!   ` LEFT-JOIN` on the right side of an outer join, and a
//!   `RIGHT-JOIN t` step for RIGHT / FULL joins; `SCAN x VIRTUAL TABLE
//!   INDEX n:str` for a virtual table or table-valued function;
//!   `SCAN CONSTANT ROW` / `SCAN n-ROW VALUES CLAUSE` without one;
//! - the sorters hipp's planner leaves to the executor: `USE TEMP
//!   B-TREE FOR GROUP BY`, `DISTINCT`, `ORDER BY` (`LAST TERM OF` /
//!   `LAST n TERMS OF ORDER BY` when the loop delivers a prefix) and
//!   `f(DISTINCT)` aggregates;
//! - subqueries where they are evaluated: `[CORRELATED] SCALAR
//!   SUBQUERY n`, `[CORRELATED] LIST SUBQUERY n` (n is the pin's
//!   select number), FROM subqueries, views and CTEs computed as
//!   `CO-ROUTINE x` (read once, in order, as the first loop) or
//!   `MATERIALIZE x` (shared, or read inside a join), a recursive
//!   CTE's `SETUP` / `RECURSIVE STEP`, the window co-routines
//!   `(subquery-n)`, compound `MERGE (op)` / `COMPOUND QUERY` trees.
//!
//! What hipp does differently from the pin is printed as hipp does it
//! (STATUS.md lists the differences): it does not flatten FROM
//! subqueries, reorder joins, build automatic indexes or Bloom
//! filters, turn EXISTS into a join, use an index for an IN operator
//! or a min() / max() lookup, nor run a MULTI-INDEX OR.

use super::*;
use crate::query::{RExpr, SubPlan, SubSource};
use crate::schema::{Catalog, TableDef};
use crate::sql::{CompoundOp, JoinKind, SortOrder};

/// The rows of an EXPLAIN QUERY PLAN: (id, parent, detail).
pub(super) type EqpRows = AVec<(i64, i64, AVec<u8>)>;

/// The last byte of a count(*) loop's detail until the row is
/// returned (`step_explain` drops it): `requalify` leaves the line
/// as it is.
const COUNT_MARK: u8 = 0;

/// Where the statement after a leading `EXPLAIN QUERY PLAN` starts,
/// if `sql` has one; plain `EXPLAIN` is refused
/// (`ParseKind::ExplainBytecode`).
pub(super) fn query_plan_prefix<E>(sql: &[u8]) -> Result<Option<usize>, E> {
    let mut i = 0usize;
    let mut words = [0usize; 3];
    for (k, w) in [&b"EXPLAIN"[..], b"QUERY", b"PLAN"].iter().enumerate() {
        // Blanks and comments.
        loop {
            match (sql.get(i), sql.get(i + 1)) {
                (Some(c), _) if c.is_ascii_whitespace() => i += 1,
                (Some(b'-'), Some(b'-')) => {
                    while sql.get(i).is_some_and(|c| *c != b'\n') {
                        i += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    i += 2;
                    while i < sql.len() && !sql[i..].starts_with(b"*/") {
                        i += 1;
                    }
                    i = (i + 2).min(sql.len());
                }
                _ => break,
            }
        }
        let start = i;
        while sql
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c >= 0x80)
        {
            i += 1;
        }
        let word = sql.get(start..i).unwrap_or(&[]);
        if !word.eq_ignore_ascii_case(w) {
            return match k {
                0 => Ok(None),
                // `EXPLAIN <statement>`: the pin's bytecode listing.
                1 => Err(Error::Parse {
                    offset: words[0],
                    kind: crate::error::ParseKind::ExplainBytecode,
                }),
                _ => Err(Error::Parse {
                    offset: start,
                    kind: crate::error::ParseKind::UnexpectedToken,
                }),
            };
        }
        if let Some(s) = words.get_mut(k) {
            *s = start;
        }
    }
    Ok(Some(i))
}

/// A FROM source's EXPLAIN QUERY PLAN names (`SubSource::eqp`).
pub(super) fn source_names(s: &crate::sql::FromSource, arena: &Arena) -> (AVec<u8>, AVec<u8>) {
    let name = handle_bytes(arena, s.table);
    let alias = s.alias.map(|a| copy_vec(handle_bytes(arena, a)));
    if s.derived.is_some() {
        let own = if s.cte.is_some() {
            copy_vec(name)
        } else {
            AVec::new()
        };
        let scan = alias.clone().unwrap_or_else(|| own.clone());
        let mat = if own.is_empty() {
            alias.unwrap_or_default()
        } else {
            own
        };
        return (scan, mat);
    }
    let scan = match alias {
        Some(a) => a,
        None => {
            let mut v = AVec::new();
            #[cfg(feature = "attach")]
            let schema = s.schema.map(|h| handle_bytes(arena, h));
            #[cfg(not(feature = "attach"))]
            let schema: Option<&[u8]> = None;
            let schema = match schema {
                Some(x) => Some(x),
                None if s.in_temp => Some(&b"temp"[..]),
                None => None,
            };
            if let Some(x) = schema {
                v.extend_from_slice(x);
                v.push(b'.');
            }
            v.extend_from_slice(name);
            v
        }
    };
    (scan, AVec::new())
}

/// Append `n` in decimal (or `0x` hex) to `out`.
fn num(out: &mut AVec<u8>, n: u64, hex: bool) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let base = if hex { 16 } else { 10 };
    let mut n = n;
    loop {
        i -= 1;
        let d = (n % base) as u8;
        if let Some(b) = buf.get_mut(i) {
            *b = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        }
        n /= base;
        if n == 0 || i == 0 {
            break;
        }
    }
    if hex {
        out.extend_from_slice(b"0x");
    }
    out.extend_from_slice(buf.get(i..).unwrap_or(&[]));
}

/// One SELECT core, whichever plan type holds it.
#[derive(Clone)]
struct Core<'x> {
    sources: &'x [SubSource],
    items: &'x [RExpr],
    filter: Option<&'x RExpr>,
    group_by: &'x [RExpr],
    having: Option<&'x RExpr>,
    order_by: &'x [(RExpr, SortOrder)],
    distinct: bool,
    has_agg: bool,
    values: &'x [AVec<RExpr>],
    sel: u32,
}

/// A select statement: its cores and their compound operators.
struct Sel<'x> {
    cores: AVec<(CompoundOp, Core<'x>)>,
    comp_order: usize,
    limit: Option<&'x RExpr>,
    offset: Option<&'x RExpr>,
}

fn core_of_sub(p: &SubPlan) -> Core<'_> {
    Core {
        sources: p.sources.as_slice(),
        items: p.items.as_slice(),
        filter: p.filter.as_ref(),
        group_by: p.group_by.as_slice(),
        having: p.having.as_ref(),
        order_by: p.order_by.as_slice(),
        distinct: p.distinct,
        has_agg: p.has_agg,
        values: p.values.as_slice(),
        sel: p.sel,
    }
}

fn sel_of_sub(p: &SubPlan) -> Sel<'_> {
    let mut cores = AVec::new();
    cores.push((CompoundOp::UnionAll, core_of_sub(p)));
    for (op, t) in p.compound.iter() {
        cores.push((*op, core_of_sub(t)));
    }
    Sel {
        cores,
        comp_order: p.comp_order.len(),
        limit: p.limit.as_ref(),
        offset: p.offset.as_ref(),
    }
}

fn core_of_qp(p: &QueryPlan) -> Core<'_> {
    Core {
        sources: p.sources.as_slice(),
        items: p.items.as_slice(),
        filter: p.filter.as_ref(),
        group_by: p.group_by.as_slice(),
        having: p.having.as_ref(),
        order_by: p.order_by.as_slice(),
        distinct: p.distinct,
        has_agg: p.has_agg,
        values: p.values.as_slice(),
        sel: p.sel,
    }
}

fn sel_of_qp(p: &QueryPlan) -> Sel<'_> {
    let mut cores = AVec::new();
    cores.push((CompoundOp::UnionAll, core_of_qp(p)));
    for (op, t) in p.compound.iter() {
        cores.push((*op, core_of_qp(t)));
    }
    Sel {
        cores,
        comp_order: p.comp_order.len(),
        limit: p.limit.as_ref(),
        offset: p.offset.as_ref(),
    }
}

/// The subquery nodes of `e`, outermost first, left to right (not
/// those inside them).
fn subs<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) => out.push(e),
        RExpr::InSub(a, ..) => {
            subs(a, out);
            out.push(e);
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => {}
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            subs(a, out);
            subs(b, out);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => subs(a, out),
        RExpr::InList(x, l, ..) => {
            subs(x, out);
            for i in l.iter() {
                subs(i, out);
            }
        }
        RExpr::Between(x, a, b, _) => {
            subs(x, out);
            subs(a, out);
            subs(b, out);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                subs(b, out);
            }
            for (w, t) in whens.iter() {
                subs(w, out);
                subs(t, out);
            }
            if let Some(o) = otherwise {
                subs(o, out);
            }
        }
        RExpr::Scalar(_, args) | RExpr::Win(_, args) | RExpr::Agg { args, .. } => {
            for a in args.iter() {
                subs(a, out);
            }
        }
    }
}

/// The aggregate calls of `e` that sort or deduplicate their
/// arguments in a temp B-tree (`USE TEMP B-TREE FOR f(DISTINCT)` /
/// `f(ORDER BY)`), each once: the pin's `aFunc` list holds one entry
/// per distinct call.
fn agg_sorters<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    if let RExpr::Agg {
        distinct,
        lvl: 0,
        ext,
        ..
    } = e
    {
        let order = ext.as_ref().is_some_and(|x| !x.order.is_empty());
        if (*distinct || order) && !out.iter().any(|o| crate::query::same_expr(o, e)) {
            out.push(e);
        }
        return;
    }
    let mut kids: AVec<&RExpr> = AVec::new();
    children(e, &mut kids);
    for k in kids.iter() {
        agg_sorters(k, out);
    }
}

/// An aggregate's name as the pin's function table spells it.
fn agg_name(kind: crate::query::AggKind, ext: Option<&crate::query::AggExt>) -> &'static [u8] {
    use crate::query::AggKind as K;
    match kind {
        K::Count => b"count",
        K::Sum => b"sum",
        K::Avg => b"avg",
        K::Total => b"total",
        K::Min => b"min",
        K::Max => b"max",
        K::GroupConcat if ext.is_some_and(|x| x.string_agg) => b"string_agg",
        K::GroupConcat => b"group_concat",
        K::Median => b"median",
        K::Percentile => b"percentile",
        K::PercentileCont => b"percentile_cont",
        K::PercentileDisc => b"percentile_disc",
        #[cfg(feature = "json")]
        K::Json(j) => match (j.object, j.blob) {
            (false, false) => b"json_group_array",
            (false, true) => b"jsonb_group_array",
            (true, false) => b"json_group_object",
            (true, true) => b"jsonb_group_object",
        },
    }
}

/// The direct operands of `e` (not into subqueries).
fn children<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    match e {
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            out.push(a);
            out.push(b);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _)
        | RExpr::InSub(a, ..) => out.push(a),
        RExpr::InList(x, l, ..) => {
            out.push(x);
            for i in l.iter() {
                out.push(i);
            }
        }
        RExpr::Between(x, a, b, _) => {
            out.push(x);
            out.push(a);
            out.push(b);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                out.push(b);
            }
            for (w, t) in whens.iter() {
                out.push(w);
                out.push(t);
            }
            if let Some(o) = otherwise {
                out.push(o);
            }
        }
        RExpr::Scalar(_, a) | RExpr::Win(_, a) | RExpr::Agg { args: a, .. } => {
            for x in a.iter() {
                out.push(x);
            }
        }
        _ => {}
    }
}

/// A multi-row VALUES: the pin's VALUES co-routine (`SCAN n-ROW
/// VALUES CLAUSE`), or, when a row holds a subquery, its chain of
/// one-row selects (`SCAN n CONSTANT ROWS`).
fn values_line(w: &mut Walk<'_>, parent: i64, rows: &[AVec<RExpr>]) {
    let mut found: AVec<&RExpr> = AVec::new();
    for r in rows.iter() {
        for e in r.iter() {
            subs(e, &mut found);
        }
    }
    let n = rows.len() as u32;
    if found.is_empty() {
        w.add_n(parent, b"SCAN ", n, b"-ROW VALUES CLAUSE");
    } else {
        w.add_n(parent, b"SCAN ", n, b" CONSTANT ROWS");
    }
}

/// Whether `ix` is a WITHOUT ROWID table's clustered PRIMARY KEY.
#[cfg(feature = "expr_index")]
fn clustered(ix: &crate::schema::IndexDef) -> bool {
    #[cfg(feature = "without_rowid")]
    {
        ix.clustered
    }
    #[cfg(not(feature = "without_rowid"))]
    {
        let _ = ix;
        false
    }
}

/// A DISTINCT core read through a one-row rowid lookup (see `body`).
fn one_row_distinct(c: &Core<'_>) -> bool {
    #[cfg(feature = "expr_index")]
    {
        c.distinct
            && c.sources.len() == 1
            && c.sources
                .first()
                .and_then(|s| s.access.as_deref())
                .is_some_and(|a| a.root == 0 && !a.eq.is_empty() && a.eq.iter().all(|p| !p.is_in))
    }
    #[cfg(not(feature = "expr_index"))]
    {
        let _ = c;
        false
    }
}

/// A select's number as the pin's `Select` node of it carries it:
/// a compound's is its last core's.
fn last_sel(p: &SubPlan) -> u32 {
    p.compound.as_slice().last().map_or(p.sel, |(_, t)| t.sel)
}

/// The plan walk.
#[cfg_attr(not(feature = "expr_index"), allow(dead_code))]
pub(super) struct Walk<'a> {
    cat: &'a Catalog,
    ca: &'a Arena,
    rows: EqpRows,
    /// The largest select number of the statement's own selects, and
    /// of all of them (view copies included): the pass that renders
    /// numbers them after both (see `SelectStmt::sel`).
    parsed: u32,
    all: u32,
    /// The next window co-routine number.
    win: u32,
    /// Subqueries already shown, and shared materializations.
    seen: AVec<usize>,
    mats: AVec<u32>,
    /// The statement is an `UPDATE ... FROM` (the pin's co-routine rule).
    update_from: bool,
    /// The subqueries being shown are an index loop's IN probes (no
    /// Bloom filter of the IN operator; v0.17).
    probing: bool,
}

/// View copies are numbered from here (`cte::Expander`), then shown
/// after the statement's own selects.
pub(super) const VIEW_SEL: u32 = 0x4000_0000;

impl<'a> Walk<'a> {
    pub(super) fn new(cat: &'a Catalog, ca: &'a Arena) -> Walk<'a> {
        Walk {
            cat,
            ca,
            rows: AVec::new(),
            parsed: 0,
            all: 0,
            win: 0,
            seen: AVec::new(),
            mats: AVec::new(),
            update_from: false,
            probing: false,
        }
    }

    /// Run `f` twice: once to learn the select numbers in use, then
    /// to render with window co-routines numbered after them.
    pub(super) fn run(mut self, f: &dyn Fn(&mut Walk<'a>)) -> EqpRows {
        f(&mut self);
        let extra = self.all.saturating_sub(VIEW_SEL);
        self.win = self.parsed.saturating_add(extra);
        self.rows = AVec::new();
        self.seen = AVec::new();
        self.mats = AVec::new();
        f(&mut self);
        self.rows
    }

    /// A select number as the pin shows it.
    fn shown(&mut self, sel: u32) -> u32 {
        if sel > VIEW_SEL {
            self.all = self.all.max(sel);
            self.parsed.saturating_add(sel - VIEW_SEL)
        } else {
            self.parsed = self.parsed.max(sel);
            sel
        }
    }

    fn add(&mut self, parent: i64, parts: &[&[u8]]) -> i64 {
        let mut d = AVec::new();
        for p in parts {
            d.extend_from_slice(p);
        }
        let id = self.rows.len() as i64 + 1;
        self.rows.push((id, parent, d));
        id
    }

    fn add_n(&mut self, parent: i64, head: &[u8], n: u32, tail: &[u8]) -> i64 {
        let mut d = AVec::new();
        d.extend_from_slice(head);
        num(&mut d, u64::from(n), false);
        d.extend_from_slice(tail);
        self.add(parent, &[d.as_slice()])
    }

    /// A whole select statement under `parent`.
    pub(super) fn select_qp(&mut self, qp: &QueryPlan, parent: i64) {
        let s = sel_of_qp(qp);
        self.select(&s, parent);
    }

    pub(super) fn select_sub(&mut self, p: &SubPlan, parent: i64) {
        let s = sel_of_sub(p);
        self.select(&s, parent);
    }

    fn select(&mut self, s: &Sel<'_>, parent: i64) {
        for e in s.limit.iter().chain(s.offset.iter()) {
            self.exprs(core::slice::from_ref(*e), parent, &[]);
        }
        let n = s.cores.len();
        if n == 1 {
            if let Some((_, c)) = s.cores.as_slice().first() {
                self.core(c, parent, true);
            }
            return;
        }
        // The pin merges the compound up to its last operator other
        // than UNION ALL (all of it under an ORDER BY), as hipp's
        // executor does (`session::run_compound`); the UNION ALL
        // members after that are appended.
        // The pin's UNION-with-LIMIT-1 shortcut reads the members as
        // UNION ALL ones until a row is found (`union_first_row`).
        if s.comp_order == 0
            && s.offset.is_none()
            && matches!(s.limit, Some(RExpr::Literal(OwnedValue::Integer(1))))
            && s.cores
                .iter()
                .skip(1)
                .all(|(op, _)| *op == CompoundOp::Union)
        {
            let mut all = Sel {
                cores: AVec::new(),
                comp_order: 0,
                limit: None,
                offset: None,
            };
            for (_, c) in s.cores.iter() {
                all.cores.push((CompoundOp::UnionAll, c.clone()));
            }
            self.chain(&all, n - 1, 0, parent);
            return;
        }
        let last = if s.comp_order > 0 {
            n - 1
        } else {
            s.cores
                .iter()
                .rposition(|(op, _)| *op != CompoundOp::UnionAll)
                .unwrap_or(0)
        };
        self.chain(s, n - 1, last, parent);
    }

    /// Cores `0..=k` of a compound; returns the parent its later
    /// members go under.
    fn chain(&mut self, s: &Sel<'_>, k: usize, last: usize, parent: i64) -> i64 {
        let Some((op, c)) = s.cores.as_slice().get(k) else {
            return parent;
        };
        if k == 0 {
            self.core(c, parent, true);
            return parent;
        }
        let name: &[u8] = match op {
            CompoundOp::Union => b"UNION",
            CompoundOp::UnionAll => b"UNION ALL",
            CompoundOp::Intersect => b"INTERSECT",
            CompoundOp::Except => b"EXCEPT",
        };
        if k <= last && last > 0 {
            // The merge key's length: the ORDER BY terms, and every
            // other column unless the last operator is UNION ALL.
            let all = s
                .cores
                .as_slice()
                .last()
                .is_none_or(|(op, _)| *op != CompoundOp::UnionAll);
            let klen = if s.comp_order == 0 || all {
                usize::MAX
            } else {
                s.comp_order
            };
            let m = self.add(parent, &[b"MERGE (", name, b")"]);
            let l = self.add(m, &[b"LEFT"]);
            match s.cores.as_slice().first() {
                Some((_, c0)) if k == 1 => {
                    self.core(c0, l, false);
                    self.merge_sort(c0, l, klen);
                }
                _ => {
                    self.chain(s, k - 1, last, l);
                }
            }
            let r = self.add(m, &[b"RIGHT"]);
            self.core(c, r, false);
            self.merge_sort(c, r, klen);
            return parent;
        }
        let up = if k == 1 {
            let h = self.add(parent, &[b"COMPOUND QUERY"]);
            let l = self.add(h, &[b"LEFT-MOST SUBQUERY"]);
            if let Some((_, c0)) = s.cores.as_slice().first() {
                self.core(c0, l, true);
            }
            h
        } else {
            self.chain(s, k - 1, last, parent)
        };
        let u = self.add(up, &[name]);
        self.core(c, u, true);
        up
    }

    /// A merged compound member's sort (hipp sorts each member).
    fn merge_sort(&mut self, c: &Core<'_>, parent: i64, klen: usize) {
        let klen = klen.min(c.items.len());
        if c.values.len() <= 1 && c.sources.is_empty() {
            return;
        }
        // The merge key, when the member's lone loop delivers it
        // (`Connection::merge_core`).
        #[cfg(feature = "expr_index")]
        if c.sources.len() == 1
            && c.sources
                .first()
                .and_then(|s| s.access.as_deref())
                .is_some_and(|a| a.eqp.0 >= klen)
        {
            return;
        }
        let lone = c.sources.first().filter(|_| c.sources.len() == 1);
        #[cfg(feature = "expr_index")]
        let done = lone
            .and_then(|s| s.access.as_deref())
            .map_or(0, |a| a.eqp.0);
        #[cfg(not(feature = "expr_index"))]
        let done = 0;
        let done = done.max(self.natural(lone));
        self.sorter(parent, klen, done);
    }

    /// `USE TEMP B-TREE FOR ORDER BY` for the `k - done` terms after
    /// the `done` a loop delivers.
    fn sorter(&mut self, parent: i64, k: usize, done: usize) {
        if done >= k {
            return;
        }
        let rest = k - done;
        if done == 0 {
            self.add(parent, &[b"USE TEMP B-TREE FOR ORDER BY"]);
        } else if rest == 1 {
            self.add(parent, &[b"USE TEMP B-TREE FOR LAST TERM OF ORDER BY"]);
        } else {
            self.add_n(
                parent,
                b"USE TEMP B-TREE FOR LAST ",
                rest as u32,
                b" TERMS OF ORDER BY",
            );
        }
    }

    /// One SELECT core; `own_order`: its ORDER BY is its own (not a
    /// merged compound member's).
    fn core(&mut self, c: &Core<'_>, parent: i64, own_order: bool) {
        self.shown(c.sel);
        // VALUES and FROM-less cores.
        if !c.values.is_empty() || c.sources.is_empty() {
            if c.values.len() > 1 {
                values_line(self, parent, c.values);
            } else {
                self.add(parent, &[b"SCAN CONSTANT ROW"]);
            }
            for r in c.values.iter() {
                self.exprs(r.as_slice(), parent, &[]);
            }
            self.exprs(c.items, parent, &[]);
            return;
        }
        #[cfg(feature = "window")]
        {
            let mut classes: AVec<(usize, bool)> = AVec::new();
            win_classes(c.items, &mut classes);
            for (e, _) in c.order_by.iter() {
                win_classes(core::slice::from_ref(e), &mut classes);
            }
            if !classes.is_empty() {
                return self.window(c, parent, classes.as_slice());
            }
        }
        self.body(c, parent, own_order, c);
    }

    /// The window co-routines (v0.16, `window`): class 0 is computed
    /// over a co-routine computing class 1, and so on; the innermost
    /// one runs the core's loops. Each class with partition or order
    /// keys sorts its input.
    #[cfg(feature = "window")]
    fn window(&mut self, c: &Core<'_>, parent: i64, classes: &[(usize, bool)]) {
        let mut stack: AVec<(i64, u32)> = AVec::new();
        let mut p = parent;
        for _ in classes.iter() {
            self.win = self.win.saturating_add(1);
            let n = self.win;
            let mut d = AVec::new();
            d.extend_from_slice(b"CO-ROUTINE (subquery-");
            num(&mut d, u64::from(n), false);
            d.push(b')');
            let id = self.add(p, &[d.as_slice()]);
            stack.push((p, n));
            p = id;
        }
        // The innermost: the core without its ORDER BY.
        let inner = Core {
            sources: c.sources,
            items: &[],
            filter: c.filter,
            group_by: c.group_by,
            having: c.having,
            order_by: &[],
            distinct: false,
            has_agg: c.has_agg,
            values: &[],
            sel: c.sel,
        };
        self.body(&inner, p, false, c);
        // The innermost class's keys, which the loop may deliver.
        let keys = crate::window::class_keys(c.items, None).unwrap_or_default();
        let first = c
            .sources
            .first()
            .filter(|_| c.sources.len() == 1 && !c.has_agg && c.group_by.is_empty());
        #[cfg(feature = "expr_index")]
        let mut delivered = first
            .and_then(|s| s.access.as_deref())
            .is_some_and(|a| a.eqp.0 >= keys.len());
        #[cfg(not(feature = "expr_index"))]
        let mut delivered = false;
        delivered |= !keys.is_empty() && self.natural(first) >= keys.len();
        for ((outer, n), (_, sorts)) in stack.iter().rev().zip(classes.iter().rev()) {
            if *sorts && !core::mem::take(&mut delivered) {
                self.add(p, &[b"USE TEMP B-TREE FOR ORDER BY"]);
            }
            let mut d = AVec::new();
            d.extend_from_slice(b"SCAN (subquery-");
            num(&mut d, u64::from(*n), false);
            d.push(b')');
            self.add(*outer, &[d.as_slice()]);
            p = *outer;
        }
        self.exprs(c.items, parent, &[]);
        if c.distinct {
            self.add(parent, &[b"USE TEMP B-TREE FOR DISTINCT"]);
        }
        if !c.order_by.is_empty() {
            self.order_exprs(c, parent);
            // Rows leave the outermost class in its sort order, which
            // the pin knows satisfies an ORDER BY of its leading keys.
            let k0 = classes
                .first()
                .and_then(|(k, _)| crate::window::class_keys(c.items, Some(*k)))
                .unwrap_or_default();
            let sorted = c.order_by.len() <= k0.len()
                && c.order_by.iter().zip(k0.iter()).all(|((a, x), (b, y))| {
                    x.desc == y.desc
                        && x.nulls_first == y.nulls_first
                        && crate::query::same_expr(a, b)
                });
            if !sorted {
                self.add(parent, &[b"USE TEMP B-TREE FOR ORDER BY"]);
            }
        }
    }

    fn order_exprs(&mut self, c: &Core<'_>, parent: i64) {
        for (e, _) in c.order_by.iter() {
            self.exprs(core::slice::from_ref(e), parent, &[]);
        }
    }

    /// The table of a core the pin answers by counting a B-tree's
    /// entries (its `isSimpleCount`): `SELECT count(*) FROM t`, one
    /// table, no WHERE, GROUP BY or HAVING, no DISTINCT or FILTER.
    fn simple_count(&self, c: &Core<'_>) -> Option<&'a TableDef> {
        let [sp] = c.sources else { return None };
        let [
            RExpr::Agg {
                kind: crate::query::AggKind::Count,
                star: true,
                distinct: false,
                lvl: 0,
                ext: None,
                ..
            },
        ] = c.items
        else {
            return None;
        };
        if c.filter.is_some()
            || !c.group_by.is_empty()
            || c.having.is_some()
            || sp.derived.is_some()
        {
            return None;
        }
        #[cfg(feature = "vtab")]
        if sp.vt.is_some() {
            return None;
        }
        self.cat
            .tables
            .iter()
            .find(|t| t.root == sp.root && sp.root > 1)
    }

    /// The aggregates' temp B-trees of core `c` (`agg_sorters`), in
    /// the pin's `aFunc` order: the result list's calls, then the
    /// ORDER BY's, then the HAVING's. A DISTINCT call whose one ORDER
    /// BY key is its argument deduplicates in its ORDER BY sorter.
    fn agg_lines(&mut self, c: &Core<'_>, parent: i64) {
        let mut aggs: AVec<&RExpr> = AVec::new();
        for e in c.items.iter() {
            agg_sorters(e, &mut aggs);
        }
        for (e, _) in c.order_by.iter() {
            agg_sorters(e, &mut aggs);
        }
        if let Some(h) = c.having {
            agg_sorters(h, &mut aggs);
        }
        for e in aggs.iter() {
            let RExpr::Agg {
                kind,
                distinct,
                ext,
                ..
            } = e
            else {
                continue;
            };
            let name = agg_name(*kind, ext.as_deref());
            let (order, unique) = ext
                .as_ref()
                .map_or((false, false), |x| (!x.order.is_empty(), x.unique));
            // A min() / max() query (v0.17, `multi_index`): no sorter
            // for its ORDER BY, nor for its DISTINCT when the loops
            // deliver the argument's order.
            #[cfg(feature = "multi_index")]
            let mm = c.sources.first().map_or(0, |s| s.eqp.minmax);
            #[cfg(not(feature = "multi_index"))]
            let mm = 0u8;
            if *distinct && !unique && mm != 2 {
                self.add(parent, &[b"USE TEMP B-TREE FOR ", name, b"(DISTINCT)"]);
            }
            if order && mm == 0 {
                self.add(parent, &[b"USE TEMP B-TREE FOR ", name, b"(ORDER BY)"]);
            }
        }
    }

    /// A core's loops, subqueries and sorters; `aggs`: the core whose
    /// aggregates these loops feed (`c` itself, or the core a window
    /// co-routine computes).
    fn body(&mut self, c: &Core<'_>, parent: i64, own_order: bool, aggs: &Core<'_>) {
        // The aggregates' sorters open before the loop without a GROUP
        // BY, and with one where each group's accumulators are reset:
        // after the loop, the GROUP BY sorter and the result columns
        // (v0.16 review fix: `f(ORDER BY)` was never shown, and a
        // grouped `f(DISTINCT)` came first).
        if c.group_by.is_empty() {
            self.agg_lines(aggs, parent);
        }
        // The subqueries an access path probes with: shown with its
        // loop, not again with the WHERE.
        let mut probes: AVec<usize> = AVec::new();
        // FROM subqueries, views and CTEs first.
        let n = c.sources.len();
        let ltorj = c.sources.iter().any(|s| s.kind.pads_left());
        for (i, sp) in c.sources.iter().enumerate() {
            let Some(d) = sp.derived.as_deref() else {
                continue;
            };
            #[cfg(any(feature = "json", feature = "vtab"))]
            if sp.tvf != 0 {
                continue;
            }
            if sp.eqp.mat.as_slice() == b"\0" {
                continue;
            }
            if d.mat != 0 {
                if self.mats.iter().any(|m| *m == d.mat) {
                    continue;
                }
                self.mats.push(d.mat);
            }
            // The pin's `fromClauseTermCanBeCoroutine`.
            let co = d.mat == 0
                && !ltorj
                && if i == 0 {
                    n == 1 || !self.update_from
                } else {
                    !self.update_from
                        && c.sources.iter().take(i + 1).enumerate().all(|(j, s)| {
                            // (hipp's parser gives `,` and CROSS JOIN one
                            // kind: both count as the inner join `,` is.)
                            !matches!(s.kind, JoinKind::Left | JoinKind::Full)
                                && (j == i || s.derived.is_none())
                        })
                };
            let mut h = AVec::new();
            h.extend_from_slice(if co { b"CO-ROUTINE " } else { b"MATERIALIZE " });
            self.sub_name(&mut h, sp.eqp.mat.as_slice(), last_sel(d));
            let id = self.add(parent, &[h.as_slice()]);
            match d.rec {
                Some(r) => {
                    let mut s = sel_of_sub(d);
                    let split = r.split.min(s.cores.len());
                    let mut steps = Sel {
                        cores: AVec::new(),
                        comp_order: 0,
                        limit: None,
                        offset: None,
                    };
                    while s.cores.len() > split {
                        if let Some(c) = s.cores.pop() {
                            steps.cores.insert(0, c);
                        }
                    }
                    s.comp_order = 0;
                    let st = self.add(id, &[b"SETUP"]);
                    self.select(&s, st);
                    let rs = self.add(id, &[b"RECURSIVE STEP"]);
                    self.select(&steps, rs);
                }
                None => self.select_sub(d, id),
            }
        }
        let count_of = self.simple_count(c);
        // A join's WHERE terms with subqueries are shown after the loop
        // that makes them computable, where the pin codes them (v0.17,
        // `join_order`): per term, that loop's position.
        #[cfg(feature = "join_order")]
        let mut wterms: AVec<(&RExpr, usize)> = AVec::new();
        #[cfg(feature = "join_order")]
        if n > 1
            && let Some(f) = c.filter
        {
            let mut parts: AVec<&RExpr> = AVec::new();
            crate::xplan::conjuncts(f, &mut parts);
            for p in parts.iter() {
                let mut at = 0usize;
                for (j, sj) in c.sources.iter().enumerate() {
                    let band = crate::query::RefRange {
                        w: (sj.eqp.base, sj.eqp.base + sj.ncols),
                        s: (0, 0),
                    };
                    if crate::query::expr_refs_range(p, &band) {
                        at = j;
                    }
                }
                wterms.push((p, at));
            }
        }
        for (i, sp) in c.sources.iter().enumerate() {
            #[cfg(feature = "join_order")]
            if sp.eqp.omit {
                continue;
            }
            let mut line = AVec::new();
            self.loop_line(sp, &mut line, &mut probes);
            // The pin's count(*) loop names the table itself, never an
            // alias or schema (v0.16 review fix); `COUNT_MARK` keeps a
            // routed statement's qualifier off it (`requalify`).
            if let Some(t) = count_of {
                let mut shown = AVec::new();
                self.scan_name(sp, &mut shown);
                let tail = line
                    .as_slice()
                    .get(shown.len().saturating_add(5)..)
                    .unwrap_or(&[]);
                let mut l = copy_vec(b"SCAN ");
                l.extend_from_slice(handle_bytes(self.ca, t.name));
                l.extend_from_slice(tail);
                l.push(COUNT_MARK);
                line = l;
            }
            // A min() / max() query's loops all print SEARCH (v0.17,
            // `multi_index`; the pin's `WHERE_ORDERBY_MIN` / `_MAX`).
            #[cfg(feature = "multi_index")]
            if sp.eqp.minmax != 0 && line.as_slice().starts_with(b"SCAN ") {
                let mut l = copy_vec(b"SEARCH ");
                l.extend_from_slice(line.as_slice().get(5..).unwrap_or(&[]));
                let wr_pk = sp.derived.is_none()
                    && self
                        .cat
                        .tables
                        .iter()
                        .find(|t| t.root == sp.root)
                        .is_some_and(|t| !t.has_rowid())
                    && !l.as_slice().windows(7).any(|w| w == b" USING ");
                if wr_pk {
                    l.extend_from_slice(b" USING PRIMARY KEY");
                }
                line = l;
            }
            #[cfg(feature = "multi_index")]
            let multi = sp.access.as_deref().and_then(|a| a.or.as_deref());
            #[cfg(not(feature = "multi_index"))]
            let multi: Option<()> = None;
            if i > 0 && sp.kind.pads_right() && multi.is_none() {
                line.extend_from_slice(b" LEFT-JOIN");
            }
            // The Bloom filters built before this loop (v0.17,
            // `join_order`): its own, and those pulled down to it.
            #[cfg(feature = "join_order")]
            for (q, sq) in c.sources.iter().enumerate() {
                if sp.eqp.bloom_lines & (1u64 << q) != 0 {
                    let mut b = AVec::new();
                    self.bloom_line(sq, &mut b);
                    self.add(parent, &[b.as_slice()]);
                }
            }
            // An automatic index's Bloom filter (v0.17, `join_order`).
            #[cfg(feature = "join_order")]
            if let Some(ai) = sp.access.as_deref().and_then(|a| a.auto.as_deref())
                && ai.bloom
            {
                let mut b = copy_vec(b"BLOOM FILTER ON ");
                self.scan_name(sp, &mut b);
                b.push(b' ');
                self.auto_keys(sp, ai, &mut b);
                self.add(parent, &[b.as_slice()]);
            }
            let id = self.add(parent, &[line.as_slice()]);
            // A MULTI-INDEX OR loop's branches (v0.17, `multi_index`).
            #[cfg(feature = "multi_index")]
            if let Some(o) = multi {
                self.or_branches(sp, o, id, i > 0 && sp.kind.pads_right());
            }
            #[cfg(not(feature = "multi_index"))]
            let _ = (id, multi);
            #[cfg(feature = "expr_index")]
            if let Some(a) = sp.access.as_deref() {
                self.probing = true;
                for p in a.eq.iter() {
                    if let Some(e) = &p.sub {
                        self.exprs(core::slice::from_ref(e), parent, &[]);
                    }
                }
                self.probing = false;
            }
            if let Some(on) = &sp.on {
                self.exprs(core::slice::from_ref(on), parent, probes.as_slice());
            }
            #[cfg(feature = "join_order")]
            for (p, at) in wterms.iter() {
                if *at == i {
                    self.exprs(core::slice::from_ref(*p), parent, probes.as_slice());
                }
            }
        }
        for (i, sp) in c.sources.iter().enumerate() {
            if i > 0 && sp.kind.pads_left() {
                let mut name = AVec::new();
                self.scan_name(sp, &mut name);
                let r = self.add(parent, &[b"RIGHT-JOIN ", name.as_slice()]);
                self.add(r, &[b"SCAN ", name.as_slice()]);
            }
        }
        #[cfg(feature = "join_order")]
        let shown = !wterms.is_empty();
        #[cfg(not(feature = "join_order"))]
        let shown = false;
        if let Some(f) = c.filter
            && !shown
        {
            self.exprs(core::slice::from_ref(f), parent, probes.as_slice());
        }
        // (A join the pin's planner ordered reports what it delivers
        // on its first loop: v0.17, `join_order`.)
        #[cfg(feature = "join_order")]
        let first = c.sources.first().filter(|s| n == 1 || s.eqp.joined);
        #[cfg(not(feature = "join_order"))]
        let first = c.sources.first().filter(|_| n == 1);
        #[cfg(feature = "expr_index")]
        let acc = first.and_then(|s| s.access.as_deref());
        #[cfg(feature = "expr_index")]
        let delivered = acc.map_or(0, |a| a.eqp.0);
        #[cfg(not(feature = "expr_index"))]
        let delivered = 0usize;
        let delivered = delivered.max(self.natural(first));
        if !c.group_by.is_empty() {
            #[cfg(feature = "expr_index")]
            let grouped = acc.is_some_and(|a| a.groups);
            #[cfg(not(feature = "expr_index"))]
            let grouped = false;
            let grouped = grouped || self.natural(first) >= c.group_by.len();
            if !grouped {
                self.add(parent, &[b"USE TEMP B-TREE FOR GROUP BY"]);
            }
        }
        if let Some(h) = c.having {
            self.exprs(core::slice::from_ref(h), parent, &[]);
        }
        self.exprs(c.items, parent, &[]);
        if !c.group_by.is_empty() {
            self.agg_lines(aggs, parent);
        }
        // The pin turns a DISTINCT whose ORDER BY is its result list
        // (any directions) into a GROUP BY, which then delivers the
        // ORDER BY.
        let as_group = c.distinct
            && !c.has_agg
            && c.group_by.is_empty()
            && c.order_by.len() == c.items.len()
            && c.order_by
                .iter()
                .zip(c.items.iter())
                .all(|((o, _), i)| crate::query::same_expr(o, i));
        // DISTINCT: delivered by the loop when the planner was given
        // it (no ORDER BY, no aggregate) or found the loop brings equal
        // rows together, or redundant (a rowid among the result columns
        // of a lone table).
        // (An aggregate without GROUP BY is one row, but the pin
        // still opens its DISTINCT table: v0.17 fix.)
        if c.distinct {
            let given = c.order_by.is_empty() && !c.has_agg;
            let unique = n == 1 && c.items.iter().any(|e| self.is_rowid(c.sources, e));
            #[cfg(feature = "expr_index")]
            let adjacent = acc.is_some_and(|a| a.eqp.3);
            #[cfg(not(feature = "expr_index"))]
            let adjacent = false;
            let adjacent = adjacent || first.is_some_and(|s| s.eqp.nat_distinct);
            let done = if c.has_agg && c.group_by.is_empty() {
                false
            } else if as_group {
                delivered >= c.items.len()
            } else {
                unique || adjacent || (given && delivered >= c.items.len())
            };
            if !done {
                self.add(parent, &[b"USE TEMP B-TREE FOR DISTINCT"]);
            }
        }
        if own_order && !c.order_by.is_empty() && (!as_group || one_row_distinct(c)) {
            self.order_exprs(c, parent);
            // An aggregate without GROUP BY is one row; a GROUP BY
            // the ORDER BY repeats sorts once.
            let single = c.has_agg && c.group_by.is_empty();
            let same_group = !c.group_by.is_empty()
                && c.group_by.len() == c.order_by.len()
                && c.group_by
                    .iter()
                    .zip(c.order_by.iter())
                    .all(|(g, (o, _))| crate::query::same_expr(g, o));
            let k = c.order_by.len();
            let done = if c.group_by.is_empty() && (!c.distinct || !c.has_agg) {
                delivered.min(k)
            } else {
                0
            };
            // A DISTINCT read through a one-row rowid lookup still sorts
            // for its ORDER BY on the pin (its group and distinct
            // logic sees no delivered order).
            let one_row = as_group && one_row_distinct(c);
            if one_row {
                self.sorter(parent, k, 0);
            } else if !single && !same_group {
                self.sorter(parent, k, done);
            }
        }
    }

    /// A MULTI-INDEX OR loop's branches under its line `id` (v0.17,
    /// `multi_index`): `INDEX n`, the subqueries its loop evaluates
    /// (its IN probes first), then its loop (`LEFT-JOIN` as the
    /// source's).
    #[cfg(feature = "multi_index")]
    fn or_branches(&mut self, sp: &SubSource, o: &crate::xmulti::OrAccess, id: i64, left: bool) {
        for sub in o.subs.iter() {
            let mut h = copy_vec(b"INDEX ");
            num(&mut h, u64::from(sub.n), false);
            let ix = self.add(id, &[h.as_slice()]);
            let mut skip: AVec<usize> = AVec::new();
            if let Some(a) = &sub.acc {
                self.probing = true;
                for p in a.eq.iter() {
                    if let Some(e) = &p.sub {
                        self.exprs(core::slice::from_ref(e), ix, &[]);
                        if let RExpr::InSub(_, s, _) = e {
                            skip.push(s.id);
                        }
                    }
                }
                self.probing = false;
            }
            self.exprs(core::slice::from_ref(&sub.cond), ix, skip.as_slice());
            let mut l = copy_vec(b"SCAN ");
            self.scan_name(sp, &mut l);
            if let Some(a) = &sub.acc {
                self.access(sp, a, &mut l);
            }
            if left {
                l.extend_from_slice(b" LEFT-JOIN");
            }
            self.add(ix, &[l.as_slice()]);
        }
    }

    /// The order terms a lone table read in its own order delivers
    /// (the planner leaves it without an access path;
    /// `xplan::natural_sat`), or a virtual table that consumes them.
    fn natural(&self, first: Option<&SubSource>) -> usize {
        // A virtual table that consumes the ORDER BY: fts5's rank /
        // rowid order, generate_series's value order.
        #[cfg(feature = "vtab")]
        if let Some(v) = first.and_then(|s| s.vt.as_deref()) {
            let consumed = match v.tab.module {
                #[cfg(feature = "fts5")]
                crate::vtab::Module::Fts5 => v.idx_num & 3 != 0,
                crate::vtab::Module::Series => v.idx_num & 0x18 != 0,
                _ => false,
            };
            return if consumed { usize::MAX } else { 0 };
        }
        first.map_or(0, |s| s.eqp.natural)
    }

    /// Whether `e` is the rowid of the lone source.
    fn is_rowid(&self, srcs: &[SubSource], e: &RExpr) -> bool {
        let Some(s) = srcs.first() else { return false };
        if s.derived.is_some() {
            return false;
        }
        match e {
            RExpr::Rowid(_) => true,
            RExpr::Column(slot, ..) => s.rowid_alias.is_some_and(|a| a + s.eqp.base == *slot),
            _ => false,
        }
    }

    /// `name`, or `(subquery-n)` when it is empty.
    fn sub_name(&mut self, out: &mut AVec<u8>, name: &[u8], sel: u32) {
        if name.is_empty() {
            out.extend_from_slice(b"(subquery-");
            let n = self.shown(sel);
            num(out, u64::from(n), false);
            out.push(b')');
        } else {
            out.extend_from_slice(name);
        }
    }

    fn scan_name(&mut self, sp: &SubSource, out: &mut AVec<u8>) {
        let sel = sp.derived.as_deref().map_or(0, last_sel);
        self.sub_name(out, sp.eqp.scan.as_slice(), sel);
    }

    /// The loop over one FROM source.
    fn loop_line(&mut self, sp: &SubSource, out: &mut AVec<u8>, probes: &mut AVec<usize>) {
        out.extend_from_slice(b"SCAN ");
        self.scan_name(sp, out);
        #[cfg(feature = "vtab")]
        if let Some(v) = sp.vt.as_deref() {
            let hex = matches!(
                v.tab.module,
                crate::vtab::Module::Series | crate::vtab::Module::Dbstat
            );
            out.extend_from_slice(b" VIRTUAL TABLE INDEX ");
            if v.idx_num < 0 && !hex && v.idx_num != i32::MIN {
                out.push(b'-');
            }
            let mut i = v.idx_num;
            // hipp's fts5 order flags (1 rank, 2 rowid, 4 DESC) are the
            // pin's FTS5_BI_ORDER_RANK / _ROWID / _DESC (0x20, 0x40,
            // 0x80).
            #[cfg(feature = "fts5")]
            if v.tab.module == crate::vtab::Module::Fts5 {
                i = (i & 7) << 5;
            }
            let n = if hex {
                u64::from(i as u32)
            } else {
                u64::from(i.unsigned_abs())
            };
            num(out, n, hex);
            out.push(b':');
            out.extend_from_slice(v.idx_str.as_slice());
            return;
        }
        #[cfg(any(feature = "json", feature = "vtab"))]
        if sp.tvf != 0 {
            // json_each / json_tree: the JSON argument, and the path.
            let n = sp
                .derived
                .as_deref()
                .and_then(|d| d.values.as_slice().first())
                .map_or(1, |r| match r.len() {
                    // (No JSON argument: a call whose argument reads
                    // a source past a CROSS JOIN, v0.17.)
                    0 => 0,
                    1 => 1,
                    _ => 3,
                });
            out.extend_from_slice(b" VIRTUAL TABLE INDEX ");
            num(out, n, false);
            out.push(b':');
            return;
        }
        if sp.eqp.mat.as_slice() == b"\0" {
            // A table-valued pragma function: the pin's pragma module
            // sets no index number.
            if sp
                .eqp
                .scan
                .as_slice()
                .get(..7)
                .is_some_and(|p| eq_ci(p, b"pragma_"))
            {
                out.extend_from_slice(b" VIRTUAL TABLE INDEX 0:");
            }
            return;
        }
        if sp.derived.is_some() || sp.rec != 0 {
            return;
        }
        #[cfg(feature = "expr_index")]
        if let Some(a) = sp.access.as_deref() {
            for p in a.eq.iter() {
                if let Some(RExpr::InSub(_, s, _)) = &p.sub {
                    probes.push(s.id);
                }
            }
            self.access(sp, a, out);
        }
        let _ = probes;
    }

    /// `BLOOM FILTER ON x (a=? AND b=?)` for a lookup loop's filter
    /// (v0.17, `join_order`; `sqlite3WhereExplainBloomFilter`): the
    /// INTEGER PRIMARY KEY's name (or `rowid`), else the index's
    /// equality columns.
    #[cfg(feature = "join_order")]
    fn bloom_line(&mut self, sp: &SubSource, out: &mut AVec<u8>) {
        out.extend_from_slice(b"BLOOM FILTER ON ");
        self.scan_name(sp, out);
        out.extend_from_slice(b" (");
        let Some(a) = sp.access.as_deref() else {
            return;
        };
        let n = a.bloom.as_ref().map_or(0, |k| k.len());
        let t = self.cat.tables.iter().find(|t| t.root == sp.root);
        let ix = self
            .cat
            .indexes
            .iter()
            .find(|i| i.root == a.root && a.root != 0);
        match (t, ix) {
            (Some(t), Some(i)) => {
                let mut keys: AVec<AVec<u8>> = AVec::new();
                self.key_names(t, i, &mut keys);
                for (k, name) in keys.iter().take(n).enumerate() {
                    if k > 0 {
                        out.extend_from_slice(b" AND ");
                    }
                    out.extend_from_slice(name.as_slice());
                    out.extend_from_slice(b"=?");
                }
            }
            (t, _) => {
                match t.and_then(|t| t.rowid_alias.and_then(|c| t.columns.as_slice().get(c))) {
                    Some(c) => out.extend_from_slice(handle_bytes(self.ca, c.name)),
                    None => out.extend_from_slice(b"rowid"),
                }
                out.extend_from_slice(b"=?");
            }
        }
        out.push(b')');
    }

    /// An automatic index's key: `(c=? AND d=?)` (v0.17,
    /// `join_order`).
    #[cfg(feature = "join_order")]
    fn auto_keys(&self, sp: &SubSource, ai: &crate::xplan::AutoIdx, out: &mut AVec<u8>) {
        let t = self.cat.tables.iter().find(|t| t.root == sp.root);
        out.push(b'(');
        for (i, k) in ai.keys.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b" AND ");
            }
            if let Some(c) = t.and_then(|t| t.columns.as_slice().get(k.col)) {
                out.extend_from_slice(handle_bytes(self.ca, c.name));
            }
            out.extend_from_slice(b"=?");
        }
        out.push(b')');
    }

    /// The index part of a loop's line.
    #[cfg(feature = "expr_index")]
    fn access(&mut self, sp: &SubSource, a: &crate::xplan::IdxAccess, out: &mut AVec<u8>) {
        #[cfg(feature = "multi_index")]
        if a.or.is_some() {
            out.truncate(0);
            out.extend_from_slice(b"MULTI-INDEX OR");
            return;
        }
        #[cfg(feature = "join_order")]
        if let Some(ai) = a.auto.as_deref() {
            out.truncate(0);
            out.extend_from_slice(b"SEARCH ");
            self.scan_name(sp, out);
            out.extend_from_slice(if ai.partial {
                b" USING AUTOMATIC PARTIAL COVERING INDEX "
            } else {
                b" USING AUTOMATIC COVERING INDEX "
            });
            self.auto_keys(sp, ai, out);
            return;
        }
        let Some(t) = self.cat.tables.iter().find(|t| t.root == sp.root) else {
            return;
        };
        let constrained = !a.eq.is_empty() || a.lo.is_some() || a.hi.is_some() || a.eqp.2;
        let ix = self
            .cat
            .indexes
            .iter()
            .find(|i| i.root == a.root && a.root != 0);
        let mut keys: AVec<AVec<u8>> = AVec::new();
        match ix {
            None if t.has_rowid() => {
                if !constrained {
                    return;
                }
                out.truncate(0);
                out.extend_from_slice(b"SEARCH ");
                self.scan_name(sp, out);
                out.extend_from_slice(b" USING INTEGER PRIMARY KEY ");
                keys.push(copy_vec(b"rowid"));
                self.terms(a, keys.as_slice(), out);
                return;
            }
            Some(i) if !clustered(i) => {
                if constrained {
                    out.truncate(0);
                    out.extend_from_slice(b"SEARCH ");
                    self.scan_name(sp, out);
                }
                out.extend_from_slice(if a.eqp.1 {
                    b" USING COVERING INDEX "
                } else {
                    b" USING INDEX "
                });
                out.extend_from_slice(handle_bytes(self.ca, i.name));
                if !constrained {
                    return;
                }
                out.push(b' ');
                self.key_names(t, i, &mut keys);
            }
            _ => {
                // A WITHOUT ROWID table's own B-tree.
                if !constrained {
                    return;
                }
                let Some(pk) = self.pk_of(t) else {
                    return;
                };
                out.truncate(0);
                out.extend_from_slice(b"SEARCH ");
                self.scan_name(sp, out);
                out.extend_from_slice(b" USING PRIMARY KEY ");
                self.key_names(t, pk, &mut keys);
            }
        }
        self.terms(a, keys.as_slice(), out);
    }

    /// A WITHOUT ROWID table's clustered PRIMARY KEY index.
    #[cfg(feature = "expr_index")]
    fn pk_of(&self, t: &TableDef) -> Option<&'a crate::schema::IndexDef> {
        let name = handle_bytes(self.ca, t.name);
        self.cat
            .indexes
            .iter()
            .find(|i| clustered(i) && eq_ci(handle_bytes(self.ca, i.table), name))
    }

    /// An index's key columns as the pin names them: the column, or
    /// `<expr>`; then the rowid, or a WITHOUT ROWID table's primary
    /// key columns the index does not hold.
    #[cfg(feature = "expr_index")]
    fn key_names(&self, t: &TableDef, ix: &crate::schema::IndexDef, keys: &mut AVec<AVec<u8>>) {
        let col = |c: usize| {
            t.columns
                .as_slice()
                .get(c)
                .map_or(AVec::new(), |c| copy_vec(handle_bytes(self.ca, c.name)))
        };
        for (i, k) in ix.columns.iter().enumerate() {
            let expr = ix
                .ext
                .as_ref()
                .is_some_and(|x| x.exprs.as_slice().get(i).is_some_and(|e| e.is_some()));
            keys.push(match (k.col, t.rowid_alias) {
                (Some(c), _) => col(c),
                // The INTEGER PRIMARY KEY column (the rowid).
                (None, Some(a)) if !expr => col(a),
                (None, None) if !expr => copy_vec(b"rowid"),
                _ => copy_vec(b"<expr>"),
            });
        }
        if clustered(ix) {
            return;
        }
        if t.has_rowid() {
            keys.push(copy_vec(b"rowid"));
        } else if let Some(pk) = self.pk_of(t) {
            for k in pk.columns.iter() {
                if !ix.columns.iter().any(|x| x.col == k.col)
                    && let Some(c) = k.col
                {
                    keys.push(col(c));
                }
            }
        }
    }

    /// `(a=? AND b>? AND b<?)`.
    #[cfg(feature = "expr_index")]
    fn terms(&self, a: &crate::xplan::IdxAccess, keys: &[AVec<u8>], out: &mut AVec<u8>) {
        out.push(b'(');
        let mut first = true;
        let mut sep = |out: &mut AVec<u8>| {
            if !first {
                out.extend_from_slice(b" AND ");
            }
            first = false;
        };
        #[cfg(feature = "multi_index")]
        let skip = a.skip;
        #[cfg(not(feature = "multi_index"))]
        let skip = 0;
        let k = a.eq.len() + skip;
        for (i, key) in keys.iter().take(k).enumerate() {
            sep(out);
            // A skip-scan's columns (v0.17, `multi_index`).
            if i < skip {
                out.extend_from_slice(b"ANY(");
                out.extend_from_slice(key.as_slice());
                out.push(b')');
                continue;
            }
            out.extend_from_slice(key.as_slice());
            out.extend_from_slice(b"=?");
        }
        let rk = if a.range_rowid {
            keys.last()
        } else {
            keys.get(k)
        };
        if let Some(rk) = rk {
            if a.lo.is_some() || a.eqp.2 {
                sep(out);
                out.extend_from_slice(rk.as_slice());
                out.extend_from_slice(b">?");
            }
            if a.hi.is_some() {
                sep(out);
                out.extend_from_slice(rk.as_slice());
                out.extend_from_slice(b"<?");
            }
        }
        out.push(b')');
    }

    /// The subqueries of `es`, each where it is evaluated (once per
    /// statement when uncorrelated), skipping those in `skip`.
    fn exprs(&mut self, es: &[RExpr], parent: i64, skip: &[usize]) {
        let mut found: AVec<&RExpr> = AVec::new();
        for e in es.iter() {
            subs(e, &mut found);
        }
        for e in found.iter() {
            let (p, list) = match e {
                RExpr::SubScalar(p) | RExpr::SubExists(p) => (p, false),
                RExpr::InSub(_, p, _) => (p, true),
                _ => continue,
            };
            if skip.contains(&p.id) || (p.id != 0 && self.seen.iter().any(|x| *x == p.id)) {
                continue;
            }
            self.seen.push(p.id);
            let n = self.shown(last_sel(p));
            let mut d = AVec::new();
            if !p.uncorrelated {
                d.extend_from_slice(b"CORRELATED ");
            }
            d.extend_from_slice(if list {
                b"LIST SUBQUERY "
            } else {
                b"SCALAR SUBQUERY "
            });
            num(&mut d, u64::from(n), false);
            let id = self.add(parent, &[d.as_slice()]);
            let probing = core::mem::replace(&mut self.probing, false);
            self.select_sub(p, id);
            self.probing = probing;
            // The IN operator's Bloom filter over an uncorrelated
            // subquery's values, unless an index loop probes with
            // them (v0.17 fix; a compound's are not shown).
            if list && p.uncorrelated && !probing && p.compound.is_empty() {
                self.add(id, &[b"CREATE BLOOM FILTER"]);
            }
        }
    }

    /// The target loop of an UPDATE or DELETE.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write(
        &mut self,
        t: Option<&TableDef>,
        name: &[u8],
        #[cfg(feature = "expr_index")] access: Option<&crate::xplan::IdxAccess>,
        filter: Option<&RExpr>,
        from: &[SubSource],
        sets: &[RExpr],
        returning: &[RExpr],
    ) {
        let Some(t) = t else { return };
        let mut line = AVec::new();
        line.extend_from_slice(b"SCAN ");
        line.extend_from_slice(name);
        #[cfg(feature = "expr_index")]
        if let Some(a) = access {
            let sp = SubSource {
                root: t.root,
                ..placeholder(name)
            };
            self.access(&sp, a, &mut line);
        }
        #[cfg(not(feature = "expr_index"))]
        let _ = t;
        let id = self.add(0, &[line.as_slice()]);
        // A MULTI-INDEX OR loop's branches (v0.17, `multi_index`).
        #[cfg(feature = "multi_index")]
        if let Some(o) = access.and_then(|a| a.or.as_deref()) {
            let sp = SubSource {
                root: t.root,
                ..placeholder(name)
            };
            self.or_branches(&sp, o, id, false);
        }
        #[cfg(not(feature = "multi_index"))]
        let _ = id;
        #[cfg_attr(not(feature = "expr_index"), allow(unused_mut))]
        let mut probes: AVec<usize> = AVec::new();
        #[cfg(feature = "expr_index")]
        if let Some(a) = access {
            self.probing = true;
            for p in a.eq.iter() {
                if let Some(e) = &p.sub {
                    if let RExpr::InSub(_, s, _) = e {
                        probes.push(s.id);
                    }
                    self.exprs(core::slice::from_ref(e), 0, &[]);
                }
            }
            self.probing = false;
        }
        if !from.is_empty() {
            self.update_from = true;
            let c = Core {
                sources: from,
                items: &[],
                filter: None,
                group_by: &[],
                having: None,
                order_by: &[],
                distinct: false,
                has_agg: false,
                values: &[],
                sel: 0,
            };
            self.body(&c, 0, false, &c);
        }
        if let Some(f) = filter {
            self.exprs(core::slice::from_ref(f), 0, probes.as_slice());
        }
        self.exprs(sets, 0, &[]);
        self.exprs(returning, 0, &[]);
    }
}

/// A source with only a name, for an UPDATE's or DELETE's target.
#[cfg(feature = "expr_index")]
fn placeholder(name: &[u8]) -> SubSource {
    SubSource {
        root: 0,
        kind: JoinKind::Inner,
        on: None,
        rowid_alias: None,
        ncols: 0,
        affs: AVec::new(),
        defaults: AVec::new(),
        derived: None,
        gcols: None,
        rec: 0,
        #[cfg(any(feature = "json", feature = "vtab"))]
        tvf: 0,
        #[cfg(any(feature = "json", feature = "vtab"))]
        tvf_dead: AVec::new(),
        #[cfg(feature = "vtab")]
        vt: None,
        access: None,
        hint: crate::query::Hint::Any,
        temp: false,
        #[cfg(feature = "attach")]
        ext: 0,
        eqp: crate::query::EqpInfo {
            scan: copy_vec(name),
            ..Default::default()
        },
    }
}

/// The window classes of `es` (v0.16, `window`): per class, whether
/// it sorts (has PARTITION BY or ORDER BY terms).
#[cfg(feature = "window")]
fn win_classes(es: &[RExpr], out: &mut AVec<(usize, bool)>) {
    for e in es.iter() {
        if let RExpr::Win(call, _) = e
            && !call.copy
        {
            let sorts = call.npart > 0 || !call.order.is_empty();
            match out
                .as_mut_slice()
                .iter_mut()
                .find(|(k, _)| *k == call.class)
            {
                Some(x) => x.1 |= sorts,
                None => {
                    let at = out.iter().position(|(k, _)| *k > call.class);
                    match at {
                        Some(i) => out.insert(i, (call.class, sorts)),
                        None => out.push((call.class, sorts)),
                    }
                }
            }
        }
        let mut kids: AVec<&RExpr> = AVec::new();
        children(e, &mut kids);
        for k in kids.iter() {
            win_classes(core::slice::from_ref(*k), out);
        }
    }
}

/// The rows of the plan `plan` of statement `ast` (see the module
/// docs). `Plan::Temp` / `Plan::Att` are described by the database
/// that runs them (`Statement::eqp_rows`).
pub(super) fn describe<S: Storage>(
    w: &mut Walk<'_>,
    plan: &Plan,
    ast: &Ast,
    arena: &Arena,
    conn: &Connection<S>,
) {
    let ast = match ast {
        Ast::With { body, .. } => body.as_ref(),
        a => a,
    };
    let ca = &conn.catalog_arena;
    let table = |root: u32| conn.catalog.tables.iter().find(|t| t.root == root);
    // The target's name as the loop shows it: the alias, else the
    // table's name.
    let target = |root: u32, alias: Option<crate::arena::ABytes>| -> AVec<u8> {
        match (alias, table(root)) {
            (Some(a), _) => copy_vec(handle_bytes(arena, a)),
            (None, Some(t)) => copy_vec(handle_bytes(ca, t.name)),
            (None, None) => AVec::new(),
        }
    };
    match plan {
        Plan::Query(qp) => w.select_qp(qp, 0),
        Plan::Scan { access, .. } => {
            let name = match ast {
                Ast::Select(s) => s
                    .from
                    .as_slice()
                    .first()
                    .map_or(AVec::new(), |f| source_names(f, arena).0),
                _ => AVec::new(),
            };
            // The lookup a `WHERE col = literal` became (v0.16 review
            // fix: a secondary-index lookup, and the one a NULL or
            // non-numeric literal short-circuits, printed as SCAN).
            let (rowid, ix) = legacy_lookup(conn, ast, arena, access);
            if rowid {
                w.add(
                    0,
                    &[
                        b"SEARCH ",
                        name.as_slice(),
                        b" USING INTEGER PRIMARY KEY (rowid=?)",
                    ],
                );
            } else if let Some((ix, col)) = ix {
                w.add(
                    0,
                    &[
                        b"SEARCH ",
                        name.as_slice(),
                        b" USING INDEX ",
                        ix.as_slice(),
                        b" (",
                        col.as_slice(),
                        b"=?)",
                    ],
                );
            } else {
                w.add(0, &[b"SCAN ", name.as_slice()]);
            }
        }
        Plan::Update {
            #[cfg(feature = "expr_index")]
            access,
            root,
            sets,
            filter,
            from_sources,
            returning,
            #[cfg(feature = "join_order")]
            from_join,
            ..
        } => {
            // An UPDATE ... FROM the join planner ordered (v0.17
            // review fix): its loops as a SELECT's.
            #[cfg(feature = "join_order")]
            if let Some(j) = from_join.as_deref() {
                w.update_from = true;
                let c = Core {
                    sources: j.sources.as_slice(),
                    items: &[],
                    filter: None,
                    group_by: &[],
                    having: None,
                    order_by: &[],
                    distinct: false,
                    has_agg: false,
                    values: &[],
                    sel: 0,
                };
                w.body(&c, 0, false, &c);
                if let Some(f) = &j.filter {
                    w.exprs(core::slice::from_ref(f), 0, &[]);
                }
                w.exprs(j.sets.as_slice(), 0, &[]);
                w.exprs(returning.as_slice(), 0, &[]);
                return;
            }
            let alias = match ast {
                Ast::Update { alias, .. } => *alias,
                _ => None,
            };
            let mut es: AVec<RExpr> = AVec::new();
            for (_, e) in sets.iter() {
                es.push(e.clone());
            }
            w.write(
                table(*root),
                target(*root, alias).as_slice(),
                #[cfg(feature = "expr_index")]
                access.as_deref(),
                filter.as_ref(),
                from_sources.as_slice(),
                es.as_slice(),
                returning.as_slice(),
            );
        }
        Plan::Delete {
            #[cfg(feature = "expr_index")]
            access,
            root,
            filter,
            returning,
            ..
        } => {
            // The pin deletes in one pass, reading whole rows, unless
            // triggers (RETURNING is one), foreign keys or a WHERE
            // subquery make it collect the rowids first.
            #[cfg(feature = "expr_index")]
            let access = access.as_deref().map(|a| {
                #[cfg(feature = "triggers")]
                let trig = table(*root).is_some_and(|t| {
                    conn.catalog
                        .triggers_on(ca, handle_bytes(ca, t.name))
                        .any(|tr| tr.event == crate::sql::ast::TrigEvent::Delete)
                });
                #[cfg(not(feature = "triggers"))]
                let trig = false;
                let two = trig
                    || !returning.is_empty()
                    || table(*root).is_some_and(|t| conn.fk_required(arena, t, None))
                    || filter.as_ref().is_some_and(crate::query::has_sub);
                let mut a = a.clone();
                a.eqp.1 &= two;
                a
            });
            w.write(
                table(*root),
                target(*root, None).as_slice(),
                #[cfg(feature = "expr_index")]
                access.as_ref(),
                filter.as_ref(),
                &[],
                &[],
                returning.as_slice(),
            );
        }
        Plan::Insert {
            select,
            rows_r,
            rows_v,
            upsert,
            returning,
            ..
        } => {
            if let Some(p) = select {
                w.select_sub(p, 0);
            }
            if rows_v.len() > 1 {
                w.add_n(0, b"SCAN ", rows_v.len() as u32, b"-ROW VALUES CLAUSE");
            }
            if let Some(rows) = rows_r {
                if rows.len() > 1 {
                    values_line(w, 0, rows.as_slice());
                }
                for r in rows.iter() {
                    w.exprs(r.as_slice(), 0, &[]);
                }
            }
            // The DO UPDATE runs inside the constraint checks: its
            // WHERE, its SET, and the RETURNING it fires (the pin
            // codes the RETURNING again for the update) — v0.16
            // review fix: none of these were shown.
            if let Some(up) = upsert.as_ref().filter(|u| u.do_update) {
                if let Some(f) = &up.filter {
                    w.exprs(core::slice::from_ref(f), 0, &[]);
                }
                for (_, e) in up.sets.iter() {
                    w.exprs(core::slice::from_ref(e), 0, &[]);
                }
                let n = w.seen.len();
                w.exprs(returning.as_slice(), 0, &[]);
                w.seen.truncate(n);
            }
            w.exprs(returning.as_slice(), 0, &[]);
        }
        Plan::CreateTableAs {
            select: Some(p), ..
        } => w.select_sub(p, 0),
        // A write of a view through INSTEAD OF triggers: the rows are
        // computed, then each is handed to the triggers.
        #[cfg(feature = "triggers")]
        Plan::Instead(ip) => {
            let (name, insert) = match ast {
                Ast::Update { table, .. } | Ast::Delete { table, .. } => (*table, false),
                Ast::Insert { table, .. } => (*table, true),
                _ => (crate::arena::ABytes::EMPTY, true),
            };
            if !(insert && !ip.source.values.is_empty() && ip.source.compound.is_empty()) {
                w.select_sub(&ip.source, 0);
            }
            if !insert {
                w.add(0, &[b"SCAN ", handle_bytes(arena, name)]);
            }
        }
        _ => {}
    }
}

/// Run `text` (an EXPLAIN QUERY PLAN) on `c` and collect its rows.
pub(super) async fn collect<T: Storage>(
    c: &mut Connection<T>,
    text: &[u8],
) -> Result<EqpRows, T::Error> {
    let Ok(t) = core::str::from_utf8(text) else {
        return Err(Error::Unsupported);
    };
    let prep: Prep<'_, '_, T> = Box::pin(c.prepare(t));
    let st = prep.await?;
    // The rows as made, marks included (`COUNT_MARK`).
    match &st.plan {
        Plan::Explain(rows) => Ok(rows.clone()),
        _ => Err(Error::Unsupported),
    }
}

impl<S: Storage> Statement<'_, S> {
    /// The EXPLAIN QUERY PLAN rows of this statement's plan (v0.16,
    /// `explain`). A statement routed to the TEMP or an attached
    /// database is explained there.
    pub(super) async fn eqp_rows(&mut self) -> Result<EqpRows, S::Error> {
        #[cfg(feature = "params")]
        if matches!(self.plan, Plan::Deferred) {
            return Err(Error::Unsupported);
        }
        let text = |sql: &[u8]| {
            let mut t = copy_vec(b"EXPLAIN QUERY PLAN ");
            t.extend_from_slice(sql);
            t
        };
        if let Plan::Temp { sql, .. } = &self.plan {
            let t = text(sql.as_slice());
            let c = self.conn.temp_conn().await?;
            return collect(c, t.as_slice()).await.map_err(temp::mem_err);
        }
        #[cfg(feature = "attach")]
        if let Plan::Att(p) = &self.plan {
            let (db, t) = (p.db, text(p.sql.as_slice()));
            let tabs = self.conn.ext_tabs(p.ext.as_slice())?;
            let Some(a) = self.conn.attached.as_mut_slice().get_mut(db) else {
                return Err(Error::Schema {
                    kind: SchemaKind::NoSuchDatabase,
                });
            };
            *a.conn.ext.borrow_mut() = tabs;
            let fut: core::pin::Pin<
                Box<dyn core::future::Future<Output = Result<EqpRows, S::Error>> + '_>,
            > = Box::pin(collect(&mut a.conn, t.as_slice()));
            return fut.await;
        }
        let conn = &*self.conn;
        let walk = Walk::new(&conn.catalog, &conn.catalog_arena);
        let (plan, ast, arena) = (&self.plan, &self.ast, &self.arena);
        Ok(walk.run(&|w| describe(w, plan, ast, arena, conn)))
    }

    /// Step an EXPLAIN QUERY PLAN: its rows (id, parent, notused,
    /// detail), made when it was prepared.
    pub(super) async fn step_explain(&mut self) -> Result<Step, S::Error> {
        if !self.query_loaded {
            let Plan::Explain(rows) = &self.plan else {
                return Err(Error::Unsupported);
            };
            let mut out = AVec::new();
            for (id, parent, d) in rows.iter() {
                let mut r = AVec::new();
                r.push(OwnedValue::Integer(*id));
                r.push(OwnedValue::Integer(*parent));
                r.push(OwnedValue::Integer(0));
                let d = d.as_slice();
                let d = d.strip_suffix(&[COUNT_MARK]).unwrap_or(d);
                r.push(OwnedValue::Text(copy_vec(d)));
                out.push(r);
                self.query_rowids.push(None);
            }
            self.query_rows = out;
            self.query_loaded = true;
            self.query_pos = 0;
        }
        self.step_query().await
    }
}

/// An index lookup's (index name, key column name).
type IndexLine = (AVec<u8>, AVec<u8>);

/// The lookup a single-table scan's `WHERE col = literal` made
/// (`Connection::plan_legacy_select`): whether it seeks the rowid,
/// or the index (name, key column) it reads. An `Access::Empty`
/// lookup (a NULL, or a literal no rowid equals) is the seek the
/// literal's column took, decided before reading anything.
fn legacy_lookup<S: Storage>(
    conn: &Connection<S>,
    ast: &Ast,
    arena: &Arena,
    access: &Access,
) -> (bool, Option<IndexLine>) {
    let ca = &conn.catalog_arena;
    #[cfg_attr(
        not(all(feature = "indexes", not(feature = "expr_index"))),
        allow(unused_variables)
    )]
    let index_line = |root: u32| {
        let ix = conn.catalog.indexes.iter().find(|i| i.root == root)?;
        let t = conn.catalog.find(ca, handle_bytes(ca, ix.table))?;
        let c = ix.columns.as_slice().first()?.col?;
        let col = t.columns.as_slice().get(c)?;
        Some((
            copy_vec(handle_bytes(ca, ix.name)),
            copy_vec(handle_bytes(ca, col.name)),
        ))
    };
    match access {
        Access::FullScan => (false, None),
        Access::Rowid(_) => (true, None),
        #[cfg(all(feature = "indexes", not(feature = "expr_index")))]
        Access::Index { root, .. } => (false, index_line(*root)),
        Access::Empty => {
            let Ast::Select(s) = ast else {
                return (false, None);
            };
            let (Some(f), Some(Expr::Binary(BinOp::Eq, l, _))) =
                (s.from.as_slice().first(), s.filter.as_ref())
            else {
                return (false, None);
            };
            let (Expr::Column { name, .. }, Some(t)) = (
                l.as_ref(),
                conn.catalog.find(ca, handle_bytes(arena, f.table)),
            ) else {
                return (false, None);
            };
            match conn.resolve_target(t, handle_bytes(arena, *name)) {
                Ok(Target::Rowid) => (true, None),
                Ok(Target::Col(i)) if Some(i) == t.rowid_alias => (true, None),
                #[cfg(all(feature = "indexes", not(feature = "expr_index")))]
                Ok(Target::Col(i)) => match conn.plan_index_access(
                    t,
                    handle_bytes(arena, f.table),
                    i,
                    arena,
                    Literal::Integer(0),
                ) {
                    Some(Access::Index { root, .. }) => (false, index_line(root)),
                    _ => (false, None),
                },
                _ => (false, None),
            }
        }
    }
}

/// Put back the schema qualifiers of a statement routed to the TEMP
/// or an attached database (v0.16 review fix): that database runs
/// the statement's text with the qualifiers `schema.` cut (and
/// `sqlite_temp_schema` / `sqlite_temp_master` read as its
/// `sqlite_schema`), so its loops name the tables bare; the pin
/// prints `SCAN temp.t` for `FROM temp.t`, the schema as written.
/// `sql` is the statement as written; `temp`: the TEMP route.
pub(super) fn requalify<E>(
    rows: &mut EqpRows,
    sql: &[u8],
    schema: &[u8],
    temp: bool,
    arena: &Arena,
) {
    use crate::sql::lexer::Tok;
    let Ok(toks) = crate::sql::parser::tokens::<E>(sql, arena) else {
        return;
    };
    let t = toks.as_slice();
    let name = |i: usize| match t.get(i).map(|x| x.tok) {
        Some(Tok::Ident(h, _)) | Some(Tok::Text(h)) => Some(handle_bytes(arena, h)),
        _ => None,
    };
    let is_dot = |i: usize| t.get(i).is_some_and(|x| x.tok == Tok::Dot);
    // (the name the routed database prints, the pin's).
    let mut map: AVec<(AVec<u8>, AVec<u8>)> = AVec::new();
    for i in 0..t.len() {
        let Some(n) = name(i) else { continue };
        if is_dot(i + 1) {
            continue;
        }
        let shown: &[u8] = if temp && super::temp::is_temp_schema_name(n) {
            b"sqlite_schema"
        } else {
            n
        };
        let q = i
            .checked_sub(2)
            .filter(|_| i.checked_sub(1).is_some_and(is_dot))
            .and_then(|k| name(k).filter(|q| eq_ci(q, schema)).map(|q| (k, q)));
        let mut pin = AVec::new();
        match q {
            Some((k, q)) if k.checked_sub(1).is_none_or(|d| !is_dot(d)) => {
                pin.extend_from_slice(q);
                pin.push(b'.');
            }
            Some(_) => continue,
            None if temp && shown != n => {}
            None => continue,
        }
        pin.extend_from_slice(n);
        if !map.iter().any(|(s, _)| s.as_slice() == shown) {
            map.push((copy_vec(shown), pin));
        }
    }
    for (_, _, d) in rows.as_mut_slice().iter_mut() {
        if d.as_slice().last() == Some(&COUNT_MARK) {
            continue;
        }
        let at = if d.as_slice().starts_with(b"SCAN ") {
            5
        } else if d.as_slice().starts_with(b"SEARCH ") {
            7
        } else {
            continue;
        };
        let rest = d.as_slice().get(at..).unwrap_or(&[]);
        let Some((shown, pin)) = map.iter().find(|(s, _)| {
            rest.starts_with(s.as_slice()) && rest.get(s.len()).is_none_or(|c| *c == b' ')
        }) else {
            continue;
        };
        let mut out = AVec::new();
        out.extend_from_slice(d.as_slice().get(..at).unwrap_or(&[]));
        out.extend_from_slice(pin.as_slice());
        out.extend_from_slice(rest.get(shown.len()..).unwrap_or(&[]));
        *d = out;
    }
}
