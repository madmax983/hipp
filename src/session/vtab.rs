//! Virtual tables in the session (v0.16, leaf `vtab`): the planner
//! and executor side of `crate::vtab`'s module abstraction.
//!
//! - **FROM sources.** A virtual table of the file, or an eponymous
//!   module named in FROM (`generate_series(1, 9)`, `dbstat`), plans
//!   as a source of the module's declared columns (HIDDEN ones are
//!   not in `*`) with a rowid (`plan_vtab_source`). Its rows come
//!   from the module's cursor when the join reaches it
//!   (`vtab_rows`), once per execution, or once per row of the
//!   sources to its left when a filter argument reads them (the
//!   JSON table-valued functions' lateral machinery).
//! - **xBestIndex.** Once the WHERE clause is resolved
//!   (`plan_vtab_cons`) each `column op expr` term (`=`, `<`, `<=`,
//!   `>`, `>=`, `IS`, either way round; a BETWEEN's two halves) and
//!   each table-valued-function argument (an `=` on the next hidden
//!   column) becomes an `aConstraint` entry, usable when its other
//!   side reads only enclosing queries and sources to the left; the
//!   module picks `argvIndex` / `omit` / `idxNum` / `idxStr`. The
//!   used right-hand sides become the filter's arguments and the
//!   omitted terms leave the WHERE clause, as on the pin.
//! - **Statements.** `CREATE VIRTUAL TABLE` (the pin's stored text;
//!   an R*Tree's shadow tables and root node), DROP TABLE of a
//!   virtual table (its shadow tables with it), and INSERT / UPDATE
//!   / DELETE of a writable one (`xUpdate`: the R*Tree, in
//!   `session::rtree`). Shadow tables are read only, as under the
//!   pin shell's defensive mode.

use super::*;
use crate::query::{
    RefRange, SourceDesc, SubPlan, TVF_LATERAL, TVF_VTAB, and_terms, expr_refs_range,
};
use crate::vtab::{BestErr, Cons, IndexInfo, Module, Op, VTab, VtSrc, Vtab};

/// A planned virtual-table statement (`Plan::Vtab`).
pub(super) enum VtPlan {
    /// CREATE VIRTUAL TABLE: the table's name, its stored text and
    /// declared table.
    Create {
        name: AVec<u8>,
        sql: AVec<u8>,
        tab: VTab,
    },
    /// An IF NOT EXISTS / IF EXISTS no-op.
    Noop,
    /// A virtual table's schema row written under `PRAGMA
    /// writable_schema` (a `.dump` replayed): name and text; no
    /// xCreate (the shadow tables follow as ordinary statements).
    RawRow { name: AVec<u8>, sql: AVec<u8> },
    /// DROP TABLE of a virtual table.
    Drop { name: AVec<u8>, tab: Option<VTab> },
    /// ALTER TABLE ... RENAME TO of an R*Tree or fts5 table (with
    /// its shadow tables, the pin's xRename).
    #[cfg(any(feature = "rtree", feature = "fts5"))]
    Rename { old: AVec<u8>, new: AVec<u8> },
    /// INSERT / UPDATE / DELETE of a writable virtual table.
    #[cfg(feature = "rtree")]
    Write(Box<super::rtree::RtWrite>),
    /// INSERT / UPDATE / DELETE of an fts5 table (leaf `fts5`).
    #[cfg(feature = "fts5")]
    Fts5Write(Box<super::fts5::FtWrite>),
}

/// `(column values, rowid)` rows a module produced.
type VRows = AVec<(AVec<OwnedValue>, Option<i64>)>;

fn vt_err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

impl<S: Storage> Connection<S> {
    /// `INSERT INTO sqlite_schema(type, name, tbl_name, rootpage,
    /// sql) VALUES(...)` under `PRAGMA writable_schema = ON` (v0.16
    /// review fix): the one row a `.dump` writes, the pin shell's and
    /// hipp-cli's, for a virtual table — `'table'`, the name twice, 0
    /// and its `CREATE VIRTUAL TABLE` text, all literals. Other rows
    /// are refused. `None`: not such an INSERT.
    pub(super) fn plan_schema_insert(
        &self,
        arena: &Arena,
        ast: &Ast,
    ) -> Result<Option<Plan>, S::Error> {
        let Ast::Insert {
            table,
            columns,
            rows,
            select: None,
            upsert: None,
            returning,
            ..
        } = ast
        else {
            return Ok(None);
        };
        if !self.prag.writable_schema
            || !crate::schema::Catalog::is_schema_name(handle_bytes(arena, *table))
        {
            return Ok(None);
        }
        let ([row], true) = (rows.as_slice(), returning.is_empty()) else {
            return Err(Error::Unsupported);
        };
        // Each value by the schema column it lands in.
        const COLS: [&[u8]; 5] = [b"type", b"name", b"tbl_name", b"rootpage", b"sql"];
        let mut vals: [OwnedValue; 5] = core::array::from_fn(|_| OwnedValue::Null);
        if row.len() != 5 {
            return Err(Error::Unsupported);
        }
        for (k, e) in row.iter().enumerate() {
            let at = match columns {
                Some(c) => {
                    let n = c.as_slice().get(k).map(|h| handle_bytes(arena, *h));
                    COLS.iter().position(|x| n.is_some_and(|n| eq_ci(n, x)))
                }
                None => Some(k),
            };
            let (Some(at), Expr::Literal(l)) = (at, e) else {
                return Err(Error::Unsupported);
            };
            if let Some(v) = vals.get_mut(at) {
                *v = literal_owned(arena, Some(*l));
            }
        }
        let [
            OwnedValue::Text(ty),
            OwnedValue::Text(name),
            OwnedValue::Text(tbl),
            OwnedValue::Integer(0),
            OwnedValue::Text(sql),
        ] = &vals
        else {
            return Err(Error::Unsupported);
        };
        let ok = ty.as_slice() == b"table"
            && name.as_slice() == tbl.as_slice()
            && matches!(
                crate::sql::parser::parse::<S::Error>(sql.as_slice(), arena, self.cfg.limits()),
                Ok(Ast::CreateVirtual { name: h, .. }) if eq_ci(handle_bytes(arena, h), name.as_slice())
            );
        if !ok {
            return Err(Error::Unsupported);
        }
        if self
            .catalog
            .taken(&self.catalog_arena, name.as_slice())
            .is_some()
        {
            return Err(vt_err(SchemaKind::DuplicateTable));
        }
        Ok(Some(Plan::Vtab(Box::new(VtPlan::RawRow {
            name: name.clone(),
            sql: sql.clone(),
        }))))
    }

    /// `name` is not one a virtual table's module claims for a shadow
    /// table (v0.16 review fix: the pin's "object name reserved for
    /// internal use"; hipp created `t_content` beside a contentless
    /// fts5 table `t`).
    pub(super) fn shadow_name_free(&self, name: &[u8]) -> Result<(), S::Error> {
        // `writable_schema` lifts it, as the pin shell's `.dump`
        // replay (defensive mode off) does.
        if !self.prag.writable_schema && self.catalog.shadow_reserved(&self.catalog_arena, name) {
            return Err(vt_err(SchemaKind::ReservedName));
        }
        Ok(())
    }

    /// The virtual table `name` names (a table of the file, or an
    /// eponymous module no table or view shadows): its declared
    /// table and, for a table of the file, its name.
    pub(super) fn vtab_named(&self, name: &[u8]) -> Option<Result<(VTab, bool), S::Error>> {
        let ca = &self.catalog_arena;
        if let Some(v) = self.catalog.find_vtab(ca, name) {
            return Some(match &v.tab {
                Ok(t) => Ok((t.clone(), true)),
                Err(k) => Err(vt_err(*k)),
            });
        }
        if self.catalog.find_readable(ca, name).is_some()
            || self.catalog.find_view(ca, name).is_some()
        {
            return None;
        }
        let m = Module::lookup(name)?;
        if !m.eponymous() {
            return None;
        }
        Some(
            m.connect(name, &[], false)
                .map(|t| (t, false))
                .map_err(vt_err),
        )
    }

    /// Plan FROM source `s` when it names a virtual table (`None`
    /// otherwise): its description and placeholder plan, as a JSON
    /// table-valued function's. Returns its width.
    pub(super) fn plan_vtab_source(
        &self,
        s: &crate::sql::FromSource,
        arena: &Arena,
        descs: &mut AVec<SourceDesc>,
        plans: &mut AVec<SourcePlan>,
    ) -> Result<Option<usize>, S::Error> {
        let n = handle_bytes(arena, s.table);
        if self.is_temp {
            return Ok(None);
        }
        #[cfg(feature = "attach")]
        if s.schema
            .is_some_and(|h| !eq_ci(handle_bytes(arena, h), b"main"))
        {
            return Ok(None);
        }
        let Some(found) = self.vtab_named(n) else {
            return Ok(None);
        };
        let (tab, own) = found?;
        // A table-valued call's arguments bind to the hidden columns
        // (the pin's "too many arguments on X - max N").
        let nargs = s.args.as_ref().map_or(0, |a| a.len());
        if nargs > tab.hidden_cols().len() {
            return Err(vt_err(SchemaKind::WrongArgumentCount));
        }
        if matches!(s.indexed, Some(crate::sql::IndexHint::By(_))) {
            return Err(vt_err(SchemaKind::UnknownIndex));
        }
        let mut cols = AVec::new();
        let mut names = AVec::new();
        let w = tab.cols.len();
        for c in tab.cols.iter() {
            cols.push((c.name.clone(), c.affinity()));
            names.push(c.name.clone());
        }
        let qual = copy_vec(handle_bytes(arena, s.alias.unwrap_or(s.table)));
        let mut desc = SourceDesc::new(qual, cols, None, true);
        desc.kind = s.kind;
        desc.db = 1;
        desc.vt_visible = if tab.visible() < w { tab.visible() } else { 0 };
        #[cfg(feature = "fts5")]
        if tab.fts.is_some() {
            desc.fts_col = Some(tab.visible());
        }
        descs.push(desc);
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        let mut result_affs = AVec::new();
        let mut result_named = AVec::new();
        for _ in 0..w {
            affs.push(crate::record::Affinity::Blob);
            defaults.push(OwnedValue::Null);
            result_affs.push(None);
            result_named.push(false);
        }
        let holder = SubPlan {
            sources: AVec::new(),
            items: AVec::new(),
            filter: None,
            group_by: AVec::new(),
            having: None,
            order_by: AVec::new(),
            distinct: false,
            limit: None,
            offset: None,
            has_agg: false,
            compound: AVec::new(),
            values: AVec::new(),
            comp_order: AVec::new(),
            result_names: names,
            result_affs,
            result_named,
            id: 0,
            uncorrelated: false,
            scalar_col: 0,
            rec: None,
            mat: 0,
        };
        let vname = if own { copy_vec(n) } else { AVec::new() };
        plans.push(SourcePlan {
            root: 0,
            kind: s.kind,
            on: None,
            rowid_alias: None,
            ncols: w,
            affs,
            defaults,
            derived: Some(Box::new(holder)),
            gcols: None,
            rec: 0,
            tvf: TVF_VTAB,
            tvf_dead: AVec::new(),
            vt: Some(Box::new(VtSrc {
                tab,
                name: vname,
                idx_num: 0,
                idx_str: AVec::new(),
                nargs,
            })),
            #[cfg(feature = "expr_index")]
            access: None,
            #[cfg(feature = "expr_index")]
            hint: crate::query::Hint::Any,
            temp: false,
            #[cfg(feature = "attach")]
            ext: 0,
        });
        Ok(Some(w))
    }

    /// xBestIndex for every virtual-table source of a core (see the
    /// module docs): the constraints the WHERE clause, the inner
    /// joins' ON clauses and the table-valued-function arguments
    /// give, the module's choice, the filter's arguments, and the
    /// omitted terms taken out of `filter` / the ON clauses.
    pub(super) fn plan_vtab_cons(
        &self,
        sources: &mut [SourcePlan],
        cur: &crate::query::ScopeGroup,
        filter: &mut Option<RExpr>,
        order: Option<(&[u8], bool)>,
    ) -> Result<bool, S::Error> {
        let n = sources.len();
        #[cfg_attr(not(feature = "fts5"), allow(unused_mut))]
        let mut consumed = false;
        if !sources.iter().any(|s| s.vt.is_some()) {
            return Ok(consumed);
        }
        let end = cur.offsets.as_slice().last().copied().unwrap_or(0)
            + cur.sources.as_slice().last().map_or(0, |s| s.columns.len());
        let first = cur.offsets.as_slice().first().copied().unwrap_or(end);
        let any_outer = sources.iter().any(|s| s.kind.pads_left());
        for i in 0..n {
            let Some(sp) = sources.get(i) else {
                continue;
            };
            let Some(vt) = sp.vt.as_deref() else {
                continue;
            };
            let here = cur.offsets.as_slice().get(i).copied().unwrap_or(end);
            let me = cur.first_src + i;
            let later = RefRange {
                w: (here, end),
                s: (me, cur.first_src + n),
            };
            let left = RefRange {
                w: (first, here),
                s: (cur.first_src, me),
            };
            let ncols = sp.ncols;
            #[cfg(feature = "fts5")]
            let is_fts = matches!(vt.tab.module, crate::vtab::Module::Fts5);
            let col_of = |e: &RExpr| -> Option<i32> {
                match e {
                    RExpr::Column(slot, ..) if *slot >= here && *slot < here + ncols => {
                        i32::try_from(*slot - here).ok()
                    }
                    RExpr::Rowid(src) if *src == me => Some(-1),
                    _ => None,
                }
            };
            // (constraint, right-hand side, where it came from:
            // 0 = WHERE term k, 1 = ON term k, 2 = argument, 3 =
            // half of a BETWEEN term k of WHERE).
            let mut cands: AVec<(Cons, RExpr, u8, usize)> = AVec::new();
            let mut virt: AVec<(Cons, RExpr, u8, usize)> = AVec::new();
            let mut wterms: AVec<RExpr> = AVec::new();
            if let Some(f) = filter.as_ref() {
                let mut t = AVec::new();
                and_terms(f, &mut t);
                for x in t.iter() {
                    wterms.push((*x).clone());
                }
            }
            let mut oterms: AVec<RExpr> = AVec::new();
            // The ON of a RIGHT / FULL join stays a join condition:
            // the unmatched rows of the module's every row are emitted
            // after the loop (v0.16 review fix: a RIGHT JOIN's ON
            // became lateral filter arguments, which were refused).
            if let Some(o) = sp.on.as_ref()
                && !sp.kind.pads_right()
                && !sp.kind.pads_left()
            {
                let mut t = AVec::new();
                and_terms(o, &mut t);
                for x in t.iter() {
                    oterms.push((*x).clone());
                }
            }
            let push_term =
                |src: u8,
                 k: usize,
                 t: &RExpr,
                 cands: &mut AVec<(Cons, RExpr, u8, usize)>,
                 virt: &mut AVec<(Cons, RExpr, u8, usize)>| {
                    let usable = |r: &RExpr| !expr_refs_range(r, &later);
                    match t {
                        RExpr::Binary(op, l, r) => {
                            let o = match op {
                                BinOp::Eq => Op::Eq,
                                BinOp::Lt => Op::Lt,
                                BinOp::Le => Op::Le,
                                BinOp::Gt => Op::Gt,
                                BinOp::Ge => Op::Ge,
                                BinOp::Is => Op::Is,
                                _ => return,
                            };
                            let flip = |o: Op| match o {
                                Op::Lt => Op::Gt,
                                Op::Le => Op::Ge,
                                Op::Gt => Op::Lt,
                                Op::Ge => Op::Le,
                                x => x,
                            };
                            if let Some(c) = col_of(l)
                                && col_of(r).is_none()
                            {
                                cands.push((
                                    Cons {
                                        col: c,
                                        op: o,
                                        usable: usable(r),
                                    },
                                    (**r).clone(),
                                    src,
                                    k,
                                ));
                            } else if let Some(c) = col_of(r)
                                && col_of(l).is_none()
                            {
                                cands.push((
                                    Cons {
                                        col: c,
                                        op: flip(o),
                                        usable: usable(l),
                                    },
                                    (**l).clone(),
                                    src,
                                    k,
                                ));
                            }
                        }
                        // `x MATCH q` (leaf `fts5`): the pin's
                        // SQLITE_INDEX_CONSTRAINT_MATCH on column x.
                        #[cfg(feature = "fts5")]
                        RExpr::Scalar(crate::query::ScalarFn::Match, a) => {
                            if let (Some(q), Some(x)) = (a.as_slice().first(), a.as_slice().get(1))
                                && let Some(c) = col_of(x)
                                && col_of(q).is_none()
                            {
                                cands.push((
                                    Cons {
                                        col: c,
                                        op: Op::Match,
                                        usable: usable(q),
                                    },
                                    q.clone(),
                                    src,
                                    k,
                                ));
                            }
                        }
                        // `x LIKE p` / `x GLOB p` (two arguments): the
                        // pin's SQLITE_INDEX_CONSTRAINT_LIKE / _GLOB,
                        // which only fts5 tables use.
                        #[cfg(feature = "fts5")]
                        RExpr::Like(x, p, false) if is_fts => {
                            if let Some(c) = col_of(x)
                                && col_of(p).is_none()
                            {
                                cands.push((
                                    Cons {
                                        col: c,
                                        op: Op::Like,
                                        usable: usable(p),
                                    },
                                    (**p).clone(),
                                    src,
                                    k,
                                ));
                            }
                        }
                        #[cfg(feature = "fts5")]
                        RExpr::Scalar(
                            f @ (crate::query::ScalarFn::Like | crate::query::ScalarFn::Glob),
                            a,
                        ) if is_fts && a.len() == 2 => {
                            if let (Some(p), Some(x)) = (a.as_slice().first(), a.as_slice().get(1))
                                && let Some(c) = col_of(x)
                                && col_of(p).is_none()
                            {
                                let op = if *f == crate::query::ScalarFn::Like {
                                    Op::Like
                                } else {
                                    Op::Glob
                                };
                                cands.push((
                                    Cons {
                                        col: c,
                                        op,
                                        usable: usable(p),
                                    },
                                    p.clone(),
                                    src,
                                    k,
                                ));
                            }
                        }
                        RExpr::Between(x, lo, hi, false) if src == 0 => {
                            if let Some(c) = col_of(x) {
                                virt.push((
                                    Cons {
                                        col: c,
                                        op: Op::Ge,
                                        usable: usable(lo),
                                    },
                                    (**lo).clone(),
                                    3,
                                    k,
                                ));
                                virt.push((
                                    Cons {
                                        col: c,
                                        op: Op::Le,
                                        usable: usable(hi),
                                    },
                                    (**hi).clone(),
                                    3,
                                    k,
                                ));
                            }
                        }
                        _ => {}
                    }
                };
            let pushable = !sp.kind.pads_right() && !any_outer;
            if pushable {
                for (k, t) in wterms.iter().enumerate() {
                    push_term(0, k, t, &mut cands, &mut virt);
                }
            }
            for (k, t) in oterms.iter().enumerate() {
                push_term(1, k, t, &mut cands, &mut virt);
            }
            let hidden = vt.tab.hidden_cols();
            if let Some(args) = sp
                .derived
                .as_deref()
                .and_then(|d| d.values.as_slice().first())
            {
                for (k, a) in args.iter().enumerate().take(vt.nargs) {
                    let col = hidden.as_slice().get(k).copied().unwrap_or(0);
                    cands.push((
                        Cons {
                            col: i32::try_from(col).unwrap_or(0),
                            op: Op::Eq,
                            usable: !expr_refs_range(a, &later),
                        },
                        a.clone(),
                        2,
                        k,
                    ));
                }
            }
            // The pin's BETWEEN halves come last, later terms' first.
            for pair in virt.as_slice().chunks(2).rev() {
                for c in pair.iter() {
                    cands.push(c.clone());
                }
            }
            let mut cons = AVec::new();
            for c in cands.iter() {
                cons.push(c.0);
            }
            let module = vt.tab.module;
            let mut aorder = AVec::new();
            // Only fts5 reads the ORDER BY (its rank and rowid orders);
            // hipp sorts every result itself.
            #[cfg(feature = "fts5")]
            let order = order.filter(|_| module == Module::Fts5);
            #[cfg(not(feature = "fts5"))]
            let order: Option<(&[u8], bool)> = order.filter(|_| false);
            if let Some((name, desc)) = order {
                if let Some(c) = vt
                    .tab
                    .cols
                    .iter()
                    .position(|c| eq_ci(c.name.as_slice(), name))
                {
                    aorder.push((c as i32, desc));
                } else if eq_ci(name, b"rowid") || eq_ci(name, b"oid") || eq_ci(name, b"_rowid_") {
                    aorder.push((-1, desc));
                }
            }
            let mut info = IndexInfo::new(cons.clone(), aorder);
            match module.best_index(&vt.tab, &mut info) {
                Ok(()) => {}
                Err(BestErr::Err(k)) => return Err(vt_err(k)),
                Err(BestErr::Unusable) => return Err(Error::Unsupported),
            }
            // The pin also asks with only what needs no other source
            // (`whereLoopAddVirtual`'s last calls): generate_series
            // errors there when its start needs one.
            let lateral_cons = cands
                .iter()
                .any(|c| c.0.usable && expr_refs_range(&c.1, &left));
            if lateral_cons && module == Module::Series {
                let mut c2 = cons.clone();
                for (c, cand) in c2.as_mut_slice().iter_mut().zip(cands.iter()) {
                    if expr_refs_range(&cand.1, &left) {
                        c.usable = false;
                    }
                }
                let mut i2 = IndexInfo::new(c2, AVec::new());
                if let Err(BestErr::Err(k)) = module.best_index(&vt.tab, &mut i2) {
                    return Err(vt_err(k));
                }
            }
            let nargv = info.nargs();
            let mut argv: AVec<RExpr> = AVec::new();
            for _ in 0..nargv {
                argv.push(RExpr::Literal(OwnedValue::Null));
            }
            let mut drop_w: AVec<usize> = AVec::new();
            let mut drop_o: AVec<usize> = AVec::new();
            let mut between: AVec<(usize, u8)> = AVec::new();
            for (u, c) in info.usage.iter().zip(cands.iter()) {
                if u.argv == 0 {
                    continue;
                }
                if let Some(slot) = argv.as_mut_slice().get_mut(usize::from(u.argv) - 1) {
                    *slot = c.1.clone();
                }
                if u.omit {
                    match c.2 {
                        0 => drop_w.push(c.3),
                        1 => drop_o.push(c.3),
                        3 => match between.as_mut_slice().iter_mut().find(|b| b.0 == c.3) {
                            Some(b) => b.1 += 1,
                            None => between.push((c.3, 1)),
                        },
                        _ => {}
                    }
                }
            }
            for (k, cnt) in between.iter() {
                if *cnt >= 2 {
                    drop_w.push(*k);
                }
            }
            let lateral = argv.iter().any(|a| expr_refs_range(a, &left));
            if lateral && sp.kind.pads_left() {
                return Err(Error::Unsupported);
            }
            let rebuild = |terms: &AVec<RExpr>, drop: &AVec<usize>| -> Option<RExpr> {
                let mut out = None;
                for (k, t) in terms.iter().enumerate() {
                    if !drop.iter().any(|d| *d == k) {
                        out = Some(and_expr(out, t.clone()));
                    }
                }
                out
            };
            if !drop_w.is_empty() {
                *filter = rebuild(&wterms, &drop_w);
            }
            let Some(sp) = sources.get_mut(i) else {
                continue;
            };
            if !drop_o.is_empty() {
                sp.on = rebuild(&oterms, &drop_o);
            }
            sp.tvf = if lateral {
                TVF_VTAB | TVF_LATERAL
            } else {
                TVF_VTAB
            };
            if let Some(d) = sp.derived.as_deref_mut() {
                d.values.clear();
                d.values.push(argv);
            }
            // An fts5 table's rank order is its own (the pin's
            // orderByConsumed); hipp sorts every other.
            #[cfg(feature = "fts5")]
            if module == Module::Fts5 && info.idx_num & 1 != 0 {
                consumed = true;
            }
            if let Some(v) = sp.vt.as_deref_mut() {
                v.idx_num = info.idx_num;
                v.idx_str = info.idx_str;
            }
        }
        Ok(consumed)
    }

    /// The statements that name a virtual table or a shadow table
    /// (`None`: any other statement): CREATE VIRTUAL TABLE, DROP
    /// TABLE, writes, and the pin's refusals (a shadow table may
    /// not be written or dropped; a virtual table may not be
    /// indexed or altered).
    pub(super) fn plan_vtab_stmt(
        &self,
        arena: &Arena,
        ast: &Ast,
    ) -> Result<Option<Plan>, S::Error> {
        if self.is_temp {
            return Ok(None);
        }
        let ca = &self.catalog_arena;
        let name_of = |h: crate::arena::ABytes| handle_bytes(arena, h);
        if let Ast::CreateVirtual {
            name,
            if_not_exists,
            module,
            args,
            text,
        } = ast
        {
            return self
                .plan_create_vtab(
                    arena,
                    name_of(*name),
                    *if_not_exists,
                    name_of(*module),
                    args,
                    *text,
                )
                .map(Some);
        }
        let target = match ast {
            Ast::Insert { table, .. } | Ast::Update { table, .. } | Ast::Delete { table, .. } => {
                Some((*table, 0u8))
            }
            Ast::DropTable { name, .. } => Some((*name, 1)),
            #[cfg(feature = "indexes")]
            Ast::CreateIndex { table, .. } => Some((*table, 2)),
            #[cfg(feature = "triggers")]
            Ast::CreateTrigger(t) => Some((t.table, 4)),
            Ast::AlterAddColumn { table, .. } => Some((*table, 3)),
            Ast::AlterRename { table, .. }
            | Ast::AlterRenameColumn { table, .. }
            | Ast::AlterDropColumn { table, .. } => Some((*table, 3)),
            _ => None,
        };
        let Some((h, what)) = target else {
            return Ok(None);
        };
        let t = name_of(h);
        // A shadow table may not be written, dropped, altered or
        // given triggers (the pin shell's defensive mode); nor
        // indexed here (the pin allows it, but hipp's R*Tree writes
        // would not maintain the index).
        // `PRAGMA writable_schema` (a `.dump` replayed) writes them.
        if self.catalog.is_shadow(ca, t) && !(what == 0 && self.prag.writable_schema) {
            return Err(vt_err(if what == 1 {
                SchemaKind::ShadowNoDrop
            } else {
                SchemaKind::ShadowReadOnly
            }));
        }
        let Some(v) = self.catalog.find_vtab(ca, t) else {
            return Ok(None);
        };
        match what {
            // xDestroy needs the module (the pin's "no such module").
            1 => Ok(Some(Plan::Vtab(Box::new(VtPlan::Drop {
                name: copy_vec(t),
                tab: Some(v.tab.clone().map_err(vt_err)?),
            })))),
            2 | 4 => Err(vt_err(SchemaKind::VtabNotIndexable)),
            #[cfg(any(feature = "rtree", feature = "fts5"))]
            3 if matches!(ast, Ast::AlterRename { .. }) => self.plan_vtab_rename(arena, ast, t, v),
            3 => Err(vt_err(SchemaKind::VtabNotAlterable)),
            _ => {
                let tab = v.tab.clone().map_err(vt_err)?;
                if !tab.module.writable() {
                    return Err(vt_err(SchemaKind::VtabReadOnly));
                }
                #[cfg(feature = "fts5")]
                if tab.fts.is_some() {
                    return self.plan_fts5_write(arena, ast, t, tab).map(Some);
                }
                #[cfg(feature = "rtree")]
                {
                    self.plan_rtree_write(arena, ast, t, tab).map(Some)
                }
                #[cfg(not(feature = "rtree"))]
                {
                    let _ = tab;
                    Err(vt_err(SchemaKind::VtabReadOnly))
                }
            }
        }
    }

    /// Plan `ALTER TABLE vt RENAME TO new` of R*Tree or fts5 table
    /// `vt` (another module's table cannot be renamed here; neither
    /// can one while the schema has views or triggers, whose text the
    /// pin would edit too).
    #[cfg(any(feature = "rtree", feature = "fts5"))]
    fn plan_vtab_rename(
        &self,
        arena: &Arena,
        ast: &Ast,
        t: &[u8],
        v: &crate::schema::VtabDef,
    ) -> Result<Option<Plan>, S::Error> {
        let Ast::AlterRename { to, .. } = ast else {
            return Err(Error::Unsupported);
        };
        let Some(sfxs) = v.tab.as_ref().ok().and_then(shadow_suffixes) else {
            return Err(vt_err(SchemaKind::VtabNotAlterable));
        };
        #[cfg(feature = "triggers")]
        let trig = !self.catalog.triggers.is_empty();
        #[cfg(not(feature = "triggers"))]
        let trig = false;
        if trig || !self.catalog.views.is_empty() {
            return Err(vt_err(SchemaKind::VtabNotAlterable));
        }
        let new = handle_bytes(arena, *to);
        reserved_name::<S::Error>(new)?;
        self.shadow_name_free(new)?;
        let ca = &self.catalog_arena;
        for sfx in core::iter::once(b"" as &[u8]).chain(sfxs.iter().copied()) {
            let mut n = copy_vec(new);
            n.extend_from_slice(sfx);
            if self.catalog.taken(ca, n.as_slice()).is_some() {
                return Err(vt_err(SchemaKind::DuplicateTable));
            }
        }
        Ok(Some(Plan::Vtab(Box::new(VtPlan::Rename {
            old: copy_vec(t),
            new: copy_vec(new),
        }))))
    }

    fn plan_create_vtab(
        &self,
        arena: &Arena,
        name: &[u8],
        if_not_exists: bool,
        module: &[u8],
        args: &AVec<crate::arena::ABytes>,
        text: crate::arena::ABytes,
    ) -> Result<Plan, S::Error> {
        reserved_name::<S::Error>(name)?;
        let ca = &self.catalog_arena;
        match self.catalog.taken(ca, name) {
            Some(true) => return Err(vt_err(SchemaKind::DuplicateIndex)),
            Some(false) if if_not_exists => return Ok(Plan::Vtab(Box::new(VtPlan::Noop))),
            Some(false) => return Err(vt_err(SchemaKind::DuplicateTable)),
            None => {}
        }
        self.shadow_name_free(name)?;
        let m = Module::lookup(module)
            .filter(|m| m.creatable())
            .ok_or(vt_err(SchemaKind::NoSuchModule))?;
        let mut spans: AVec<&[u8]> = AVec::new();
        for a in args.iter() {
            spans.push(handle_bytes(arena, *a));
        }
        let tab = m.connect(name, spans.as_slice(), true).map_err(vt_err)?;
        // dbstat's schema argument must name a database (v0.16 review
        // fix: the pin's "no such database"; hipp made a table that
        // read no rows).
        if m == Module::Dbstat && !tab.schema.is_empty() {
            let d = tab.schema.as_slice();
            #[cfg(feature = "attach")]
            let attached = self.att_index(d).is_some();
            #[cfg(not(feature = "attach"))]
            let attached = false;
            if !(eq_ci(d, b"main") || eq_ci(d, b"temp") || attached) {
                return Err(vt_err(SchemaKind::UnknownDatabase));
            }
        }
        // The shadow tables xCreate makes must be free (v0.16 review
        // fix: hipp wrote a second `r_node` beside the user's table,
        // a schema the pin refuses as malformed).
        #[cfg(any(feature = "rtree", feature = "fts5"))]
        for sfx in created_shadows(&tab).iter() {
            let mut n = copy_vec(name);
            n.extend_from_slice(sfx);
            if self.catalog.taken(ca, n.as_slice()).is_some() {
                return Err(vt_err(SchemaKind::DuplicateTable));
            }
        }
        let mut stored = copy_vec(b"CREATE VIRTUAL TABLE ");
        stored.extend_from_slice(handle_bytes(arena, text));
        Ok(Plan::Vtab(Box::new(VtPlan::Create {
            name: copy_vec(name),
            sql: stored,
            tab,
        })))
    }
}

impl<S: Storage> Statement<'_, S> {
    /// Run a `Plan::Vtab` statement.
    pub(super) async fn step_vtab(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_vtab().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            #[cfg(feature = "rtree")]
            Err(e) if matches!(&self.plan, Plan::Vtab(p) if matches!(**p, VtPlan::Write(_))) => {
                Err(self.write_failed(e).await)
            }
            #[cfg(feature = "fts5")]
            Err(e) if matches!(&self.plan, Plan::Vtab(p) if matches!(**p, VtPlan::Fts5Write(_))) => {
                Err(self.write_failed(e).await)
            }
            Err(e) => {
                abort_write!(self, &e);
                Err(e)
            }
        }
    }

    async fn exec_vtab(&mut self) -> Result<(), S::Error> {
        let Plan::Vtab(p) = &self.plan else {
            return Err(Error::Unsupported);
        };
        match &**p {
            VtPlan::Noop => Ok(()),
            VtPlan::RawRow { name, sql } => {
                let (name, sql) = (name.clone(), sql.clone());
                self.insert_schema_row(
                    b"table",
                    name.as_slice(),
                    name.as_slice(),
                    0,
                    Some(sql.as_slice()),
                )
                .await?;
                self.conn.pager.bump_schema_cookie();
                if !self.conn.explicit_txn {
                    auto_commit!(self.conn)?;
                }
                Ok(())
            }
            VtPlan::Create { name, sql, tab } => {
                let (name, sql, tab) = (name.clone(), sql.clone(), tab.clone());
                self.insert_schema_row(
                    b"table",
                    name.as_slice(),
                    name.as_slice(),
                    0,
                    Some(sql.as_slice()),
                )
                .await?;
                #[cfg(feature = "rtree")]
                if let Module::Rtree { .. } = tab.module {
                    self.rtree_create(name.as_slice(), &tab).await?;
                }
                #[cfg(feature = "fts5")]
                if tab.fts.is_some() {
                    self.fts5_create(name.as_slice(), &tab).await?;
                }
                let _ = tab;
                self.conn.pager.bump_schema_cookie();
                if !self.conn.explicit_txn {
                    auto_commit!(self.conn)?;
                }
                Ok(())
            }
            VtPlan::Drop { name, tab } => {
                let (name, tab) = (name.clone(), tab.clone());
                self.drop_vtab(name.as_slice(), tab.as_ref()).await
            }
            #[cfg(feature = "rtree")]
            VtPlan::Write(_) => self.exec_rtree_write().await,
            #[cfg(feature = "fts5")]
            VtPlan::Fts5Write(_) => self.exec_fts5_write().await,
            #[cfg(any(feature = "rtree", feature = "fts5"))]
            VtPlan::Rename { old, new } => {
                let (old, new) = (old.clone(), new.clone());
                self.rename_vtab(old.as_slice(), new.as_slice()).await
            }
        }
    }

    /// Rename R*Tree or fts5 table `old` and its shadow tables: each
    /// schema row's name and the name token of its text, which takes
    /// the pin's quoted form (`CREATE VIRTUAL TABLE "new" USING ...`,
    /// `CREATE TABLE "new_node"(...)`). An fts5 table's pending terms
    /// follow it.
    #[cfg(any(feature = "rtree", feature = "fts5"))]
    async fn rename_vtab(&mut self, old: &[u8], new: &[u8]) -> Result<(), S::Error> {
        let tab = self
            .conn
            .catalog
            .find_vtab(&self.conn.catalog_arena, old)
            .and_then(|v| v.tab.as_ref().ok())
            .cloned()
            .ok_or(vt_err(SchemaKind::VtabNotAlterable))?;
        let sfxs = shadow_suffixes(&tab).ok_or(vt_err(SchemaKind::VtabNotAlterable))?;
        // fts5's xRename syncs the table first.
        #[cfg(feature = "fts5")]
        if let Some(cfg) = tab.fts.as_deref() {
            self.conn.fts5_sync_one(old, cfg).await?;
            self.conn.fts5_renamed(old, new);
        }
        // (schema rowid, new name, type, root page, new text).
        type Edit = (i64, AVec<u8>, AVec<u8>, i64, AVec<u8>);
        let mut edits: AVec<Edit> = AVec::new();
        {
            let mut cursor = ScanCursor::new(1);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
                let (
                    Some(Value::Text(ty)),
                    Some(Value::Text(n)),
                    Some(Value::Integer(root)),
                    Some(Value::Text(sql)),
                ) = (
                    values.as_slice().first(),
                    values.as_slice().get(1),
                    values.as_slice().get(3),
                    values.as_slice().get(4),
                )
                else {
                    continue;
                };
                if !eq_ci(ty, b"table") {
                    continue;
                }
                let sfx = if eq_ci(n, old) {
                    &[][..]
                } else if n.len() > old.len() && n.get(..old.len()).is_some_and(|p| eq_ci(p, old)) {
                    match n.get(old.len()..) {
                        Some(x) if sfxs.iter().any(|s| eq_ci(s, x)) => x,
                        _ => continue,
                    }
                } else {
                    continue;
                };
                let mut name = copy_vec(new);
                name.extend_from_slice(sfx);
                let head: &[u8] = if sfx.is_empty() {
                    b"CREATE VIRTUAL TABLE "
                } else {
                    b"CREATE TABLE "
                };
                let at = head.len().min(sql.len());
                let end = crate::sql::parser::token_end(sql, at);
                let mut text = copy_vec(head);
                text.push(b'"');
                for b in name.iter() {
                    if *b == b'"' {
                        text.push(b'"');
                    }
                    text.push(*b);
                }
                text.push(b'"');
                text.extend_from_slice(sql.get(end..).unwrap_or(&[]));
                edits.push((rowid, name, copy_vec(ty), *root, text));
            }
        }
        for (rowid, name, ty, root, text) in edits.iter() {
            let mut rec = AVec::new();
            encode_record(
                &[
                    Value::Text(ty.as_slice()),
                    Value::Text(name.as_slice()),
                    Value::Text(name.as_slice()),
                    Value::Integer(*root),
                    Value::Text(text.as_slice()),
                ],
                &mut rec,
            );
            btree::btree_delete(&mut self.conn.pager, 1, *rowid).await?;
            btree::btree_insert(&mut self.conn.pager, 1, *rowid, rec.as_slice()).await?;
        }
        self.conn.pager.bump_schema_cookie();
        if !self.conn.explicit_txn {
            auto_commit!(self.conn)?;
        }
        Ok(())
    }

    /// DROP TABLE of a virtual table: its schema row, then (xDestroy)
    /// each shadow table in the pin's order.
    async fn drop_vtab(&mut self, name: &[u8], tab: Option<&VTab>) -> Result<(), S::Error> {
        let mut gone = AVec::new();
        {
            let mut cursor = ScanCursor::new(1);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
                let hit = matches!(values.as_slice().first(), Some(Value::Text(t)) if eq_ci(t, b"table"))
                    && matches!(values.as_slice().get(1), Some(Value::Text(n)) if eq_ci(n, name));
                if hit {
                    gone.push(rowid);
                }
            }
        }
        for r in gone.iter() {
            btree::btree_delete(&mut self.conn.pager, 1, *r).await?;
        }
        #[cfg(feature = "rtree")]
        if tab.is_some_and(|t| matches!(t.module, Module::Rtree { .. })) {
            for sfx in [b"_node" as &[u8], b"_rowid", b"_parent"] {
                let mut sname = copy_vec(name);
                sname.extend_from_slice(sfx);
                self.drop_plain_table(sname.as_slice()).await?;
            }
        }
        #[cfg(feature = "fts5")]
        if let Some(t) = tab
            && t.fts.is_some()
        {
            self.fts5_forget(name);
            for sfx in Self::fts5_shadows(t).iter() {
                let mut sname = copy_vec(name);
                sname.extend_from_slice(sfx);
                self.drop_plain_table(sname.as_slice()).await?;
            }
        }
        let _ = tab;
        self.drop_table_end().await
    }

    /// Drop the ordinary table `name` (no indexes, no triggers): its
    /// schema row and its pages, as DROP TABLE does.
    #[cfg(any(feature = "rtree", feature = "fts5"))]
    async fn drop_plain_table(&mut self, name: &[u8]) -> Result<(), S::Error> {
        let Some(root) = self
            .conn
            .catalog
            .find(&self.conn.catalog_arena, name)
            .map(|t| t.root)
        else {
            return Ok(());
        };
        let mut gone = AVec::new();
        {
            let mut cursor = ScanCursor::new(1);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let values = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
                if matches!(values.as_slice().get(1), Some(Value::Text(n)) if eq_ci(n, name)) {
                    gone.push(rowid);
                }
            }
        }
        for r in gone.iter() {
            btree::btree_delete(&mut self.conn.pager, 1, *r).await?;
        }
        #[cfg(feature = "autovacuum")]
        if self.conn.pager.autovac() {
            let moved = crate::autovac::drop_tree(&mut self.conn.pager, root).await?;
            if moved != 0 {
                crate::autovac::schema_set_root(&mut self.conn.pager, moved, root).await?;
                // The moved tree's root, for the shadow tables still
                // to drop.
                if let Some(t) = self
                    .conn
                    .catalog
                    .tables
                    .as_mut_slice()
                    .iter_mut()
                    .find(|t| t.root == moved)
                {
                    t.root = root;
                }
            }
            return Ok(());
        }
        let pages = btree::collect_table_pages(&mut self.conn.pager, root).await?;
        for p in pages.iter() {
            self.conn.pager.free_page(*p).await?;
        }
        Ok(())
    }

    /// The rows of virtual-table source `sp` for the row `row`
    /// (enclosing values, the sources before it, the rest NULL):
    /// its filter arguments evaluated, then its module's cursor read
    /// to the end (at most `need` rows).
    pub(super) async fn vtab_rows(
        &mut self,
        sp: &SourcePlan,
        vt: &VtSrc,
        row: &RowData,
        need: Option<usize>,
    ) -> Result<VRows, S::Error> {
        let mut argv = AVec::new();
        if let Some(d) = sp.derived.as_deref()
            && let Some(r) = d.values.as_slice().first()
        {
            for a in r.iter() {
                argv.push(self.eval_row_sub(a, row).await?);
            }
        }
        match vt.tab.module {
            Module::Series => {
                let mut c = crate::vtab::SeriesCursor::filter(vt.idx_num, argv.as_slice());
                crate::vtab::drain(&mut c, 4, need, self.conn.cfg.max_vtab_rows)
                    .map_err(|kind| Error::Schema { kind })
            }
            Module::Dbstat => self.dbstat_source(vt, argv.as_slice(), need).await,
            #[cfg(feature = "rtree")]
            Module::Rtree { .. } => self.rtree_rows(vt, argv.as_slice(), need).await,
            #[cfg(feature = "rtree")]
            Module::Check => self.rtreecheck_rows(argv.as_slice()).await,
            #[cfg(feature = "fts5")]
            Module::Fts5 => self.fts5_rows(vt, argv.as_slice(), need).await,
            #[cfg(feature = "fts5")]
            Module::Fts5Vocab => self.vocab_rows(vt).await,
        }
    }

    /// dbstat's rows: `sqlite_schema` and every b-tree the schema
    /// names, in its rowid order (name order when asked), each
    /// walked as `statNext` walks it — of `main`, `temp` (v0.16
    /// review fix: no rows came for it) or an attached database.
    async fn dbstat_source(
        &mut self,
        vt: &VtSrc,
        argv: &[OwnedValue],
        need: Option<usize>,
    ) -> Result<VRows, S::Error> {
        let a = crate::vtab::stat_args(vt.idx_num, argv);
        let schema: AVec<u8> = match &a.schema {
            Some(s) => s.clone(),
            None if !vt.tab.schema.is_empty() => vt.tab.schema.clone(),
            None => copy_vec(b"main"),
        };
        // `name = NULL`: no rows.
        let null_name =
            a.name.is_none() && argv.len() > usize::from(a.schema.is_some()) && vt.idx_num & 2 != 0;
        if eq_ci(schema.as_slice(), b"main") {
            return dbstat_db(&mut self.conn.pager, &a, null_name, b"main", need).await;
        }
        if eq_ci(schema.as_slice(), b"temp") {
            return match self.conn.temp.as_deref_mut() {
                Some(c) => dbstat_db(&mut c.pager, &a, null_name, b"temp", need)
                    .await
                    .map_err(super::temp::mem_err),
                None => Ok(AVec::new()),
            };
        }
        #[cfg(feature = "attach")]
        if let Some(i) = self.conn.att_index(schema.as_slice())
            && let Some(at) = self.conn.attached.as_mut_slice().get_mut(i)
        {
            if self.conn.explicit_txn {
                at.used.set(true);
            }
            let label = at.name.clone();
            return dbstat_db(&mut at.conn.pager, &a, null_name, label.as_slice(), need).await;
        }
        // A database not attached: no rows (the pin's too).
        Ok(AVec::new())
    }
}

/// `dbstat_source` over one database's pager (`label` is its name in
/// the `schema` column).
async fn dbstat_db<T: Storage>(
    pager: &mut Pager<T>,
    a: &crate::vtab::StatArgs,
    null_name: bool,
    label: &[u8],
    need: Option<usize>,
) -> Result<VRows, T::Error> {
    // A database not yet written has no pages.
    if pager.pristine() || null_name {
        return Ok(AVec::new());
    }
    let mut trees: AVec<(AVec<u8>, u32)> = AVec::new();
    trees.push((copy_vec(b"sqlite_schema"), 1));
    if pager.page_count() > 0 {
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        while cursor.next_row_into(pager, &mut buf).await?.is_some() {
            let values = crate::record::decode_record::<T::Error>(buf.as_slice(), 5)?;
            let (Some(Value::Text(n)), Some(Value::Integer(r))) =
                (values.as_slice().get(1), values.as_slice().get(3))
            else {
                continue;
            };
            if *r != 0 {
                trees.push((copy_vec(n), u32::try_from(*r).unwrap_or(0)));
            }
        }
    }
    if let Some(n) = &a.name {
        let mut kept = AVec::new();
        for t in trees.iter() {
            if t.0.as_slice() == n.as_slice() {
                kept.push(t.clone());
            }
        }
        trees = kept;
    }
    if a.ordered {
        trees
            .as_mut_slice()
            .sort_by(|x, y| x.0.as_slice().cmp(y.0.as_slice()));
    }
    crate::vtab::dbstat_rows(pager, trees.as_slice(), label, a.agg, need).await
}

/// The suffixes of the shadow tables xCreate makes for `t` (v0.16
/// review fix): an R*Tree's three; an fts5 table's `_data`, `_idx`,
/// `_config`, and `_content` / `_docsize` when its options keep them.
#[cfg(any(feature = "rtree", feature = "fts5"))]
fn created_shadows(t: &VTab) -> AVec<&'static [u8]> {
    let mut out: AVec<&'static [u8]> = AVec::new();
    #[cfg(feature = "fts5")]
    if let Some(c) = t.fts.as_deref() {
        out.extend_from_slice(&[b"_data", b"_idx", b"_config"]);
        if c.has_content() {
            out.push(b"_content");
        }
        if c.columnsize {
            out.push(b"_docsize");
        }
        return out;
    }
    if let Some(s) = shadow_suffixes(t) {
        out.extend_from_slice(s);
    }
    out
}

/// The shadow tables' suffixes of a module whose tables can be
/// renamed (R*Tree, fts5); None for the others.
#[cfg(any(feature = "rtree", feature = "fts5"))]
fn shadow_suffixes(t: &VTab) -> Option<&'static [&'static [u8]]> {
    #[cfg(feature = "rtree")]
    if matches!(t.module, Module::Rtree { .. }) {
        return Some(&[b"_node", b"_parent", b"_rowid"]);
    }
    #[cfg(feature = "fts5")]
    if t.fts.is_some() {
        return Some(&[b"_data", b"_idx", b"_content", b"_docsize", b"_config"]);
    }
    let _ = t;
    None
}
