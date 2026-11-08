//! The R*Tree in the session (v0.16, leaf `rtree`): CREATE (the
//! shadow tables and the empty root), reads (`rtreeFilter`: a rowid
//! lookup or a constrained depth-first walk), writes (`rtreeUpdate`
//! for each row of an INSERT / UPDATE / DELETE), and `rtreecheck()`.
//!
//! The tree algorithm is `crate::rtree`'s, a pure function of the
//! shadow-table rows it reads: each write runs over a [`Store`] of
//! the rows read so far, and when it needs one more ([`Miss`]) the
//! row is read from its b-tree here and the write runs again. Its
//! result ([`crate::rtree::Writes`]) is then applied to the shadow
//! tables through the pager, in the statement's transaction — the
//! journal, statement savepoints and crash recovery cover it as any
//! table write.

use super::vtab::VtPlan;
use super::*;
use crate::rtree::{Fail, Found, Geom, Miss, NewRow, Search, Store, Tree};
use crate::vtab::{Module, VTab, VtSrc};

/// The shape of a write (`RtWrite::kind`).
#[derive(Clone)]
pub(super) enum RtKind {
    /// INSERT: where each supplied value goes (`Some(column)`, or
    /// `None` for the rowid) and the rows (VALUES, or a SELECT's).
    Insert {
        targets: AVec<Option<usize>>,
        rows: AVec<AVec<RExpr>>,
        select: Option<Box<crate::query::SubPlan>>,
    },
    /// UPDATE (`sets`: the assigned column of each extra result
    /// item) or DELETE (`None`): the rows of `SELECT *, rowid,
    /// <set expressions> FROM t WHERE ...`.
    Change {
        select: Box<crate::query::SubPlan>,
        sets: Option<AVec<Option<usize>>>,
    },
}

/// A planned INSERT / UPDATE / DELETE of an R*Tree.
pub(super) struct RtWrite {
    pub conflict: ConflictAction,
    name: AVec<u8>,
    tab: VTab,
    kind: RtKind,
}

/// The roots of an R*Tree's shadow tables and its geometry.
struct Shadow {
    node: u32,
    parent: u32,
    rowid: u32,
    g: Geom,
    naux: usize,
}

/// An open R*Tree cursor (v0.17; `Statement::rtree_open`): the
/// table's shadow tables and either its search in progress (and the
/// node image it asked for) or the one cell a rowid lookup found.
pub(super) struct RtCursor {
    sh: Shadow,
    search: Option<Search>,
    one: Option<crate::rtree::Cell>,
    img: Option<AVec<u8>>,
}

fn rt_err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

fn corrupt<E>() -> Error<E> {
    rt_err(SchemaKind::VtabCorrupt)
}

/// The field `i` of a record as an owned value (NULL past its end).
fn field<E>(payload: &[u8], i: usize) -> Result<OwnedValue, E> {
    let vals = crate::record::decode_record::<E>(payload, i + 1)?;
    Ok(vals
        .as_slice()
        .get(i)
        .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)))
}

impl<S: Storage> Connection<S> {
    /// Plan an INSERT / UPDATE / DELETE of R*Tree `name`.
    pub(super) fn plan_rtree_write(
        &self,
        arena: &Arena,
        ast: &Ast,
        name: &[u8],
        tab: VTab,
    ) -> Result<Plan, S::Error> {
        use crate::query::{ScopeGroup, has_agg, resolve_scoped};
        // An index on a shadow table (one the pin made) would not be
        // maintained by these writes.
        let ca = &self.catalog_arena;
        for sfx in [b"_node" as &[u8], b"_parent", b"_rowid"] {
            let mut n = copy_vec(name);
            n.extend_from_slice(sfx);
            if self.catalog.indexes_for(ca, n.as_slice()).next().is_some() {
                return Err(Error::Unsupported);
            }
        }
        let col_of = |n: &[u8]| -> Result<Option<usize>, S::Error> {
            if let Some(i) = tab.cols.iter().position(|c| eq_ci(c.name.as_slice(), n)) {
                return Ok(Some(i));
            }
            if eq_ci(n, b"rowid") || eq_ci(n, b"oid") || eq_ci(n, b"_rowid_") {
                return Ok(None);
            }
            Err(rt_err(SchemaKind::UnknownColumn))
        };
        let (conflict, kind) = match ast {
            Ast::Insert {
                columns,
                rows,
                select,
                conflict,
                upsert,
                returning,
                ..
            } => {
                if upsert.is_some() || !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                let mut targets = AVec::new();
                match columns {
                    Some(cs) => {
                        for c in cs.iter() {
                            targets.push(col_of(handle_bytes(arena, *c))?);
                        }
                    }
                    None => {
                        for i in 0..tab.visible() {
                            targets.push(Some(i));
                        }
                    }
                }
                let mut rr = AVec::new();
                let cur = ScopeGroup::new(AVec::new(), AVec::new(), 0);
                let mut planner = self.sub_planner(arena);
                for row in rows.iter() {
                    if row.len() != targets.len() && !(row.is_empty() && columns.is_none()) {
                        return Err(rt_err(SchemaKind::ColumnCountMismatch));
                    }
                    let mut out = AVec::new();
                    for e in row.iter() {
                        let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                        if has_agg(&r) {
                            return Err(rt_err(SchemaKind::MisusedAggregate));
                        }
                        out.push(r);
                    }
                    rr.push(out);
                }
                let sel = match select {
                    Some(s) => {
                        let p = self.plan_subquery(s, arena, &[])?;
                        if p.result_names.len() != targets.len() {
                            return Err(rt_err(SchemaKind::ColumnCountMismatch));
                        }
                        Some(Box::new(p))
                    }
                    None => None,
                };
                (
                    *conflict,
                    RtKind::Insert {
                        targets,
                        rows: rr,
                        select: sel,
                    },
                )
            }
            Ast::Update {
                alias,
                conflict,
                assignments,
                from,
                filter,
                returning,
                ..
            } => {
                if !from.is_empty() || !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                let mut sets = AVec::new();
                let mut extra = AVec::new();
                extra.push(rowid_expr(arena)?);
                for (c, e) in assignments.iter() {
                    sets.push(col_of(handle_bytes(arena, *c))?);
                    extra.push(e.clone());
                }
                let sel = self.change_select(arena, ast, *alias, filter.clone(), extra)?;
                (
                    *conflict,
                    RtKind::Change {
                        select: Box::new(sel),
                        sets: Some(sets),
                    },
                )
            }
            Ast::Delete {
                filter, returning, ..
            } => {
                if !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                let mut extra = AVec::new();
                extra.push(rowid_expr(arena)?);
                let sel = self.change_select(arena, ast, None, filter.clone(), extra)?;
                (
                    ConflictAction::Abort,
                    RtKind::Change {
                        select: Box::new(sel),
                        sets: None,
                    },
                )
            }
            _ => return Err(Error::Unsupported),
        };
        let conflict = if conflict == ConflictAction::Unset {
            ConflictAction::Abort
        } else {
            conflict
        };
        Ok(Plan::Vtab(Box::new(VtPlan::Write(Box::new(RtWrite {
            conflict,
            name: copy_vec(name),
            tab,
            kind,
        })))))
    }

    /// `SELECT *, <extra> FROM <target> WHERE <filter>`, planned.
    fn change_select(
        &self,
        arena: &Arena,
        ast: &Ast,
        alias: Option<crate::arena::ABytes>,
        filter: Option<Expr>,
        extra: AVec<Expr>,
    ) -> Result<crate::query::SubPlan, S::Error> {
        let (Ast::Update { table, .. } | Ast::Delete { table, .. }) = ast else {
            return Err(Error::Unsupported);
        };
        let sel = crate::sql::parser::star_select(*table, alias, filter, extra, arena);
        self.plan_subquery(&sel, arena, &[])
    }
}

/// The expression `rowid`.
fn rowid_expr<E>(arena: &Arena) -> Result<Expr, E> {
    Ok(Expr::Column {
        table: None,
        name: arena.alloc_copy_handle(b"rowid")?,
        #[cfg(feature = "v16")]
        schema: None,
    })
}

impl<S: Storage> Statement<'_, S> {
    /// The shadow tables of R*Tree `name` (`tab` declared): their
    /// roots, and the node size node 1's blob gives.
    async fn rt_shadow(&mut self, name: &[u8], tab: &VTab) -> Result<Shadow, S::Error> {
        let ca = &self.conn.catalog_arena;
        let root = |sfx: &[u8]| -> Result<u32, S::Error> {
            let mut n = copy_vec(name);
            n.extend_from_slice(sfx);
            self.conn
                .catalog
                .find(ca, n.as_slice())
                .map(|t| t.root)
                .ok_or(corrupt())
        };
        let (node, parent, rowid) = (root(b"_node")?, root(b"_parent")?, root(b"_rowid")?);
        let int = matches!(tab.module, Module::Rtree { int: true });
        let size = match btree::seek_rowid(&mut self.conn.pager, node, 1).await? {
            Some(p) => match field::<S::Error>(p.as_slice(), 1)? {
                OwnedValue::Blob(b) => b.len(),
                _ => 0,
            },
            None => 0,
        };
        // The pin's "undersize RTree blobs".
        if size < 512 - 64 {
            return Err(corrupt());
        }
        Ok(Shadow {
            node,
            parent,
            rowid,
            g: Geom {
                ndim2: usize::from(tab.ndim2),
                int,
                node_size: size,
            },
            naux: usize::from(tab.naux),
        })
    }

    /// xCreate: the shadow tables (`%_rowid`, `%_node`, `%_parent`,
    /// with the pin's text) and node 1, empty, of the node size the
    /// page size gives.
    pub(super) async fn rtree_create(&mut self, name: &[u8], tab: &VTab) -> Result<(), S::Error> {
        let mut quoted = AVec::new();
        quoted.push(b'"');
        for b in name.iter() {
            if *b == b'"' {
                quoted.push(b'"');
            }
            quoted.push(*b);
        }
        let mut node_root = 0;
        for (sfx, cols) in [
            (
                b"_rowid" as &[u8],
                b"(rowid INTEGER PRIMARY KEY,nodeno" as &[u8],
            ),
            (b"_node", b"(nodeno INTEGER PRIMARY KEY,data)"),
            (b"_parent", b"(nodeno INTEGER PRIMARY KEY,parentnode)"),
        ] {
            let mut tname = copy_vec(name);
            tname.extend_from_slice(sfx);
            let mut sql = copy_vec(b"CREATE TABLE ");
            sql.extend_from_slice(quoted.as_slice());
            sql.extend_from_slice(sfx);
            sql.push(b'"');
            sql.extend_from_slice(cols);
            if sfx == b"_rowid" {
                for i in 0..tab.naux {
                    sql.extend_from_slice(b",a");
                    crate::record::render_int(i64::from(i), &mut sql);
                }
                sql.push(b')');
            }
            let root = alloc_root!(&mut self.conn.pager);
            btree::create_empty_root(&mut self.conn.pager, root).await?;
            self.insert_schema_row(
                b"table",
                tname.as_slice(),
                tname.as_slice(),
                root,
                Some(sql.as_slice()),
            )
            .await?;
            if sfx == b"_node" {
                node_root = root;
            }
        }
        let size = Geom::create_size(usize::from(tab.ndim2), self.conn.pager.page_size());
        let mut blob = AVec::new();
        for _ in 0..size {
            blob.push(0u8);
        }
        let mut rec = AVec::new();
        encode_record(&[Value::Null, Value::Blob(blob.as_slice())], &mut rec);
        btree::btree_insert(&mut self.conn.pager, node_root, 1, rec.as_slice()).await?;
        // The pin's xCreate writes the root with an INSERT, which
        // counts in `total_changes()` and leaves `last_insert_rowid()`
        // at 1.
        self.conn.env.add_total(1);
        self.conn.env.last_rowid.set(1);
        Ok(())
    }

    /// Read the shadow row a write needs into `st`.
    async fn rt_load(&mut self, sh: &Shadow, st: &mut Store, m: Miss) -> Result<(), S::Error> {
        let pager = &mut self.conn.pager;
        match m {
            Miss::Node(k) => {
                let v = match btree::seek_rowid(pager, sh.node, k).await? {
                    Some(p) => match field::<S::Error>(p.as_slice(), 1)? {
                        OwnedValue::Blob(b) => Some(b),
                        OwnedValue::Text(b) => Some(b),
                        _ => Some(AVec::new()),
                    },
                    None => None,
                };
                st.add_node(k, v);
            }
            Miss::Parent(k) | Miss::Rowid(k) => {
                let root = if matches!(m, Miss::Parent(_)) {
                    sh.parent
                } else {
                    sh.rowid
                };
                let v = match btree::seek_rowid(pager, root, k).await? {
                    Some(p) => Some(crate::func::int64_value(&field::<S::Error>(
                        p.as_slice(),
                        1,
                    )?)),
                    None => None,
                };
                if matches!(m, Miss::Parent(_)) {
                    st.add_parent(k, v);
                } else {
                    st.add_rowid(k, v);
                }
            }
            Miss::NodeMax => st.node_max = Some(btree::seek_max_rowid(pager, sh.node).await?),
            Miss::RowidMax => st.rowid_max = Some(btree::seek_max_rowid(pager, sh.rowid).await?),
            Miss::NodeKeys | Miss::RowidKeys => {
                let root = if m == Miss::NodeKeys {
                    sh.node
                } else {
                    sh.rowid
                };
                let mut keys = AVec::new();
                let mut cur = ScanCursor::new(root);
                let mut buf = AVec::new();
                while let Some(k) = cur.next_row_into(pager, &mut buf).await? {
                    keys.push(k);
                }
                if m == Miss::NodeKeys {
                    st.node_keys = Some(keys);
                } else {
                    st.rowid_keys = Some(keys);
                }
            }
        }
        Ok(())
    }

    /// Replace (or insert) row `k` of the table rooted at `root`.
    async fn rt_put(&mut self, root: u32, k: i64, vals: &[Value<'_>]) -> Result<(), S::Error> {
        let mut rec = AVec::new();
        encode_record(vals, &mut rec);
        btree::btree_delete(&mut self.conn.pager, root, k).await?;
        btree::btree_insert(&mut self.conn.pager, root, k, rec.as_slice()).await
    }

    /// One `xUpdate`: delete `old`, insert `new` (REPLACE deleting a
    /// row that holds its rowid when `replace`), the tree's changes
    /// written to the shadow tables. Returns the inserted rowid and
    /// the key of the last shadow row written.
    async fn rt_update(
        &mut self,
        sh: &Shadow,
        st: &mut Store,
        old: Option<i64>,
        new: Option<&NewRow>,
        replace: bool,
    ) -> Result<(Option<i64>, Option<i64>), S::Error> {
        let mut guard = 0usize;
        let (writes, rowid) = loop {
            let mut tree = Tree::new(sh.g, st);
            match tree.update(old, new, replace) {
                Ok(r) => break (tree.w, r),
                Err(Fail::Miss(m)) => {
                    drop(tree);
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err(corrupt());
                    }
                    self.rt_load(sh, st, m).await?;
                }
                Err(Fail::Corrupt) => return Err(corrupt()),
                Err(Fail::Unique) => return Err(rt_err(SchemaKind::UniqueViolation)),
            }
        };
        for (k, v) in writes.nodes.iter() {
            match v {
                Some(b) => {
                    self.rt_put(sh.node, *k, &[Value::Null, Value::Blob(b.as_slice())])
                        .await?
                }
                None => {
                    btree::btree_delete(&mut self.conn.pager, sh.node, *k).await?;
                }
            }
        }
        for (k, v) in writes.parents.iter() {
            match v {
                Some(p) => {
                    self.rt_put(sh.parent, *k, &[Value::Null, Value::Integer(*p)])
                        .await?
                }
                None => {
                    btree::btree_delete(&mut self.conn.pager, sh.parent, *k).await?;
                }
            }
        }
        for (k, v) in writes.rowids.iter() {
            let Some(nodeno) = v else {
                btree::btree_delete(&mut self.conn.pager, sh.rowid, *k).await?;
                continue;
            };
            // A moved entry keeps its auxiliary values (the pin's
            // upsert); the inserted row takes its own.
            let mut aux: AVec<OwnedValue> = AVec::new();
            match writes.aux.as_ref() {
                Some((r, a)) if r == k => aux = a.clone(),
                _ if sh.naux > 0 => {
                    if let Some(p) = btree::seek_rowid(&mut self.conn.pager, sh.rowid, *k).await? {
                        let vals =
                            crate::record::decode_record::<S::Error>(p.as_slice(), 2 + sh.naux)?;
                        for v in vals.iter().skip(2) {
                            aux.push(OwnedValue::from_value(*v));
                        }
                    }
                }
                _ => {}
            }
            let mut vals = AVec::new();
            vals.push(Value::Null);
            vals.push(Value::Integer(*nodeno));
            for i in 0..sh.naux {
                vals.push(aux.as_slice().get(i).map_or(Value::Null, |v| v.as_value()));
            }
            self.rt_put(sh.rowid, *k, vals.as_slice()).await?;
        }
        st.absorb(&writes);
        // The shadow rows' own changes count in `total_changes()`
        // only (the pin's internal statements).
        self.conn.env.add_total(writes.count);
        Ok((rowid, writes.last_insert))
    }

    /// Run a planned R*Tree write: each row's `xUpdate` in order.
    pub(super) async fn exec_rtree_write(&mut self) -> Result<(), S::Error> {
        let Plan::Vtab(p) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let VtPlan::Write(w) = &**p else {
            return Err(Error::Unsupported);
        };
        let (name, tab, conflict, kind) =
            (w.name.clone(), w.tab.clone(), w.conflict, w.kind.clone());
        let is_insert = matches!(kind, RtKind::Insert { .. });
        let ncols = tab.cols.len();
        let ndim2 = usize::from(tab.ndim2);
        // (old rowid, new row's columns): the rows to write,
        // materialised first, as the pin collects them.
        let mut work: AVec<(Option<i64>, Option<AVec<OwnedValue>>)> = AVec::new();
        let empty = RowData::empty();
        match kind {
            RtKind::Insert {
                targets,
                rows,
                select,
            } => {
                let mut vrows: AVec<AVec<OwnedValue>> = AVec::new();
                if let Some(sel) = select {
                    vrows = self.exec_subplan(&sel, &empty).await?;
                } else {
                    for r in rows.iter() {
                        let mut out = AVec::new();
                        for e in r.iter() {
                            out.push(self.eval_row_sub(e, &empty).await?);
                        }
                        vrows.push(out);
                    }
                }
                for r in vrows.iter() {
                    let mut cols = AVec::new();
                    for _ in 0..ncols {
                        cols.push(OwnedValue::Null);
                    }
                    for (t, v) in targets.iter().zip(r.iter()) {
                        if let Some(c) = t
                            && let Some(slot) = cols.as_mut_slice().get_mut(*c)
                        {
                            *slot = v.clone();
                        }
                    }
                    work.push((None, Some(cols)));
                }
            }
            RtKind::Change { select, sets } => {
                let rows = self.exec_subplan(&select, &empty).await?;
                for r in rows.iter() {
                    let old = r.as_slice().get(ncols).map(crate::func::int64_value);
                    let new = match &sets {
                        None => None,
                        Some(sets) => {
                            let mut cols = AVec::new();
                            for v in r.iter().take(ncols) {
                                cols.push(v.clone());
                            }
                            for (k, t) in sets.iter().enumerate() {
                                if let Some(c) = t
                                    && let Some(v) = r.as_slice().get(ncols + 1 + k)
                                    && let Some(slot) = cols.as_mut_slice().get_mut(*c)
                                {
                                    *slot = v.clone();
                                }
                            }
                            Some(cols)
                        }
                    };
                    work.push((old, new));
                }
                // A DELETE collects its rowids in a RowSet on the pin:
                // the rows go in rowid order, each once.
                if sets.is_none() {
                    crate::arena::sort_by(work.as_mut_slice(), &mut |a, b| a.0.cmp(&b.0));
                    let mut kept: AVec<(Option<i64>, Option<AVec<OwnedValue>>)> = AVec::new();
                    for w in work.iter() {
                        if kept.as_slice().last().is_none_or(|k| k.0 != w.0) {
                            kept.push(w.clone());
                        }
                    }
                    work = kept;
                }
            }
        }
        let sh = self.rt_shadow(name.as_slice(), &tab).await?;
        let mut st = Store::default();
        let mut result = Ok(());
        for (old, new) in work.iter() {
            let row = match new {
                None => None,
                Some(cols) => {
                    let c = match crate::rtree::coords(
                        &sh.g,
                        cols.as_slice().get(1..1 + ndim2).unwrap_or(&[]),
                    ) {
                        Ok(c) => c,
                        Err(_) if conflict == ConflictAction::Ignore => continue,
                        Err(_) => {
                            result = Err(rt_err(SchemaKind::RtreeConstraint));
                            break;
                        }
                    };
                    let id = cols.as_slice().first().cloned().unwrap_or(OwnedValue::Null);
                    let mut aux = AVec::new();
                    for v in cols.iter().skip(1 + ndim2) {
                        aux.push(v.clone());
                    }
                    Some(NewRow {
                        rowid: (!matches!(id, OwnedValue::Null))
                            .then(|| crate::func::int64_value(&id)),
                        c,
                        aux,
                    })
                }
            };
            let r = self
                .rt_update(
                    &sh,
                    &mut st,
                    *old,
                    row.as_ref(),
                    conflict == ConflictAction::Replace,
                )
                .await;
            match r {
                Ok((Some(rowid), _)) if is_insert => self.conn.note_insert(0, rowid),
                Ok((_, last)) => {
                    self.conn.env.note_change();
                    if let Some(l) = last {
                        self.conn.env.last_rowid.set(l);
                    }
                }
                Err(Error::Schema {
                    kind: SchemaKind::UniqueViolation,
                }) if conflict == ConflictAction::Ignore => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        result?;
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        self.conn.env.finish(true);
        Ok(())
    }

    /// `rtreeFilter` over R*Tree source `vt`: a rowid lookup
    /// (`idxNum` 1) or the walk `idxStr` constrains, as rows of the
    /// table's columns (at most `need`).
    pub(super) async fn rtree_rows(
        &mut self,
        vt: &VtSrc,
        argv: &[OwnedValue],
        need: Option<usize>,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let mut c = self.rtree_open(vt, argv).await?;
        let mut out = AVec::new();
        while need.is_none_or(|n| out.len() < n) {
            match self.rtree_next(&mut c).await? {
                Some(r) => out.push(r),
                None => break,
            }
        }
        Ok(out)
    }

    /// Open a cursor on R*Tree `vt` (v0.17; the pin's xFilter): a
    /// rowid lookup finds its one cell now, a search starts at the
    /// root and walks the nodes only as `rtree_next` asks for rows.
    pub(super) async fn rtree_open(
        &mut self,
        vt: &VtSrc,
        argv: &[OwnedValue],
    ) -> Result<RtCursor, S::Error> {
        let sh = self.rt_shadow(vt.name.as_slice(), &vt.tab).await?;
        if vt.idx_num != 1 {
            return Ok(RtCursor {
                sh,
                search: Some(Search::new(vt.idx_str.as_slice(), argv)),
                one: None,
                img: None,
            });
        }
        let mut one = None;
        if let Some(r) = argv.first().and_then(crate::rtree::lookup_rowid)
            && let Some(p) = btree::seek_rowid(&mut self.conn.pager, sh.rowid, r).await?
        {
            let leaf = crate::func::int64_value(&field::<S::Error>(p.as_slice(), 1)?);
            let img = self.rt_node(&sh, leaf).await?.ok_or(corrupt())?;
            one = Some(crate::rtree::leaf_cell(&sh.g, img.as_slice(), r).ok_or(corrupt())?);
        }
        Ok(RtCursor {
            sh,
            search: None,
            one,
            img: None,
        })
    }

    /// The cursor's next row (rowid, coordinates, auxiliary columns)
    /// and its rowid; None at the end.
    #[allow(clippy::type_complexity)]
    pub(super) async fn rtree_next(
        &mut self,
        c: &mut RtCursor,
    ) -> Result<Option<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let cell = match c.search.as_mut() {
            Some(s) => loop {
                match s.step(&c.sh.g, c.img.take()) {
                    Found::Need(no) => c.img = self.rt_node(&c.sh, no).await?,
                    Found::Row(x) => break x,
                    Found::Done => return Ok(None),
                    Found::Corrupt => return Err(corrupt()),
                }
            },
            None => match c.one.take() {
                Some(x) => x,
                None => return Ok(None),
            },
        };
        let sh = &c.sh;
        let mut row = AVec::new();
        row.push(OwnedValue::Integer(cell.rowid));
        for k in 0..sh.g.ndim2 {
            row.push(crate::rtree::coord_value(
                &sh.g,
                cell.c.get(k).copied().unwrap_or(0),
            ));
        }
        if sh.naux > 0 {
            let p = btree::seek_rowid(&mut self.conn.pager, sh.rowid, cell.rowid).await?;
            let vals = match &p {
                Some(p) => crate::record::decode_record::<S::Error>(p.as_slice(), 2 + sh.naux)?,
                None => AVec::new(),
            };
            for i in 0..sh.naux {
                row.push(
                    vals.as_slice()
                        .get(2 + i)
                        .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)),
                );
            }
        }
        Ok(Some((row, Some(cell.rowid))))
    }

    /// Node `no`'s blob (`None`: no such row).
    async fn rt_node(&mut self, sh: &Shadow, no: i64) -> Result<Option<AVec<u8>>, S::Error> {
        Ok(
            match btree::seek_rowid(&mut self.conn.pager, sh.node, no).await? {
                Some(p) => match field::<S::Error>(p.as_slice(), 1)? {
                    OwnedValue::Blob(b) | OwnedValue::Text(b) => Some(b),
                    _ => Some(AVec::new()),
                },
                None => None,
            },
        )
    }

    /// `rtreecheck([schema,] table)`'s one row: "ok", or the report
    /// lines of `rtreeCheckTable`.
    pub(super) async fn rtreecheck_rows(
        &mut self,
        argv: &[OwnedValue],
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let text = |v: Option<&OwnedValue>| -> AVec<u8> {
            match v {
                Some(OwnedValue::Text(t)) | Some(OwnedValue::Blob(t)) => t.clone(),
                Some(OwnedValue::Integer(i)) => {
                    let mut o = AVec::new();
                    crate::record::render_int(*i, &mut o);
                    o
                }
                _ => AVec::new(),
            }
        };
        let (db, table) = if argv.len() >= 2 {
            (text(argv.first()), text(argv.get(1)))
        } else {
            (copy_vec(b"main"), text(argv.first()))
        };
        if !eq_ci(db.as_slice(), b"main") {
            return Err(rt_err(SchemaKind::UnknownTable));
        }
        let ca = &self.conn.catalog_arena;
        let tab = match self
            .conn
            .catalog
            .find_vtab(ca, table.as_slice())
            .map(|v| v.tab.clone())
        {
            Some(Ok(t)) if matches!(t.module, Module::Rtree { .. }) => t,
            found => {
                // Not an R*Tree: the pin reads its dimensions from
                // `SELECT *` (less a `%_rowid` table's auxiliary
                // columns); fewer than one is its report, anything
                // else fails reading `%_node`.
                let ncols = match found {
                    Some(Ok(t)) => Some(t.visible()),
                    Some(Err(_)) => None,
                    None => self
                        .conn
                        .catalog
                        .find_readable(ca, table.as_slice())
                        .map(|t| t.columns.len()),
                };
                let Some(ncols) = ncols else {
                    return Err(rt_err(SchemaKind::UnknownTable));
                };
                let mut rn = table.clone();
                rn.extend_from_slice(b"_rowid");
                let naux = self
                    .conn
                    .catalog
                    .find(ca, rn.as_slice())
                    .map_or(0, |t| t.columns.len() as i64 - 2);
                if (ncols as i64 - 1 - naux) / 2 >= 1 {
                    return Err(rt_err(SchemaKind::UnknownTable));
                }
                let mut row = AVec::new();
                row.push(OwnedValue::Text(copy_vec(
                    b"Schema corrupt or not an rtree",
                )));
                row.push(OwnedValue::Null);
                row.push(OwnedValue::Null);
                let mut out = AVec::new();
                out.push((row, Some(1)));
                return Ok(out);
            }
        };
        let sh = self.rt_shadow(table.as_slice(), &tab).await?;
        let mut st = Store::default();
        let mut counts = (0i64, 0i64);
        let mut buf = AVec::new();
        let mut cur = ScanCursor::new(sh.node);
        while let Some(k) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let v = match field::<S::Error>(buf.as_slice(), 1)? {
                OwnedValue::Blob(b) | OwnedValue::Text(b) => b,
                _ => AVec::new(),
            };
            st.add_node(k, Some(v));
        }
        for (root, is_parent) in [(sh.parent, true), (sh.rowid, false)] {
            let mut cur = ScanCursor::new(root);
            while let Some(k) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let v = crate::func::int64_value(&field::<S::Error>(buf.as_slice(), 1)?);
                if is_parent {
                    st.add_parent(k, Some(v));
                    counts.1 += 1;
                } else {
                    st.add_rowid(k, Some(v));
                    counts.0 += 1;
                }
            }
        }
        // The pin reads `int` from the first row's type: a table
        // with no rows checks as REAL.
        let int = sh.g.int && counts.0 > 0;
        let lines = crate::rtree::check(sh.g.ndim2 / 2, int, &st, counts);
        let mut report = AVec::new();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                report.push(b'\n');
            }
            report.extend_from_slice(l.as_slice());
        }
        if lines.is_empty() {
            report.extend_from_slice(b"ok");
        }
        let mut row = AVec::new();
        row.push(OwnedValue::Text(report));
        row.push(OwnedValue::Null);
        row.push(OwnedValue::Null);
        let mut out = AVec::new();
        out.push((row, Some(1)));
        Ok(out)
    }
}
