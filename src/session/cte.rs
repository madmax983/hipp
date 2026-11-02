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
use crate::sql::{CompoundOp, Cte, CteUse, FromSource, RecMark, UpsertAction, With};

/// How deep views and CTE bodies may nest inside one another
/// before the expansion gives up (`ParseKind::RecursionLimit`).
const MAX_EXPANSION_DEPTH: usize = 64;

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
    depth: usize,
}

fn schema_err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

impl<S: Storage> Connection<S> {
    /// Expand every view and CTE reference in `ast` into a derived
    /// table (v0.16, `v16`; see the module docs). `None` when the
    /// statement holds nothing to expand (it plans as written).
    /// INSERT into a view is refused here (`IsView`).
    pub(super) fn expand_ast(&self, arena: &Arena, ast: &Ast) -> Result<Option<Ast>, S::Error> {
        let mut x = Expander {
            catalog: &self.catalog,
            ca: &self.catalog_arena,
            arena,
            frames: AVec::new(),
            vis: AVec::new(),
            views: AVec::new(),
            next_id: 1,
            depth: 0,
        };
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
        let mut x = Expander {
            catalog: &self.catalog,
            ca: &self.catalog_arena,
            arena,
            frames: AVec::new(),
            vis: AVec::new(),
            views: AVec::new(),
            next_id: 1,
            depth: 0,
        };
        let mut f = FromSource {
            table: arena.alloc_copy_handle(handle_bytes(&self.catalog_arena, v.name))?,
            alias: None,
            kind: crate::sql::JoinKind::Inner,
            on: None,
            derived: None,
            natural: false,
            using: AVec::new(),
            args: None,
            qualified: true,
            cte: None,
        };
        x.view::<S::Error>(&mut f, v)?;
        let cols = f.cte.map(|c| c.columns).unwrap_or_default();
        match f.derived {
            Some(d) => Ok((*d, cols)),
            None => Err(Error::Unsupported),
        }
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
                self.items::<E>(returning.as_mut_slice())?;
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
                self.sources::<E>(from.as_mut_slice())?;
                if let Some(f) = filter {
                    self.expr::<E>(f)?;
                }
                self.items::<E>(returning.as_mut_slice())?;
            }
            Ast::Delete {
                filter, returning, ..
            } => {
                if let Some(f) = filter {
                    self.expr::<E>(f)?;
                }
                self.items::<E>(returning.as_mut_slice())?;
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
        for _ in ctes.iter() {
            busy.push(Busy::Free);
        }
        self.vis.push(self.frames.len());
        self.frames.push(Frame { ctes, busy });
        Ok(())
    }

    fn pop_frame(&mut self) {
        self.vis.pop();
        self.frames.pop();
    }

    fn enter<E>(&mut self) -> Result<(), E> {
        self.depth += 1;
        if self.depth > MAX_EXPANSION_DEPTH {
            return Err(Error::Parse {
                offset: 0,
                kind: crate::error::ParseKind::RecursionLimit,
            });
        }
        Ok(())
    }

    fn select<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        self.enter::<E>()?;
        let pushed = match s.with.take() {
            Some(w) => {
                self.push_frame::<E>(*w)?;
                true
            }
            None => false,
        };
        let r = self.select_body::<E>(s);
        if pushed {
            self.pop_frame();
        }
        self.depth -= 1;
        r
    }

    fn select_body<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        self.core::<E>(s)?;
        for (_, c) in s.compound.as_mut_slice() {
            self.core::<E>(c)?;
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
    fn core<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        self.items::<E>(s.items.as_mut_slice())?;
        self.sources::<E>(s.from.as_mut_slice())?;
        if let Some(f) = &mut s.filter {
            self.expr::<E>(f)?;
        }
        for g in s.group_by.as_mut_slice() {
            self.expr::<E>(g)?;
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

    fn items<E>(&mut self, items: &mut [SelectItem]) -> Result<(), E> {
        for it in items {
            if let SelectItem::Expr(e, _, _) = it {
                self.expr::<E>(e)?;
            }
        }
        Ok(())
    }

    fn sources<E>(&mut self, from: &mut [FromSource]) -> Result<(), E> {
        for f in from.iter_mut() {
            self.source::<E>(f)?;
            if let Some(on) = &mut f.on {
                self.expr::<E>(on)?;
            }
        }
        Ok(())
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

    fn source<E>(&mut self, f: &mut FromSource) -> Result<(), E> {
        if let Some(d) = &mut f.derived {
            return self.select::<E>(d);
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
            return self.cte::<E>(f, fi, ci);
        }
        if self.catalog.find(self.ca, name.as_slice()).is_some() {
            return Ok(());
        }
        let catalog = self.catalog;
        if let Some(v) = catalog.find_view(self.ca, name.as_slice()) {
            return self.view::<E>(f, v);
        }
        Ok(())
    }

    /// Rewrite `f` into the derived table of view `v`.
    fn view<E>(&mut self, f: &mut FromSource, v: &crate::schema::ViewDef) -> Result<(), E> {
        let name = handle_bytes(self.ca, v.name);
        if self.views.iter().any(|n| eq_ci(n.as_slice(), name)) {
            return Err(schema_err(SchemaKind::CircularView));
        }
        let Ast::CreateView {
            columns,
            mut select,
            ..
        } = crate::sql::parse::<E>(handle_bytes(self.ca, v.sql), self.arena)?
        else {
            return Err(Error::Unsupported);
        };
        // A view's body sees no CTE of the query that reads it.
        let vis = core::mem::take(&mut self.vis);
        self.views.push(copy_vec(name));
        let r = self.select::<E>(&mut select);
        self.views.pop();
        self.vis = vis;
        r?;
        if f.alias.is_none() {
            f.alias = Some(f.table);
        }
        f.derived = Some(Box::new(select));
        f.cte = Some(Box::new(CteUse {
            columns: columns.unwrap_or_default(),
            rec: RecMark::None,
            view: true,
        }));
        Ok(())
    }

    /// Rewrite `f` into the derived table of CTE `ci` of frame
    /// `fi` (the pin's `withExpand`; see the module docs).
    fn cte<E>(&mut self, f: &mut FromSource, fi: usize, ci: usize) -> Result<(), E> {
        let (cte, busy) = {
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
            (cte.clone(), busy)
        };
        match busy {
            Busy::Free => {}
            Busy::Circular => return Err(schema_err(SchemaKind::CircularReference)),
            Busy::MultiRec => return Err(schema_err(SchemaKind::RecursiveReferences)),
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
        let r = self.cte_body::<E>(&mut body, name.as_slice(), fi, ci, id);
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
        self.enter::<E>()?;
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
                    self.depth -= 1;
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
                        }));
                    }
                }
                split = k;
            }
        }
        self.set_busy(fi, ci, Busy::Circular);
        let r = if split < ncores {
            self.rec_body::<E>(body, split, fi, ci)
        } else {
            self.select_body_with::<E>(body)
        };
        self.depth -= 1;
        r?;
        Ok((split < ncores).then_some(split))
    }

    /// `select_body` with the statement's own WITH clause entered.
    fn select_body_with<E>(&mut self, s: &mut SelectStmt) -> Result<(), E> {
        let pushed = match s.with.take() {
            Some(w) => {
                self.push_frame::<E>(*w)?;
                true
            }
            None => false,
        };
        let r = self.select_body::<E>(s);
        if pushed {
            self.pop_frame();
        }
        r
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
            self.core::<E>(body)?;
            for (k, (_, c)) in body.compound.as_mut_slice().iter_mut().enumerate() {
                if k + 1 == split {
                    self.set_busy(fi, ci, Busy::MultiRec);
                }
                self.core::<E>(c)?;
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
                }
            }
        }
        Ok(())
    }
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
        });
        Ok(w)
    }
}

/// The recursive CTE queue (see the module docs): FIFO, or — with
/// an ORDER BY — a binary heap on (ORDER BY key, arrival).
struct RecQueue<'o> {
    order: &'o [(usize, crate::sql::SortOrder)],
    rows: AVec<(u64, AVec<OwnedValue>)>,
    head: usize,
    seq: u64,
}

impl RecQueue<'_> {
    fn less(&self, a: usize, b: usize) -> bool {
        match (self.rows.as_slice().get(a), self.rows.as_slice().get(b)) {
            (Some((sa, ra)), Some((sb, rb))) => {
                compound_order_cmp(ra, rb, self.order).then(sa.cmp(sb)) == core::cmp::Ordering::Less
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
        let mut queue = RecQueue {
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
                compound_key_cmp(r.as_slice(), row.as_slice(), colls.as_slice())
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
