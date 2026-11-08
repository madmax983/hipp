//! Executed views and common table expressions (v0.16, `v16`).
//!
//! Before a statement is planned, every FROM source that names a
//! view or a common table expression in scope is rewritten into a
//! derived table holding a copy of its SELECT (`Connection::
//! expand_ast`), so the rest of the planner and the executor see
//! nothing new: a view or CTE is read wherever a FROM subquery can
//! be — joins (NATURAL / USING included), subqueries, `x IN name`,
//! compound members, UPDATE ... FROM, INSERT ... SELECT, CREATE
//! TABLE ... AS. The rewrite follows the pin's name resolution
//! (`selectExpander` / `withExpand`):
//!
//! - A name with no schema qualifier is looked up in the visible
//!   WITH clauses first, innermost first; every CTE of one clause
//!   is visible to the others (forward references work). A CTE's
//!   body sees the clause it was defined in and the clauses around
//!   that, never one written inside the query that uses it.
//! - Otherwise a table, then a view, of the main schema. A view's
//!   body sees no CTE of the query that reads it.
//! - A CTE that is a UNION / UNION ALL compound whose rightmost
//!   SELECTs (those joined by the rightmost operator) name it in
//!   their own FROM clause is recursive: those SELECTs are the
//!   recursive steps, the cores before them the setup. Any other
//!   self-reference is "circular reference"; a second reference in
//!   one recursive step, or one in its subqueries, is "multiple
//!   recursive references". A view that reads itself is
//!   "circularly defined". Each error is raised only when the view
//!   or CTE is used, as on the pin.
//!
//! The recursive executor (`Statement::exec_recursive`) runs the
//! pin's queue: the setup rows enter the queue; each row taken out
//! of it is output (subject to the CTE's OFFSET / LIMIT) and then
//! fed to every recursive step as the one row of the recursive
//! table, whose result rows join the queue. The queue is FIFO
//! unless the CTE has an ORDER BY, which makes it a priority queue
//! (ties in arrival order); UNION drops every row seen before.

use super::*;
use crate::arena::ABytes;
use crate::error::ParseKind;
use crate::sql::{
    CompoundOp, Cte, CteUse, FromSource, JoinKind, M10d, RecMark, UpsertAction, With,
};

/// How many SELECTs the expansion of one statement may copy out of
/// view and CTE bodies before a CTE reference is refused (the pin's
/// "VIEWs and/or subqueries nested too deep", `NestedTooDeep`:
/// probed, a chain of 999 CTEs reads and 1000 do not; the copies of
/// views read before count as well). It also bounds the work of a
/// CTE whose references double at each level.
const MAX_COPIES: usize = 1000;

/// What a reference to a CTE means while that CTE's own body is
/// being expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// Not being expanded: a reference expands it.
    Free,
    /// Its setup (or its whole body): "circular reference".
    Circular,
    /// Its recursive steps: "multiple recursive references".
    MultiRec,
}

/// One WITH clause on the expansion stack.
struct Frame {
    ctes: AVec<Cte>,
    busy: AVec<Busy>,
    /// Each CTE's materialization id (`CteUse::mat`).
    ids: AVec<u32>,
}

/// The SELECT a FROM source is read by, as the pin's query
/// flattener sees it (`flattenSubquery`'s restrictions on the
/// outer query; see `Expander::flattens`).
#[derive(Debug, Clone, Copy, Default)]
struct Outer {
    /// How many sources its FROM clause has.
    nsrc: usize,
    filter: bool,
    limit: bool,
    order: bool,
    /// Aggregate (an aggregate call, GROUP BY or HAVING).
    agg: bool,
    distinct: bool,
    /// A member of a compound SELECT.
    compound: bool,
    /// Its result list calls a function or holds a subquery (the
    /// pin's `SF_ComplexResult`).
    complex: bool,
    /// Its result list or ORDER BY calls a window function (the
    /// flattener's restriction 25; v0.17, `flatten`).
    #[cfg(feature = "flatten")]
    win: bool,
}

/// The expansion walk (see the module docs).
struct Expander<'c> {
    catalog: &'c Catalog,
    ca: &'c Arena,
    arena: &'c Arena,
    /// Every WITH clause entered so far (indices are stable while
    /// a frame is live: frames are popped in reverse order).
    frames: AVec<Frame>,
    /// The frames visible at this point, innermost last.
    vis: AVec<usize>,
    /// The views being expanded (cycle detection).
    views: AVec<AVec<u8>>,
    /// The next recursive-CTE id (`RecMark`); 0 is never used.
    next_id: u32,
    /// Materialization ids (`CteUse::mat`, index + 1): each CTE's
    /// hint and how many references expanded it, or a group of
    /// one view's references sharing one materialization.
    mats: AVec<(M10d, u32)>,
    /// How deep view and CTE bodies nest at this point.
    nest: usize,
    /// `Config::max_nesting`.
    max_nest: usize,
    /// The connection's parser limits (`Config::limits`), for view
    /// bodies re-parsed here.
    lim: crate::sql::parser::Limits,
    /// SELECTs copied out of view and CTE bodies so far.
    copies: usize,
    /// The pin's name-resolution depth at this point
    /// (`Parse.nHeight`, v0.16 review fix): the heights of the
    /// expressions the select being walked sits in, summed over
    /// every enclosing select, view and CTE reference
    /// (`SelectStmt::rctx`). A view or CTE body read here is
    /// resolved that deep.
    ctx: usize,
    /// The second walk: every expanded source's materialization
    /// id is settled, nothing is expanded.
    fixing: bool,
    /// The select numbers given to the cores of view copies so far
    /// (v0.16, `explain`; `SelectStmt::sel`, `eqp::VIEW_SEL`).
    #[cfg(feature = "explain")]
    view_sel: u32,
    /// The SELECT about to be walked is a scalar or EXISTS
    /// subquery, which the pin gives a LIMIT of its own before
    /// flattening.
    sub_limit: bool,
    /// Walking a RETURNING clause: the pin runs it as a trigger
    /// program after each row's change, where every FROM subquery
    /// (view, CTE) is materialized once for the whole statement
    /// (probed: `RETURNING (SELECT group_concat(x) FROM (SELECT b x
    /// FROM t))` repeats the first row's value while `(SELECT
    /// group_concat(b) FROM t)` follows the update).
    returning: bool,
    /// Walking the body of a view or trigger stored in main (v0.16
    /// review fix): its unqualified table names are main's, never
    /// a TEMP table's (the pin's `sqlite3FixSrcList`), so a temp
    /// table neither shadows them nor is read for a missing one.
    fix_main: bool,
    /// Walking the SELECT of an `x IN (SELECT ...)` (v0.17 review
    /// fix, `flatten`; see `cte`).
    #[cfg(feature = "flatten")]
    in_rhs: bool,
}

fn schema_err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

impl<S: Storage> Connection<S> {
    fn expander<'c>(&'c self, arena: &'c Arena) -> Expander<'c> {
        Expander {
            catalog: &self.catalog,
            ca: &self.catalog_arena,
            arena,
            frames: AVec::new(),
            vis: AVec::new(),
            views: AVec::new(),
            next_id: 1,
            mats: AVec::new(),
            nest: 0,
            max_nest: self.cfg.max_nesting,
            lim: self.cfg.limits(),
            copies: 0,
            ctx: 0,
            fixing: false,
            #[cfg(feature = "explain")]
            view_sel: 0,
            sub_limit: false,
            returning: false,
            #[cfg(feature = "triggers")]
            fix_main: self.trig.fix_main.get(),
            #[cfg(not(feature = "triggers"))]
            fix_main: false,
            #[cfg(feature = "flatten")]
            in_rhs: false,
        }
    }

    /// Expand every view and CTE reference in `ast` into a derived
    /// table (v0.16, `v16`; see the module docs). `None` when the
    /// statement holds nothing to expand (it plans as written).
    /// INSERT into a view is refused here (`IsView`).
    pub(super) fn expand_ast(&self, arena: &Arena, ast: &Ast) -> Result<Option<Ast>, S::Error> {
        let mut x = self.expander(arena);
        match ast {
            Ast::Select(_)
            | Ast::Insert { .. }
            | Ast::Update { .. }
            | Ast::Delete { .. }
            | Ast::With { .. }
            | Ast::CreateTable {
                as_select: Some(_), ..
            } => {
                let mut out = ast.clone();
                x.ast::<S::Error>(&mut out)?;
                x.fixing = true;
                x.ast::<S::Error>(&mut out)?;
                Ok(Some(out))
            }
            _ => Ok(None),
        }
    }

    /// A view's SELECT with every view and CTE it reads expanded
    /// (v0.16, `v16`), and its column-name list (empty when none
    /// was written), parsed into `arena`. The shape PRAGMA
    /// table_info and the CLI's `.schema` comment are derived from.
    pub(super) fn expand_view(
        &self,
        arena: &Arena,
        v: &crate::schema::ViewDef,
    ) -> Result<(SelectStmt, AVec<ABytes>), S::Error> {
        let mut x = self.expander(arena);
        let mut f = FromSource {
            table: arena.alloc_copy_handle(handle_bytes(&self.catalog_arena, v.name))?,
            alias: None,
            kind: JoinKind::Inner,
            on: None,
            derived: None,
            natural: false,
            using: AVec::new(),
            args: None,
            qualified: true,
            in_temp: false,
            #[cfg(feature = "attach")]
            schema: None,
            cte: None,
            indexed: None,
            #[cfg(feature = "flatten")]
            flat: false,
        };
        x.view::<S::Error>(&mut f, v)?;
        let cols = f.cte.map(|c| c.columns).unwrap_or_default();
        match f.derived {
            Some(mut d) => {
                x.fixing = true;
                x.select::<S::Error>(&mut d)?;
                Ok((*d, cols))
            }
            None => Err(Error::Unsupported),
        }
    }

    /// A view's SELECT, parsed by the caller (with its tokens),
    /// expanded as `expand_view` expands it (v0.16, ALTER TABLE).
    pub(super) fn expand_parsed_view(
        &self,
        arena: &Arena,
        name: &[u8],
        mut select: SelectStmt,
    ) -> Result<SelectStmt, S::Error> {
        let mut x = self.expander(arena);
        x.read_body::<S::Error>(select.rdepth)?;
        // A stored view's names are its database's (`fix_main`).
        x.fix_main = true;
        x.views.push(copy_vec(name));
        x.select::<S::Error>(&mut select)?;
        x.fixing = true;
        x.select::<S::Error>(&mut select)?;
        Ok(select)
    }

    /// The column names of view `name` as the pin's shell lists
    /// them in a `.schema` comment and PRAGMA table_info (v0.16,
    /// `v16`): the view's SELECT is expanded and planned; its
    /// column list (of any length), else its result names, made
    /// unique the pin's way (`id`, `id:1`, ...). `None` when the
    /// view does not exist or cannot be planned (a missing table,
    /// a cycle, ...) — the pin's shell then prints no comment.
    pub fn view_columns(&self, name: &[u8]) -> Option<AVec<AVec<u8>>> {
        let v = self.catalog.find_view(&self.catalog_arena, name)?;
        let arena = Arena::new(Scope::Statement, self.cfg.stmt_init, self.cfg.stmt_max);
        self.view_names(&arena, v).ok()
    }

    /// `view_columns` with the planner's error kept (PRAGMA
    /// table_info reports it, as the pin does).
    pub(super) fn view_names(
        &self,
        arena: &Arena,
        v: &crate::schema::ViewDef,
    ) -> Result<AVec<AVec<u8>>, S::Error> {
        let ckpt = arena.checkpoint();
        let r = (|| -> Result<AVec<AVec<u8>>, S::Error> {
            let (select, cols) = self.expand_view(arena, v)?;
            let plan = self.plan_subquery(&select, arena, &[])?;
            // A column list names the columns whatever the SELECT's
            // width (the pin checks that only when the view is read).
            let mut names = if cols.is_empty() {
                self.result_col_names(&plan, (0, 0))?
            } else {
                let mut n = AVec::new();
                for c in cols.iter() {
                    n.push(copy_vec(handle_bytes(arena, *c)));
                }
                n
            };
            crate::rules::unique_names::<S::Error>(names.as_mut_slice())?;
            Ok(names)
        })();
        arena.rewind(ckpt);
        r
    }
}

impl Expander<'_> {
    fn ast<E>(&mut self, ast: &mut Ast) -> Result<(), E> {
        match ast {
            Ast::Select(s) => self.select::<E>(s)?,
            Ast::Insert {
                table,
                rows,
                select,
                upsert,
                returning,
                ..
            } => {
                let n = handle_bytes(self.arena, *table);
                if self.catalog.find(self.ca, n).is_none()
                    && self.catalog.find_view(self.ca, n).is_some()
                {
                    return Err(schema_err(SchemaKind::IsView));
                }
                for row in rows.as_mut_slice() {
                    for e in row.as_mut_slice() {
                        self.expr::<E>(e)?;
                    }
                }
                if let Some(s) = select {
                    self.select::<E>(s)?;
                }
                if let Some(u) = upsert {
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
                }
                self.returning_items::<E>(returning.as_mut_slice())?;
            }
            Ast::Update {
                assignments,
                from,
                filter,
                returning,
                ..
            } => {
                for (_, e) in assignments.as_mut_slice() {
                    self.expr::<E>(e)?;
                }
                // UPDATE ... FROM joins the target to the sources.
                let o = Outer {
                    nsrc: from.len() + 1,
                    filter: filter.is_some(),
                    ..Outer::default()
                };
                self.sources::<E>(from.as_mut_slice(), &o)?;
                if let Some(f) = filter {
                    self.expr::<E>(f)?;
                }
                self.returning_items::<E>(returning.as_mut_slice())?;
            }
            Ast::Delete {
                filter, returning, ..
            } => {
                if let Some(f) = filter {
                    self.expr::<E>(f)?;
                }
                self.returning_items::<E>(returning.as_mut_slice())?;
            }
            Ast::CreateTable {
                as_select: Some(s), ..
            } => self.select::<E>(s)?,
            Ast::With { with, body } => {
                let w = core::mem::replace(&mut **with, With { ctes: AVec::new() });
                self.push_frame::<E>(w)?;
                let r = self.ast::<E>(body);
                self.pop_frame();
                r?;
                // The planner sees the statement itself.
                let inner = core::mem::replace(&mut **body, Ast::Begin);
                *ast = inner;
            }
            _ => {}
        }
        Ok(())
    }

    /// Enter a WITH clause (the pin's "duplicate WITH table name"
    /// is checked here, used or not).
    fn push_frame<E>(&mut self, w: With) -> Result<(), E> {
        let ctes = w.ctes;
        for (i, c) in ctes.iter().enumerate() {
            let n = handle_bytes(self.arena, c.name);
            if ctes
                .iter()
                .skip(i + 1)
                .any(|d| eq_ci(handle_bytes(self.arena, d.name), n))
            {
                return Err(schema_err(SchemaKind::DuplicateCteName));
            }
        }
        let mut busy = AVec::new();
        let mut ids = AVec::new();
        for c in ctes.iter() {
            busy.push(Busy::Free);
            ids.push(self.new_mat(c.hint, 0));
        }
        self.vis.push(self.frames.len());
        self.frames.push(Frame { ctes, busy, ids });
        Ok(())
    }

    fn pop_frame(&mut self) {
        self.vis.pop();
        self.frames.pop();
    }

    /// A fresh materialization id.
    fn new_mat(&mut self, hint: M10d, uses: u32) -> u32 {
        self.mats.push((hint, uses));
        u32::try_from(self.mats.len()).unwrap_or(0)
    }

    /// Enter one more nested view or CTE body (`Config::
    /// max_nesting`; the pin's "VIEWs and/or subqueries nested too
    /// deep" at 1000 nested views).
    fn enter<E>(&mut self) -> Result<(), E> {
        if self.nest + 1 >= self.max_nest.max(1) {
            return Err(schema_err(SchemaKind::NestedTooDeep));
        }
        self.nest += 1;
        Ok(())
    }

    fn select<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        let pushed = match s.with.take() {
            Some(w) => {
                self.push_frame::<E>(*w)?;
                true
            }
            None => false,
        };
        let ctx = self.ctx;
        self.ctx = ctx.saturating_add(s.rctx as usize);
        let r = self.select_body::<E>(s);
        self.ctx = ctx;
        if pushed {
            self.pop_frame();
        }
        r
    }

    /// A view or CTE body of resolution depth `rdepth` read here
    /// (v0.16 review fix): the pin resolves it inside the reader's
    /// expressions, so the depths add up — a chain of views or CTEs
    /// each reading the last inside a scalar subquery is refused
    /// once the sum passes the limit ("Expression tree is too
    /// large"), however small each body is on its own.
    fn read_body<E>(&self, rdepth: u32) -> Result<(), E> {
        if self.ctx.saturating_add(rdepth as usize) > self.lim.expr_depth {
            return Err(Error::Parse {
                offset: 0,
                kind: ParseKind::ExprTooDeep,
            });
        }
        Ok(())
    }

    /// The pin's width limits that it checks when it resolves a
    /// select (v0.16 review fix): a CTE or CREATE VIEW body is not
    /// checked when it is parsed (an unused CTE never), so the
    /// copies the expansion makes are checked here.
    fn widths<E>(s: &SelectStmt) -> Result<(), E> {
        let max = crate::sql::parser::MAX_COLUMN;
        let kind = if s.items.len() > max || s.values.iter().any(|r| r.len() > max) {
            ParseKind::TooManyColumns
        } else if s.group_by.len() > max || s.order_by.len() > max {
            ParseKind::TooManyTerms
        } else {
            return Ok(());
        };
        Err(Error::Parse { offset: 0, kind })
    }

    fn select_body<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        let implicit = core::mem::take(&mut self.sub_limit);
        let n = s.compound.len();
        let mut head = outer_of(s, n == 0, n > 0, self.arena);
        head.limit |= implicit && n == 0;
        self.core::<E>(s, &head)?;
        let (order, limit) = (!s.order_by.is_empty(), s.limit.is_some() || implicit);
        for (k, (_, c)) in s.compound.as_mut_slice().iter_mut().enumerate() {
            let mut o = outer_of(c, false, true, self.arena);
            // The compound's ORDER BY / LIMIT belong to its last
            // SELECT, as in the pin's parse tree.
            if k + 1 == n {
                o.order = order;
                o.limit = limit;
            }
            self.core::<E>(c, &o)?;
        }
        self.tail::<E>(s)
    }

    /// A select statement's ORDER BY / LIMIT / OFFSET.
    fn tail<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        for (e, _) in s.order_by.as_mut_slice() {
            self.expr::<E>(e)?;
        }
        if let Some(e) = &mut s.limit {
            self.expr::<E>(e)?;
        }
        if let Some(e) = &mut s.offset {
            self.expr::<E>(e)?;
        }
        Ok(())
    }

    /// One core's own clauses.
    fn core<E>(&mut self, s: &mut SelectStmt, o: &Outer) -> Result<(), E> {
        if self.fixing {
            self.share_views(s, o);
        } else {
            Self::widths::<E>(s)?;
            // A view copy's cores are numbered afresh, in the order
            // the pin copies them (v0.16, `explain`).
            #[cfg(feature = "explain")]
            if !self.views.is_empty() {
                self.view_sel = self.view_sel.saturating_add(1);
                s.sel = super::eqp::VIEW_SEL.saturating_add(self.view_sel);
            }
        }
        self.items::<E>(s.items.as_mut_slice())?;
        self.sources::<E>(s.from.as_mut_slice(), o)?;
        if let Some(f) = &mut s.filter {
            self.expr::<E>(f)?;
        }
        for g in s.group_by.as_mut_slice() {
            self.expr::<E>(g)?;
        }
        if let Some(h) = &mut s.having {
            self.expr::<E>(h)?;
        }
        // The WINDOW clause's expressions (v0.16, `window`).
        #[cfg(feature = "window")]
        for (_, w) in s.windows.as_mut_slice() {
            let mut r = Ok(());
            w.exprs_mut(&mut |x| {
                if r.is_ok() {
                    r = self.expr::<E>(x);
                }
            });
            r?;
        }
        for row in s.values.as_mut_slice() {
            for e in row.as_mut_slice() {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }

    /// A RETURNING clause's items (see `Expander::returning`).
    fn returning_items<E>(&mut self, items: &mut [SelectItem]) -> Result<(), E> {
        let was = core::mem::replace(&mut self.returning, true);
        let r = self.items::<E>(items);
        self.returning = was;
        r
    }

    fn items<E>(&mut self, items: &mut [SelectItem]) -> Result<(), E> {
        for it in items {
            if let SelectItem::Expr(e, _, _) = it {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }

    fn sources<E>(&mut self, from: &mut [FromSource], o: &Outer) -> Result<(), E> {
        let n = from.len();
        // v0.17 (`flatten`): which sources hipp's flattener may
        // splice next to (a table of main or a FROM subquery; no
        // virtual table, table-valued function or RIGHT / FULL join).
        #[cfg(feature = "flatten")]
        let plain: AVec<bool> = {
            let rf = from
                .iter()
                .any(|f| matches!(f.kind, JoinKind::Right | JoinKind::Full));
            let mut v = AVec::new();
            for f in from.iter() {
                v.push(!rf && self.plain_src(f));
            }
            v
        };
        for i in 0..n {
            // The next source's join (the pin's co-routine test).
            let next = from.get(i + 1).map(|f| f.kind);
            if let Some(f) = from.get_mut(i) {
                self.source::<E>(f, o, i, next)?;
                if let Some(on) = &mut f.on {
                    self.expr::<E>(on)?;
                }
            }
        }
        // The flattener's choice, once every source's
        // materialization is settled (v0.17, `flatten`).
        #[cfg(feature = "flatten")]
        if self.fixing {
            let ok = plain.iter().all(|p| *p);
            for i in 0..n {
                let next = from.get(i + 1).map(|f| f.kind);
                let flat = ok && from.get(i).is_some_and(|f| self.flat_ok(f, o, i, next));
                if let Some(f) = from.get_mut(i) {
                    f.flat = flat;
                }
            }
        }
        Ok(())
    }

    /// A FROM source hipp's flattener may splice into, or out of, its
    /// reader (v0.17, `flatten`): a FROM subquery (an expanded view or
    /// CTE included, not a recursive reference), or a table of main
    /// that is no virtual table.
    #[cfg(feature = "flatten")]
    fn plain_src(&self, f: &FromSource) -> bool {
        if f.cte
            .as_ref()
            .is_some_and(|c| matches!(c.rec, RecMark::Ref { .. }))
        {
            return false;
        }
        if f.derived.is_some() {
            return true;
        }
        #[cfg(feature = "attach")]
        if f.schema.is_some() {
            return false;
        }
        let n = handle_bytes(self.arena, f.table);
        #[cfg(feature = "vtab")]
        if self.catalog.find_vtab(self.ca, n).is_some() {
            return false;
        }
        f.args.is_none() && !f.in_temp && self.catalog.find(self.ca, n).is_some()
    }

    /// Whether the pin's flattener merges FROM subquery `f` (source
    /// `idx` of a reader shaped `o`, the next source's join `next`)
    /// into its reader, as far as hipp's flattener follows it (v0.17,
    /// `flatten`): `flattens` and the restrictions it leaves to the
    /// planner — no shared materialization, MATERIALIZED or recursive
    /// CTE (28, 22), no window function on either side (25), no
    /// compound reader. hipp's own limits: no compound body, no
    /// top-level COLLATE result column (the pin's substituted column
    /// keeps an implicit collation hipp's expressions cannot carry),
    /// only tables of main and FROM subqueries in the body, and a
    /// LEFT JOIN's right side only as one table with a rowid (whose
    /// NULL row guards the substituted expressions).
    #[cfg(feature = "flatten")]
    fn flat_ok(&self, f: &FromSource, o: &Outer, idx: usize, next: Option<JoinKind>) -> bool {
        if self.returning || o.win || o.compound {
            return false;
        }
        let Some(d) = f.derived.as_deref() else {
            return false;
        };
        if let Some(c) = &f.cte
            && (c.mat != 0 || !matches!(c.rec, RecMark::None))
        {
            return false;
        }
        if !d.compound.is_empty() || !d.values.is_empty() || core_win(d) {
            return false;
        }
        if d.items
            .iter()
            .any(|it| matches!(it, SelectItem::Expr(Expr::Collate(..), ..)))
            || d.from
                .iter()
                .any(|g| matches!(g.kind, JoinKind::Right | JoinKind::Full) || !self.plain_src(g))
        {
            return false;
        }
        if f.kind == JoinKind::Left {
            let Some(g) = d.from.as_slice().first() else {
                return false;
            };
            let t = self
                .catalog
                .find(self.ca, handle_bytes(self.arena, g.table));
            if g.derived.is_some() || !t.is_some_and(|t| t.has_rowid()) {
                return false;
            }
        }
        flattens(d, o, f.kind, idx, next, self.arena)
    }

    /// The CTE a source names, as (frame, index), searching the
    /// visible WITH clauses innermost first.
    fn find_cte(&self, name: &[u8]) -> Option<(usize, usize)> {
        for &fi in self.vis.iter().rev() {
            let frame = self.frames.as_slice().get(fi)?;
            if let Some(ci) = frame
                .ctes
                .iter()
                .position(|c| eq_ci(handle_bytes(self.arena, c.name), name))
            {
                return Some((fi, ci));
            }
        }
        None
    }

    fn source<E>(
        &mut self,
        f: &mut FromSource,
        o: &Outer,
        idx: usize,
        next: Option<JoinKind>,
    ) -> Result<(), E> {
        if self.fixing {
            if let Some(c) = &mut f.cte
                && c.mat != 0
            {
                // A CTE's references share its materialization
                // when it is MATERIALIZED, or has no hint and was
                // read more than once (the pin's `CteUse`).
                let (hint, uses) = self
                    .mats
                    .as_slice()
                    .get((c.mat as usize).wrapping_sub(1))
                    .copied()
                    .unwrap_or((M10d::No, 0));
                // A NOT MATERIALIZED one read twice is shared by the
                // references the pin materializes (v0.17 review fix,
                // `flatten`; `CteUse::lazy`).
                #[cfg(feature = "flatten")]
                if hint == M10d::No && uses >= 2 {
                    c.lazy = true;
                }
                #[cfg(feature = "flatten")]
                let lazy = c.lazy;
                #[cfg(not(feature = "flatten"))]
                let lazy = false;
                if !(lazy || hint == M10d::Yes || (hint == M10d::Any && uses >= 2)) {
                    c.mat = 0;
                }
            }
            if let Some(d) = &mut f.derived {
                return self.select::<E>(d);
            }
            return Ok(());
        }
        if let Some(d) = &mut f.derived {
            self.select::<E>(d)?;
            // Under RETURNING, a FROM subquery is computed once for
            // the statement.
            if self.returning && f.cte.is_none() {
                f.cte = Some(Box::new(CteUse {
                    columns: AVec::new(),
                    rec: RecMark::None,
                    view: false,
                    mat: self.new_mat(M10d::Yes, 1),
                    #[cfg(feature = "flatten")]
                    is_cte: false,
                    #[cfg(feature = "flatten")]
                    lazy: false,
                    #[cfg(feature = "flatten")]
                    grp: 0,
                }));
            }
            return Ok(());
        }
        if let Some(args) = &mut f.args {
            for e in args.as_mut_slice() {
                self.expr::<E>(e)?;
            }
            return Ok(());
        }
        if f.cte.is_some() {
            // A recursive reference already marked by its CTE.
            return Ok(());
        }
        let name = copy_vec(handle_bytes(self.arena, f.table));
        if !f.qualified
            && let Some((fi, ci)) = self.find_cte(name.as_slice())
        {
            self.cte::<E>(f, fi, ci)?;
            // A reference the pin flattens into its reader is
            // computed for that reader alone.
            // A MATERIALIZED or recursive CTE never is.
            let flat = match (&f.derived, &f.cte) {
                (Some(d), Some(c)) => {
                    let hint = self
                        .mats
                        .as_slice()
                        .get((c.mat as usize).wrapping_sub(1))
                        .map_or(M10d::Any, |m| m.0);
                    hint != M10d::Yes
                        && matches!(c.rec, RecMark::None)
                        && flattens(d, o, f.kind, idx, next, self.arena)
                }
                _ => true,
            };
            if let Some(c) = &mut f.cte {
                if flat && !self.returning {
                    c.mat = 0;
                } else if self.returning
                    && let Some(m) = self
                        .mats
                        .as_mut_slice()
                        .get_mut((c.mat as usize).wrapping_sub(1))
                    && m.0 != M10d::Yes
                {
                    // Materialized once under RETURNING (see above).
                    m.0 = M10d::Yes;
                }
            }
            return Ok(());
        }
        if self.fix_main {
            f.qualified = true;
        }
        if self.catalog.find(self.ca, name.as_slice()).is_some() {
            return Ok(());
        }
        let catalog = self.catalog;
        if let Some(v) = catalog.find_view(self.ca, name.as_slice()) {
            self.view::<E>(f, v)?;
            // A candidate for sharing with a same-named view of
            // this FROM clause (`share_views`), unless flattened.
            if let (Some(d), Some(c)) = (&f.derived, &mut f.cte)
                && (self.returning || !flattens(d, o, f.kind, idx, next, self.arena))
            {
                c.mat = if self.returning {
                    self.new_mat(M10d::Yes, 1)
                } else {
                    u32::MAX
                };
            }
        }
        Ok(())
    }

    /// The pin reuses one materialization for a view that a FROM
    /// clause names twice (`isSelfJoinView`), unless one of the two
    /// was flattened or had a WHERE term pushed into it. A FROM
    /// subquery, view or CTE that the pin flattens into its reader
    /// brings its own sources into the reader's FROM clause, so
    /// those take part too. Gives each group of such views one
    /// materialization id (the second walk, top-down: the groups
    /// are settled before the bodies are walked).
    fn share_views(&mut self, s: &mut SelectStmt, o: &Outer) {
        let mut groups: AVec<(AVec<u8>, u32, u32)> = AVec::new();
        self.share_in(s, o, &mut groups);
        if groups.iter().any(|g| g.2 < 2) {
            let mut lone = AVec::new();
            for g in groups.iter().filter(|g| g.2 < 2) {
                lone.push(g.1);
            }
            unshare(s, lone.as_slice());
        }
    }

    fn share_in(&mut self, s: &mut SelectStmt, o: &Outer, groups: &mut AVec<(AVec<u8>, u32, u32)>) {
        let n = s.from.len();
        for i in 0..n {
            let pushed = pushed_down(s, i, self.arena);
            let next = s.from.as_slice().get(i + 1).map(|f| f.kind);
            let arena = self.arena;
            let Some(f) = s.from.as_mut_slice().get_mut(i) else {
                break;
            };
            let kind = f.kind;
            match f.cte.as_mut() {
                Some(c) if c.view && c.mat == u32::MAX => {
                    // (Without the flattener a pushed-down copy takes
                    // no part; with it, it is a member that computes
                    // its own rows, `CteUse::grp`.)
                    if pushed && cfg!(not(feature = "flatten")) {
                        c.mat = 0;
                        continue;
                    }
                    let name = handle_bytes(arena, f.table);
                    let id = match groups
                        .as_mut_slice()
                        .iter_mut()
                        .find(|g| eq_ci(g.0.as_slice(), name))
                    {
                        Some(g) => {
                            g.2 = g.2.saturating_add(1);
                            g.1
                        }
                        None => {
                            let id = self.new_mat(M10d::Yes, 2);
                            groups.push((copy_vec(name), id, 1));
                            id
                        }
                    };
                    c.mat = if pushed { 0 } else { id };
                    #[cfg(feature = "flatten")]
                    {
                        c.grp = id;
                    }
                }
                Some(c) if c.mat != 0 || !matches!(c.rec, RecMark::None) => {}
                _ => {
                    if let Some(d) = f.derived.as_deref_mut()
                        && d.compound.is_empty()
                        && flattens(d, o, kind, i, next, arena)
                    {
                        let inner = outer_of(d, true, false, arena);
                        self.share_in(d, &inner, groups);
                    }
                }
            }
        }
    }

    /// Rewrite `f` into the derived table of view `v`.
    fn view<E>(&mut self, f: &mut FromSource, v: &crate::schema::ViewDef) -> Result<(), E> {
        let name = handle_bytes(self.ca, v.name);
        if self.views.iter().any(|n| eq_ci(n.as_slice(), name)) {
            return Err(schema_err(SchemaKind::CircularView));
        }
        self.enter::<E>()?;
        let parsed =
            crate::sql::parser::parse_view::<E>(handle_bytes(self.ca, v.sql), self.arena, self.lim);
        let Ok(Ast::CreateView {
            columns,
            mut select,
            ..
        }) = parsed
        else {
            self.nest -= 1;
            return Err(parsed.err().unwrap_or(Error::Unsupported));
        };
        if let Err(e) = self.read_body::<E>(select.rdepth) {
            self.nest -= 1;
            return Err(e);
        }
        self.copies = self.copies.saturating_add(count_selects(&select));
        // A view's body sees no CTE of the query that reads it.
        let vis = core::mem::take(&mut self.vis);
        self.views.push(copy_vec(name));
        // Its names are main's (the expander reads main's views
        // only; on the temp connection the mark routes nothing).
        let fixed = core::mem::replace(&mut self.fix_main, true);
        let r = self.select::<E>(&mut select);
        self.fix_main = fixed;
        self.views.pop();
        self.vis = vis;
        self.nest -= 1;
        r?;
        if f.alias.is_none() {
            f.alias = Some(f.table);
        }
        f.derived = Some(Box::new(select));
        f.cte = Some(Box::new(CteUse {
            columns: columns.unwrap_or_default(),
            rec: RecMark::None,
            view: true,
            mat: 0,
            #[cfg(feature = "flatten")]
            is_cte: false,
            #[cfg(feature = "flatten")]
            lazy: false,
            #[cfg(feature = "flatten")]
            grp: 0,
        }));
        Ok(())
    }

    /// Rewrite `f` into the derived table of CTE `ci` of frame
    /// `fi` (the pin's `withExpand`; see the module docs).
    fn cte<E>(&mut self, f: &mut FromSource, fi: usize, ci: usize) -> Result<(), E> {
        let (cte, busy, mat) = {
            let frame = self
                .frames
                .as_slice()
                .get(fi)
                .ok_or(schema_err::<E>(SchemaKind::UnknownTable))?;
            let cte = frame
                .ctes
                .as_slice()
                .get(ci)
                .ok_or(schema_err::<E>(SchemaKind::UnknownTable))?;
            let busy = frame.busy.as_slice().get(ci).copied().unwrap_or(Busy::Free);
            let mat = frame.ids.as_slice().get(ci).copied().unwrap_or(0);
            (cte.clone(), busy, mat)
        };
        match busy {
            Busy::Free => {}
            Busy::Circular => return Err(schema_err(SchemaKind::CircularReference)),
            Busy::MultiRec => return Err(schema_err(SchemaKind::RecursiveReferences)),
        }
        self.copies = self.copies.saturating_add(count_selects(&cte.select));
        if self.copies >= MAX_COPIES {
            return Err(schema_err(SchemaKind::NestedTooDeep));
        }
        self.read_body::<E>(cte.select.rdepth)?;
        if let Some(m) = self
            .mats
            .as_mut_slice()
            .get_mut((mat as usize).wrapping_sub(1))
        {
            // Inside an `x IN (SELECT ...)` the pin counts each
            // reference twice (v0.17 review fix, `flatten`: probed, a
            // CTE read only there is a MATERIALIZE, never a
            // CO-ROUTINE).
            #[cfg(feature = "flatten")]
            let uses = if self.in_rhs { 2 } else { 1 };
            #[cfg(not(feature = "flatten"))]
            let uses = 1;
            m.1 = m.1.saturating_add(uses);
        }
        let name = copy_vec(handle_bytes(self.arena, cte.name));
        let mut body = cte.select;
        // The body sees its own clause and those around it only.
        let saved = self.vis.clone();
        if let Some(p) = self.vis.iter().position(|&x| x == fi) {
            let mut v = AVec::new();
            for &x in self.vis.as_slice().get(..=p).unwrap_or(&[]) {
                v.push(x);
            }
            self.vis = v;
        }
        let id = self.next_id;
        let r = self.enter::<E>().and_then(|()| {
            let r = self.cte_body::<E>(&mut body, name.as_slice(), fi, ci, id);
            self.nest -= 1;
            r
        });
        self.set_busy(fi, ci, Busy::Free);
        self.vis = saved;
        let split = r?;
        if split.is_some() {
            self.next_id = self.next_id.wrapping_add(1).max(1);
        }
        if f.alias.is_none() {
            f.alias = Some(f.table);
        }
        f.derived = Some(Box::new(body));
        f.cte = Some(Box::new(CteUse {
            columns: cte.columns,
            rec: match split {
                Some(split) => RecMark::Body { split, id },
                None => RecMark::None,
            },
            view: false,
            mat,
            #[cfg(feature = "flatten")]
            is_cte: true,
            #[cfg(feature = "flatten")]
            lazy: false,
            #[cfg(feature = "flatten")]
            grp: 0,
        }));
        Ok(())
    }

    fn set_busy(&mut self, fi: usize, ci: usize, b: Busy) {
        if let Some(slot) = self
            .frames
            .as_mut_slice()
            .get_mut(fi)
            .and_then(|f| f.busy.as_mut_slice().get_mut(ci))
        {
            *slot = b;
        }
    }

    /// Expand a CTE's body in place, returning the first
    /// recursive core (`Some(split)`) when the CTE is recursive.
    fn cte_body<E>(
        &mut self,
        body: &mut SelectStmt,
        name: &[u8],
        fi: usize,
        ci: usize,
        id: u32,
    ) -> Result<Option<usize>, E> {
        // Mark the recursive references: walk from the rightmost
        // core leftwards while the operator matches the rightmost
        // one and the core names the CTE in its FROM clause.
        let last_op = body.compound.as_slice().last().map(|(op, _)| *op);
        let may_rec = matches!(last_op, Some(CompoundOp::Union | CompoundOp::UnionAll));
        let ncores = body.compound.len() + 1;
        let mut split = ncores;
        if may_rec {
            for k in (1..ncores).rev() {
                let Some((op, core)) = body.compound.as_mut_slice().get_mut(k - 1) else {
                    break;
                };
                if Some(*op) != last_op {
                    break;
                }
                let refs = core
                    .from
                    .iter()
                    .filter(|s| is_ref(s, name, self.arena))
                    .count();
                if refs > 1 {
                    return Err(schema_err(SchemaKind::RecursiveReferences));
                }
                if refs == 0 {
                    break;
                }
                for s in core.from.as_mut_slice() {
                    if is_ref(s, name, self.arena) {
                        s.cte = Some(Box::new(CteUse {
                            columns: AVec::new(),
                            rec: RecMark::Ref { id },
                            view: false,
                            mat: 0,
                            #[cfg(feature = "flatten")]
                            is_cte: true,
                            #[cfg(feature = "flatten")]
                            lazy: false,
                            #[cfg(feature = "flatten")]
                            grp: 0,
                        }));
                    }
                }
                split = k;
            }
        }
        self.set_busy(fi, ci, Busy::Circular);
        if split < ncores {
            self.rec_body::<E>(body, split, fi, ci)?;
        } else {
            self.select::<E>(body)?;
        }
        Ok((split < ncores).then_some(split))
    }

    /// A recursive CTE's body: the setup cores with the CTE
    /// "circular", then the recursive cores and the ORDER BY /
    /// LIMIT with it "multiple recursive references".
    fn rec_body<E>(
        &mut self,
        body: &mut SelectStmt,
        split: usize,
        fi: usize,
        ci: usize,
    ) -> Result<(), E> {
        let pushed = match body.with.take() {
            Some(w) => {
                self.push_frame::<E>(*w)?;
                true
            }
            None => false,
        };
        let r = (|| {
            let n = body.compound.len();
            let head = outer_of(body, false, true, self.arena);
            self.core::<E>(body, &head)?;
            for (k, (_, c)) in body.compound.as_mut_slice().iter_mut().enumerate() {
                if k + 1 == split {
                    self.set_busy(fi, ci, Busy::MultiRec);
                }
                let o = outer_of(c, false, n > 0, self.arena);
                self.core::<E>(c, &o)?;
            }
            self.set_busy(fi, ci, Busy::MultiRec);
            self.tail::<E>(body)
        })();
        if pushed {
            self.pop_frame();
        }
        r
    }

    fn expr<E>(&mut self, e: &mut Expr) -> Result<(), E> {
        match e {
            Expr::Literal(_) | Expr::Column { .. } | Expr::HexTooBig(_) => {}
            #[cfg(feature = "params")]
            Expr::Param(..) => {}
            #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
            Expr::TrigValue(..) => {}
            #[cfg(feature = "triggers")]
            Expr::Raise(_, None, _) => {}
            #[cfg(feature = "triggers")]
            Expr::Raise(_, Some(a), _) => self.expr::<E>(a)?,
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
                #[cfg(feature = "flatten")]
                let was = core::mem::replace(&mut self.in_rhs, true);
                let r = self.select::<E>(s);
                #[cfg(feature = "flatten")]
                {
                    self.in_rhs = was;
                }
                r?;
            }
            Expr::ScalarSubquery(s) | Expr::ExistsSubquery(s) => {
                self.sub_limit = true;
                self.select::<E>(s)?;
            }
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
                    if let Some(crate::sql::ast::Over::Spec(w)) = &mut c.over {
                        let mut r = Ok(());
                        w.exprs_mut(&mut |x| {
                            if r.is_ok() {
                                r = self.expr::<E>(x);
                            }
                        });
                        r?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Clear the materialization ids in `lone` (view groups of one)
/// from the FROM sources of `s` and of its derived tables.
fn unshare(s: &mut SelectStmt, lone: &[u32]) {
    for f in s.from.as_mut_slice() {
        if let Some(c) = f.cte.as_mut()
            && lone.contains(&c.mat)
        {
            c.mat = 0;
        }
        if let Some(d) = f.derived.as_deref_mut() {
            unshare(d, lone);
        }
    }
}

/// The flattener's view of core `s` as the reader of its FROM
/// sources; `tail` says whether the statement's ORDER BY / LIMIT
/// are this core's.
fn outer_of(s: &SelectStmt, tail: bool, compound: bool, arena: &Arena) -> Outer {
    Outer {
        nsrc: s.from.len(),
        filter: s.filter.is_some(),
        limit: tail && s.limit.is_some(),
        order: tail && !s.order_by.is_empty(),
        agg: core_is_agg(s, arena),
        distinct: s.distinct,
        compound,
        complex: s.items.iter().any(|it| match it {
            SelectItem::Expr(e, ..) => crate::rules::expr_any(e, &mut |x| {
                matches!(
                    x,
                    Expr::Func { .. }
                        | Expr::ScalarSubquery(_)
                        | Expr::ExistsSubquery(_)
                        | Expr::InSubquery(..)
                )
            }),
            _ => false,
        }),
        #[cfg(feature = "flatten")]
        win: core_win(s),
    }
}

/// Whether core `s`'s result list or ORDER BY calls a window
/// function (v0.16, `window`).
#[cfg(feature = "flatten")]
fn core_win(s: &SelectStmt) -> bool {
    #[cfg(feature = "window")]
    {
        let w = |e: &Expr| {
            crate::rules::expr_any(
                e,
                &mut |x| matches!(x, Expr::Func { clause: Some(c), .. } if c.over.is_some()),
            )
        };
        s.items.iter().any(|it| match it {
            SelectItem::Expr(e, ..) => w(e),
            _ => false,
        }) || s.order_by.iter().any(|(e, _)| w(e))
    }
    #[cfg(not(feature = "window"))]
    {
        let _ = s;
        false
    }
}

/// Whether `n` names a JSON aggregate (v0.16, `json`).
fn json_agg_name(n: &[u8]) -> bool {
    #[cfg(feature = "json")]
    return crate::json::JsonAgg::lookup(n).is_some();
    #[cfg(not(feature = "json"))]
    {
        let _ = n;
        false
    }
}

/// Whether a core aggregates: GROUP BY, HAVING, or an aggregate
/// call (outside subqueries) in its result list or ORDER BY.
fn core_is_agg(s: &SelectStmt, arena: &Arena) -> bool {
    const AGGS: [&[u8]; 12] = [
        b"count",
        b"sum",
        b"avg",
        b"total",
        b"min",
        b"max",
        b"group_concat",
        b"string_agg",
        b"median",
        b"percentile",
        b"percentile_cont",
        b"percentile_disc",
    ];
    let is_agg = |e: &Expr| {
        crate::rules::expr_any(e, &mut |x| match x {
            Expr::Func { name, args, .. } => {
                let n = handle_bytes(arena, *name);
                (AGGS.iter().any(|a| eq_ci(n, a)) || json_agg_name(n))
                    && !((eq_ci(n, b"min") || eq_ci(n, b"max")) && args.len() > 1)
            }
            _ => false,
        })
    };
    !s.group_by.is_empty()
        || s.having.is_some()
        || s.items.iter().any(|it| match it {
            SelectItem::Expr(e, ..) => is_agg(e),
            _ => false,
        })
        || s.order_by.iter().any(|(e, _)| is_agg(e))
}

/// Whether the pin's query flattener merges FROM source `idx` of a
/// reader shaped `o` — whose own join is `kind`, the next source's
/// `next` — with body `d` into the reader (`flattenSubquery` and
/// its caller's restrictions, the ones that hold before planning).
/// A flattened view or CTE is computed afresh for its reader; one
/// that is not may share a materialization. The pin's further
/// restrictions on what flattening the body would merge (its
/// column affinities across a compound, nested flattening) are not
/// modelled.
fn flattens(
    d: &SelectStmt,
    o: &Outer,
    kind: JoinKind,
    idx: usize,
    next: Option<JoinKind>,
    arena: &Arena,
) -> bool {
    // The right operand of a RIGHT or FULL JOIN never is (26); a
    // LEFT JOIN's only as one source, under a reader that is not
    // DISTINCT (3a, 3c — probed: a GROUP BY reader flattens it).
    let outer_rhs = matches!(kind, JoinKind::Left);
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        return false;
    }
    let lim = d.limit.is_some();
    if lim && (o.nsrc > 1 || o.agg || o.limit || o.compound || o.filter || o.distinct) {
        return false;
    }
    if d.offset.is_some() {
        return false;
    }
    // A FROM subquery's ORDER BY that orders nothing is dropped
    // first; one that stays blocks flattening under an ordered or
    // aggregate reader, or a first source read as a co-routine by
    // a reader with a complex result.
    let order = !d.order_by.is_empty() && (lim || !(o.order || o.nsrc > 1));
    if order
        && (o.order
            || o.agg
            || (idx == 0
                && o.complex
                && (o.nsrc == 1
                    || matches!(
                        next,
                        Some(JoinKind::Left | JoinKind::Full | JoinKind::Cross)
                    ))))
    {
        return false;
    }
    let plain = |c: &SelectStmt| {
        c.values.is_empty() && !c.from.is_empty() && !c.distinct && !core_is_agg(c, arena)
    };
    if !d.compound.is_empty() {
        return !lim
            && !order
            && !o.agg
            && !o.distinct
            && !outer_rhs
            && plain(d)
            && d.compound
                .iter()
                .all(|(op, c)| *op == CompoundOp::UnionAll && plain(c));
    }
    if !plain(d) {
        return false;
    }
    if outer_rhs && (d.from.len() > 1 || o.distinct) {
        return false;
    }
    if idx > 0
        && d.from
            .iter()
            .any(|f| matches!(f.kind, JoinKind::Right | JoinKind::Full))
    {
        return false;
    }
    true
}

/// Whether the pin pushes a WHERE term of core `s` down into its
/// FROM source `idx` (`pushDownWhereTerms`): a term of the WHERE
/// clause (or of an inner join's ON) that reads only that source's
/// columns, by its name, with no subquery — into a body without a
/// LIMIT.
fn pushed_down(s: &SelectStmt, idx: usize, arena: &Arena) -> bool {
    let Some(f) = s.from.as_slice().get(idx) else {
        return false;
    };
    if f.derived.as_ref().is_some_and(|d| d.limit.is_some()) {
        return false;
    }
    let q = handle_bytes(arena, f.alias.unwrap_or(f.table));
    let only_this = |e: &Expr| {
        let mut cols = 0usize;
        let other = crate::rules::expr_any(e, &mut |x| match x {
            Expr::Column { table, .. } => match table {
                Some(t) if eq_ci(handle_bytes(arena, *t), q) => {
                    cols += 1;
                    false
                }
                _ => true,
            },
            Expr::ScalarSubquery(_) | Expr::ExistsSubquery(_) | Expr::InSubquery(..) => true,
            _ => false,
        });
        !other && cols > 0
    };
    let mut terms: AVec<&Expr> = AVec::new();
    if let Some(w) = &s.filter {
        conjuncts(w, &mut terms);
    }
    for g in s.from.iter() {
        if matches!(g.kind, JoinKind::Inner | JoinKind::Cross)
            && let Some(on) = &g.on
        {
            conjuncts(on, &mut terms);
        }
    }
    terms.iter().any(|t| only_this(t))
}

/// The AND-separated terms of `e`.
fn conjuncts<'e>(e: &'e Expr, out: &mut AVec<&'e Expr>) {
    match e {
        Expr::Binary(crate::sql::BinOp::And, a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        _ => out.push(e),
    }
}

/// How many SELECTs `s` holds: its cores, and those of its derived
/// tables, subqueries and WITH clause.
fn count_selects(s: &SelectStmt) -> usize {
    let mut n = 0usize;
    count_into(s, &mut n);
    n
}

fn count_into(s: &SelectStmt, n: &mut usize) {
    count_core(s, n);
    for (_, c) in s.compound.iter() {
        count_core(c, n);
    }
    for (e, _) in s.order_by.iter() {
        count_expr(e, n);
    }
    if let Some(w) = &s.with {
        for c in w.ctes.iter() {
            count_into(&c.select, n);
        }
    }
}

fn count_core(s: &SelectStmt, n: &mut usize) {
    *n = n.saturating_add(1);
    for it in s.items.iter() {
        if let SelectItem::Expr(e, ..) = it {
            count_expr(e, n);
        }
    }
    for f in s.from.iter() {
        if let Some(d) = &f.derived {
            count_into(d, n);
        }
        if let Some(on) = &f.on {
            count_expr(on, n);
        }
    }
    for e in s
        .filter
        .iter()
        .chain(s.having.iter())
        .chain(s.group_by.iter())
    {
        count_expr(e, n);
    }
}

fn count_expr(e: &Expr, n: &mut usize) {
    crate::rules::expr_any(e, &mut |x| {
        match x {
            Expr::ScalarSubquery(s) | Expr::ExistsSubquery(s) | Expr::InSubquery(_, s, _) => {
                count_into(s, n)
            }
            _ => {}
        }
        false
    });
}

/// Whether FROM source `s` is a direct (recursive) reference to
/// the CTE named `name`.
fn is_ref(s: &FromSource, name: &[u8], arena: &Arena) -> bool {
    s.derived.is_none()
        && s.args.is_none()
        && !s.qualified
        && s.cte.is_none()
        && eq_ci(handle_bytes(arena, s.table), name)
}

/// A recursive CTE's planned compound tails and their compound
/// ORDER BY keys (`Connection::plan_rec_tails`).
type RecTails = (
    AVec<(CompoundOp, Box<crate::query::SubPlan>)>,
    AVec<CompKeys>,
);

/// A recursive CTE's column shape while its recursive SELECTs are
/// planned (`Connection::rec_scope`).
#[derive(Debug, Clone, Default)]
pub(super) struct RecShape {
    id: u32,
    names: AVec<AVec<u8>>,
    affs: AVec<Option<crate::record::Affinity>>,
    colls: AVec<crate::record::Coll>,
}

impl<S: Storage> Connection<S> {
    /// A derived view / CTE table's column names (v0.16, `v16`):
    /// its column list when one was written — which must have the
    /// SELECT's width (the pin's "expected N columns for 'v' but
    /// got M" / "table x has M values for N columns") — else the
    /// SELECT's result names.
    pub(super) fn cte_names(
        arena: &Arena,
        cte: Option<&CteUse>,
        plan: &crate::query::SubPlan,
    ) -> Result<AVec<AVec<u8>>, S::Error> {
        match cte {
            Some(c) if !c.columns.is_empty() => {
                if c.columns.len() != plan.width() {
                    return Err(schema_err(SchemaKind::ColumnCountMismatch));
                }
                let mut out = AVec::new();
                for h in c.columns.iter() {
                    out.push(copy_vec(handle_bytes(arena, *h)));
                }
                Ok(out)
            }
            _ => Ok(plan.result_names.clone()),
        }
    }

    /// Plan a recursive CTE's body (v0.16, `v16`): the setup
    /// cores (`0 .. split`) as a compound, then each recursive
    /// SELECT with its recursive reference shaped like the setup's
    /// result (names from the CTE's column list or the leftmost
    /// core, that core's affinities and the compound's
    /// collations), then the CTE's ORDER BY over every core, the
    /// pin's compound ORDER BY rules, and its LIMIT / OFFSET.
    pub(super) fn plan_recursive(
        &self,
        d: &SelectStmt,
        split: usize,
        id: u32,
        cte: Option<&CteUse>,
        arena: &Arena,
        scope: &[crate::query::ScopeGroup],
    ) -> Result<crate::query::SubPlan, S::Error> {
        let (base_width, base_src) = crate::query::ScopeGroup::stack_end(scope);
        let mut head = d.clone();
        head.compound = AVec::new();
        head.order_by = AVec::new();
        head.limit = None;
        head.offset = None;
        let (mut plan, head_keys) = self.plan_subquery_keyed(&head, arena, scope)?;
        let width = plan.width();
        // The recursive table's columns take the CTE's compound
        // affinities (`rules::compound_affinity`), which depend on
        // the recursive SELECTs' items: plan them with the setup's
        // first, and again if that demotes a column.
        let mut affs = plan.result_affs.clone();
        let mut pass = 0;
        let (tails, keys) = loop {
            let (tails, keys) =
                self.plan_rec_tails(d, split, id, cte, arena, scope, &plan, &affs)?;
            pass += 1;
            let mut demoted = AVec::new();
            for (i, a) in plan.result_affs.iter().enumerate() {
                demoted.push(a.map(|a| {
                    crate::rules::compound_affinity(
                        a,
                        plan.items.as_slice().get(i),
                        tails.iter().map(|(_, t): &(CompoundOp, Box<_>)| &**t),
                        i,
                    )
                }));
            }
            if pass > 1 || demoted == affs {
                break (tails, keys);
            }
            affs = demoted;
        };
        plan.comp_order = resolve_compound_order::<S::Error>(
            d.order_by.as_slice(),
            arena,
            &head_keys,
            keys.as_slice(),
            width,
        )?;
        plan.limit = self.plan_limit(d.limit.as_deref(), arena)?;
        plan.offset = self.plan_limit(d.offset.as_deref(), arena)?;
        plan.compound = tails;
        plan.rec = Some(crate::query::RecInfo {
            split,
            union: matches!(
                d.compound.as_slice().last().map(|(op, _)| *op),
                Some(CompoundOp::Union)
            ),
            id,
        });
        self.stamp_subplan(&mut plan, base_width, base_src);
        Ok(plan)
    }

    /// `plan_recursive`'s compound tails — the setup's later cores,
    /// then the recursive SELECTs, planned with the recursive
    /// table shaped by `affs` — and their compound ORDER BY keys.
    #[allow(clippy::too_many_arguments)]
    fn plan_rec_tails(
        &self,
        d: &SelectStmt,
        split: usize,
        id: u32,
        cte: Option<&CteUse>,
        arena: &Arena,
        scope: &[crate::query::ScopeGroup],
        head: &crate::query::SubPlan,
        affs: &AVec<Option<crate::record::Affinity>>,
    ) -> Result<RecTails, S::Error> {
        let width = head.width();
        let mut tails = AVec::new();
        let mut keys = AVec::new();
        let mut cores = AVec::new();
        cores.push(core_colls(head.items.as_slice(), head.values.as_slice()));
        let mut pushed = false;
        let r = (|| -> Result<(), S::Error> {
            for (k, (op, core)) in d.compound.iter().enumerate() {
                let recursive = k + 1 >= split;
                if recursive && !pushed {
                    let mut names = Self::cte_names(arena, cte, head)?;
                    crate::rules::unique_names::<S::Error>(names.as_mut_slice())?;
                    let mut colls = AVec::new();
                    for i in 0..width {
                        colls.push(compound_col_coll(cores.iter(), i));
                    }
                    let mut st = self.rec_scope.take();
                    st.push(RecShape {
                        id,
                        names,
                        affs: affs.clone(),
                        colls,
                    });
                    self.rec_scope.set(st);
                    pushed = true;
                }
                let (mut p, kk) = self.plan_subquery_keyed(core, arena, scope)?;
                if p.width() != width {
                    return Err(schema_err(SchemaKind::ColumnCountMismatch));
                }
                if recursive {
                    // The pin checks only the rightmost SELECT
                    // (`p->pWin` in generateWithRecursiveQuery),
                    // before its aggregate check.
                    #[cfg(feature = "window")]
                    if k + 1 == d.compound.len()
                        && (p.items.iter().any(crate::window::has_win)
                            || p.order_by.iter().any(|(e, _)| crate::window::has_win(e)))
                    {
                        return Err(schema_err(SchemaKind::RecursiveWindow));
                    }
                    if p.has_agg || !p.group_by.is_empty() {
                        return Err(schema_err(SchemaKind::RecursiveAggregate));
                    }
                } else {
                    cores.push(core_colls(p.items.as_slice(), p.values.as_slice()));
                }
                // A recursive step reads the queue's row: never
                // cached as uncorrelated.
                p.uncorrelated &= !recursive;
                tails.push((*op, Box::new(p)));
                keys.push(kk);
            }
            Ok(())
        })();
        if pushed {
            let mut st = self.rec_scope.take();
            st.pop();
            self.rec_scope.set(st);
        }
        r?;
        Ok((tails, keys))
    }

    /// Plan a recursive reference (v0.16, `v16`): a one-row
    /// source shaped like its CTE (`plan_recursive`). Returns its
    /// width.
    pub(super) fn plan_rec_ref(
        &self,
        s: &FromSource,
        id: u32,
        arena: &Arena,
        descs: &mut AVec<crate::query::SourceDesc>,
        plans: &mut AVec<SourcePlan>,
    ) -> Result<usize, S::Error> {
        let st = self.rec_scope.take();
        let shape = st.iter().rev().find(|r| r.id == id).cloned();
        self.rec_scope.set(st);
        let shape = shape.ok_or(schema_err::<S::Error>(SchemaKind::UnknownTable))?;
        let w = shape.names.len();
        let qual = copy_vec(handle_bytes(arena, s.alias.unwrap_or(s.table)));
        let mut cols = AVec::new();
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        for (i, n) in shape.names.iter().enumerate() {
            let aff = shape.affs.as_slice().get(i).copied().flatten();
            cols.push((n.clone(), aff.unwrap_or(crate::record::Affinity::Blob)));
            affs.push(crate::record::Affinity::Blob);
            defaults.push(OwnedValue::Null);
        }
        let mut desc = crate::query::SourceDesc::new(qual, cols, None, false);
        desc.kind = s.kind;
        desc.colls = shape.colls;
        descs.push(desc);
        plans.push(SourcePlan {
            #[cfg(feature = "explain")]
            eqp: Default::default(),
            root: 0,
            kind: s.kind,
            on: None,
            rowid_alias: None,
            ncols: w,
            affs,
            defaults,
            derived: None,
            gcols: None,
            rec: id,
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
            #[cfg(feature = "v16")]
            temp: false,
            #[cfg(feature = "attach")]
            ext: 0,
            #[cfg(feature = "flatten")]
            exists: false,
        });
        Ok(w)
    }
}

/// The recursive CTE queue (see the module docs): FIFO, or — with
/// an ORDER BY — a binary heap on (ORDER BY key, arrival).
struct RecQueue<'o> {
    order: &'o [(usize, crate::sql::SortOrder)],
    /// The database's text encoding (its BINARY order).
    enc: crate::record::TextEnc,
    rows: AVec<(u64, AVec<OwnedValue>)>,
    head: usize,
    seq: u64,
}

impl RecQueue<'_> {
    fn less(&self, a: usize, b: usize) -> bool {
        match (self.rows.as_slice().get(a), self.rows.as_slice().get(b)) {
            (Some((sa, ra)), Some((sb, rb))) => {
                compound_order_cmp(ra, rb, self.order, self.enc).then(sa.cmp(sb))
                    == core::cmp::Ordering::Less
            }
            _ => false,
        }
    }

    fn push(&mut self, row: AVec<OwnedValue>) {
        self.seq = self.seq.wrapping_add(1);
        self.rows.push((self.seq, row));
        if self.order.is_empty() {
            return;
        }
        let mut i = self.rows.len() - 1;
        while i > 0 {
            let parent = (i - 1) / 2;
            if !self.less(i, parent) {
                break;
            }
            self.rows.as_mut_slice().swap(i, parent);
            i = parent;
        }
    }

    fn pop(&mut self) -> Option<AVec<OwnedValue>> {
        if self.order.is_empty() {
            let r = self.rows.as_mut_slice().get_mut(self.head)?;
            let row = core::mem::take(&mut r.1);
            self.head += 1;
            // Drop the consumed front now and then.
            if self.head >= 1024 && self.head * 2 >= self.rows.len() {
                self.rows.remove_front(self.head);
                self.head = 0;
            }
            return Some(row);
        }
        let n = self.rows.len();
        if n == 0 {
            return None;
        }
        self.rows.as_mut_slice().swap(0, n - 1);
        let (_, row) = self.rows.pop()?;
        let n = self.rows.len();
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < n && self.less(l, m) {
                m = l;
            }
            if r < n && self.less(r, m) {
                m = r;
            }
            if m == i {
                break;
            }
            self.rows.as_mut_slice().swap(i, m);
            i = m;
        }
        Some(row)
    }
}

impl<S: Storage> Statement<'_, S> {
    /// The rows of a derived table that shares a materialization
    /// (v0.16 review fix, `v16`; `SubPlan::mat`): computed by the
    /// first reader of this execution, then served to every other.
    /// A recursive CTE is computed only as far as a bounded reader
    /// needs (`need`), and again when a later reader needs more.
    pub(super) async fn shared_rows(
        &mut self,
        d: &crate::query::SubPlan,
        outer: &RowData,
        need: Option<usize>,
    ) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        for (id, rows, complete) in self.mats.iter() {
            if *id == d.mat && (*complete || need.is_some_and(|n| rows.len() >= n)) {
                return Ok(rows.clone());
            }
        }
        let ask = if d.rec.is_some() { need } else { None };
        let rows = self.exec_subplan_need(d, outer, ask).await?;
        let complete = ask.is_none_or(|n| rows.len() < n);
        match self
            .mats
            .as_mut_slice()
            .iter_mut()
            .find(|(id, ..)| *id == d.mat)
        {
            Some(slot) => *slot = (d.mat, rows.clone(), complete),
            None => self.mats.push((d.mat, rows.clone(), complete)),
        }
        Ok(rows)
    }

    /// Run a recursive CTE (v0.16, `v16`; the pin's
    /// `generateWithRecursiveQuery`): LIMIT / OFFSET evaluate
    /// first (a LIMIT of 0 runs nothing), the setup's rows enter
    /// the queue, then each row taken from the queue is output
    /// unless the OFFSET still skips it — the LIMIT, or the
    /// reader's `need`, stopping everything once reached — and is
    /// fed to every recursive SELECT, whose rows enter the queue.
    /// Under UNION a row equal (under the compound's column
    /// collations) to any row that entered before is dropped.
    pub(super) async fn exec_recursive(
        &mut self,
        plan: &crate::query::SubPlan,
        rec: crate::query::RecInfo,
        outer: &RowData,
        need: Option<usize>,
    ) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        let (mut skip, take) = self.limit_window(&plan.limit, &plan.offset).await?;
        let take = need.map_or(take, |n| take.min(n));
        let mut out = AVec::new();
        if take == 0 {
            return Ok(out);
        }
        let mut setup = plan.clone();
        setup.rec = None;
        setup.comp_order = AVec::new();
        setup.limit = None;
        setup.offset = None;
        setup.compound = AVec::new();
        let mut steps: AVec<&crate::query::SubPlan> = AVec::new();
        let mut cores = AVec::new();
        cores.push(core_colls(plan.items.as_slice(), plan.values.as_slice()));
        for (k, (op, t)) in plan.compound.iter().enumerate() {
            cores.push(core_colls(t.items.as_slice(), t.values.as_slice()));
            if k + 1 < rec.split {
                setup.compound.push((*op, t.clone()));
            } else {
                steps.push(t);
            }
        }
        let width = plan.width();
        let mut colls = AVec::new();
        for i in 0..width {
            colls.push(compound_col_coll(cores.iter(), i));
        }
        let enc = self.conn.pager.text_enc();
        let mut queue = RecQueue {
            enc,
            order: plan.comp_order.as_slice(),
            rows: AVec::new(),
            head: 0,
            seq: 0,
        };
        // UNION: every row that entered the queue, sorted.
        let mut seen: AVec<AVec<OwnedValue>> = AVec::new();
        let admit = |row: &AVec<OwnedValue>, seen: &mut AVec<AVec<OwnedValue>>| -> bool {
            if !rec.union {
                return true;
            }
            let key = |r: &AVec<OwnedValue>| -> core::cmp::Ordering {
                compound_key_cmp(r.as_slice(), row.as_slice(), colls.as_slice(), enc)
            };
            match seen.as_slice().binary_search_by(key) {
                Ok(_) => false,
                Err(at) => {
                    seen.insert_at(at, row.clone());
                    true
                }
            }
        };
        for row in self.exec_subplan(&setup, outer).await?.iter() {
            if admit(row, &mut seen) {
                queue.push(row.clone());
            }
        }
        while let Some(row) = queue.pop() {
            if skip > 0 {
                skip -= 1;
            } else {
                out.push(row.clone());
                if out.len() >= take {
                    break;
                }
            }
            self.rec_rows.push((rec.id, row));
            let mut r = Ok(());
            for step in steps.iter() {
                match self.exec_subplan_core(step, outer, None, None).await {
                    Ok(produced) => {
                        for p in produced.iter() {
                            if admit(&p.out, &mut seen) {
                                queue.push(p.out.clone());
                            }
                        }
                    }
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                }
            }
            self.rec_rows.pop();
            r?;
        }
        Ok(out)
    }
}
